//! Evaluating one rule, and then every rule.
//!
//! The decision itself is [`uops_core::alert::step`] and lives elsewhere, without a
//! database or a clock. What is here is everything around it: which query to run, which
//! resources were expected, which of them are inside a maintenance window, and how the
//! answer gets written down exactly once.
//!
//! # Four things that are decisions, not details
//!
//! **A suppressed series is not evaluated at all.** SPEC: *"suppress alerts means the
//! rule does not fire at all"*. So a resource inside an open maintenance window is
//! skipped before `step` sees it, and its stored phase is left exactly as it was — a
//! window that opens over a firing alert does not resolve it, and one that closes does
//! not re-fire it. Suppressing *notifications* is the softer request and does the obvious
//! thing: the phase moves, nobody is told.
//!
//! **A series that stops producing data does not resolve.** A threshold rule that was
//! firing for a device which has now gone silent stays firing. "No data" is what an
//! absence rule is for, and treating it as recovery is how a real outage gets marked as
//! resolved at the moment it gets worse.
//!
//! **An absent resource has to be expected before it can be missing.** An absence rule
//! resolves its selector against the control plane and treats every resource that
//! produced no row as absent. That is why a rule scoped to the whole tenant is refused
//! when it is written: "everything" includes resources that have never reported once,
//! and the rule would fire for all of them on its first evaluation.
//!
//! **One failure does not stop a cycle.** A tenant whose `ClickHouse` query fails is
//! recorded and the rest are evaluated, because the alternative is that one broken rule
//! silences every other rule in the installation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use tokio::sync::Mutex;
use uops_core::alert::{Phase, step};
use uops_core::{ResourceId, Suppression, TenantId, TenantScope};
use uops_query::{Query, ResolvedResources, resolve};
use uops_store_ch::{ChStore, TelemetryStore};
use uops_store_pg::{AlertRule, Evaluated, PgCatalog, PgStore};

use crate::plan::{self, Reading, Series};

/// What one series' evaluation concluded.
#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub rule_id: uuid::Uuid,
    pub resource: ResourceId,
    pub dedup_key: String,
    pub phase: Phase,
    /// When the alert entered this phase. Carried so a notification can say the problem
    /// started eleven minutes ago rather than claiming it started when the message got
    /// through a rate limit.
    pub since: DateTime<Utc>,
    /// Whether somebody is to be told. Exactly the two entries `firing` and `resolved`,
    /// and never while notifications are suppressed.
    pub notify: bool,
    pub value: Option<f64>,
}

/// What one pass over one rule did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RuleOutcome {
    pub decisions: Vec<Decision>,
    /// Series skipped because a maintenance window covers them.
    pub suppressed: usize,
    /// True when the evaluation filled its series ceiling — see [`plan::MAX_SERIES`].
    pub truncated: bool,
}

impl RuleOutcome {
    #[must_use]
    pub fn notifications(&self) -> usize {
        self.decisions.iter().filter(|d| d.notify).count()
    }
}

/// What one pass over everything did.
#[derive(Clone, Debug, Default)]
pub struct Cycle {
    pub rules: usize,
    pub series: usize,
    pub notifications: usize,
    pub suppressed: usize,
    /// One sentence per rule that could not be evaluated. Collected rather than logged
    /// inside the loop, so the caller decides how often to say so and the engine stays
    /// testable without capturing output.
    pub failures: Vec<String>,
}

/// How long a tenant's suppression map is reused for.
///
/// Working out which resources are inside an open maintenance window costs one query for
/// the windows and one per window for what it covers. Doing that per *rule* means an
/// installation with SPEC's 1 000 rules pays a thousand times a cycle for an answer that
/// changes at the boundaries of a window somebody scheduled last week — which is the
/// difference between a suppression check and a second workload.
///
/// Ten seconds is the staleness this buys it with: a window that opens is honoured within
/// ten seconds, and one that closes lets alerting resume within ten. Both are far inside
/// the minute an evaluation interval is measured in, and the failure mode of being late
/// is the safe one — alerts stay suppressed a moment longer than the window, never a
/// moment less.
const SUPPRESSION_TTL: Duration = Duration::seconds(10);

/// The evaluator.
#[derive(Clone, Debug)]
pub struct Engine {
    pg: PgStore,
    ch: ChStore,
    /// Per tenant: when it was read, and what it said. Shared across clones so the run
    /// loop's spawned evaluations reuse one another's work rather than each doing it.
    suppression: SuppressionCache,
}

/// Which resources are inside an open maintenance window, and what it suppresses.
pub type Suppressions = HashMap<ResourceId, Suppression>;

/// One tenant's [`Suppressions`] and when they were read, shared between evaluations.
type SuppressionCache = Arc<Mutex<HashMap<TenantId, (DateTime<Utc>, Arc<Suppressions>)>>>;

impl Engine {
    #[must_use]
    pub fn new(pg: PgStore, ch: ChStore) -> Self {
        Self {
            pg,
            ch,
            suppression: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Evaluate one rule and write down what it decided.
    ///
    /// # Errors
    ///
    /// When the query cannot be resolved or run. A rule whose evaluation fails keeps the
    /// phase it had: the engine does not know whether the condition holds, and inventing
    /// either answer is worse than saying nothing.
    pub async fn evaluate(
        &self,
        scope: &TenantScope,
        rule: &AlertRule,
        now: DateTime<Utc>,
    ) -> uops_core::Result<RuleOutcome> {
        let query = plan::evaluation_query(&rule.query, rule.condition, now);
        let resources = resolve(&query.resources, scope, &PgCatalog::new(self.pg.clone()))
            .await
            .map_err(|e| uops_core::Error::Storage(e.to_string()))?;

        let reading = self.read(scope, &query, rule, &resources, now).await?;

        let known: HashMap<String, (Phase, DateTime<Utc>)> = self
            .pg
            .rule_state(scope, rule.id)
            .await?
            .into_iter()
            .map(|row| (row.dedup_key, (row.phase, row.since)))
            .collect();
        let suppression = self.suppressions(scope, now).await?;

        let mut outcome = RuleOutcome {
            truncated: reading.truncated,
            ..RuleOutcome::default()
        };

        for series in reading.series {
            let key = uops_core::alert::dedup_key(
                rule.id,
                series.resource,
                series.labels.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            );

            let quiet = suppression.get(&series.resource).copied();
            if quiet.is_some_and(|s| s.alerts) {
                // Not evaluated at all, and the stored phase is left alone. A window that
                // opens over a firing alert does not resolve it.
                outcome.suppressed += 1;
                continue;
            }

            let was = known.get(&key).copied();
            let breaching = rule.condition.breached_by(series.value);
            let transition = step(was, breaching, rule.condition.hold(), now);

            // Write when something is wrong, or when something *was* wrong and has just
            // stopped being — and never otherwise.
            //
            // The `was.is_some()` this used to say looks equivalent and is not: a series
            // that recovered keeps an `ok` row, so every series that has ever alerted
            // would be rewritten on every cycle, for ever. On an estate where a few
            // hundred resources have alerted at some point in the past year, that is a
            // few hundred pointless writes a minute whose only symptom is a `PostgreSQL`
            // instance that is busier than anybody can explain.
            let recovering = was.is_some_and(|(phase, _)| phase.is_active());
            if transition.phase.is_active() || recovering {
                self.pg
                    .record_evaluation(
                        scope,
                        &Evaluated {
                            rule_id: rule.id,
                            resource_id: series.resource,
                            dedup_key: key.clone(),
                            phase: transition.phase,
                            since: transition.since,
                            at: now,
                            value: Some(series.value),
                        },
                    )
                    .await?;
            }

            outcome.decisions.push(Decision {
                rule_id: rule.id,
                resource: series.resource,
                dedup_key: key,
                phase: transition.phase,
                since: transition.since,
                // The softer half of a maintenance window: the phase moves, the history
                // is kept, nobody is woken up.
                notify: transition.notify && !quiet.is_some_and(|s| s.notifications),
                value: Some(series.value),
            });
        }

        Ok(outcome)
    }

    /// Run the evaluation query, and for an absence rule add the resources that produced
    /// nothing at all.
    async fn read(
        &self,
        scope: &TenantScope,
        query: &Query,
        rule: &AlertRule,
        resources: &ResolvedResources,
        now: DateTime<Utc>,
    ) -> uops_core::Result<Reading> {
        let result = self
            .ch
            .query(query, scope, resources)
            .await
            .map_err(|e| uops_core::Error::Storage(e.to_string()))?;

        Ok(match rule.condition {
            uops_core::alert::Condition::Threshold { .. } => plan::read_threshold(&result),
            uops_core::alert::Condition::Absence { after_seconds } => {
                let mut reading = plan::read_absence(&result, now);

                // The whole point of an absence rule: the resources that are *not* in the
                // result are the ones nothing has arrived from. They cannot come out of a
                // result set, so they come out of the selector.
                let seen: HashSet<ResourceId> = reading.series.iter().map(|s| s.resource).collect();
                let expected = resources.ids().unwrap_or(&[]);

                for resource in expected.iter().filter(|r| !seen.contains(r)) {
                    reading.series.push(Series {
                        resource: *resource,
                        labels: Vec::new(),
                        // Nothing in the whole window, so the age is at least the window.
                        // Reported as the window rather than as infinity because it is
                        // the true lower bound and it is what the UI shows.
                        value: f64::from(after_seconds).max(plan::MIN_WINDOW_SECONDS) + 1.0,
                    });
                }
                reading
            }
        })
    }

    /// Which of this tenant's resources are inside an open maintenance window.
    ///
    /// Cached for [`SUPPRESSION_TTL`] and shared between evaluations: read once per rule,
    /// this would be two-plus queries a rule a cycle for an answer that changes at the
    /// edges of a window scheduled last week.
    async fn suppressions(
        &self,
        scope: &TenantScope,
        now: DateTime<Utc>,
    ) -> uops_core::Result<Arc<Suppressions>> {
        {
            let cache = self.suppression.lock().await;
            if let Some((read_at, map)) = cache.get(&scope.tenant_id())
                && now - *read_at < SUPPRESSION_TTL
                && now >= *read_at
            {
                return Ok(Arc::clone(map));
            }
        }

        let fresh = Arc::new(self.read_suppressions(scope, now).await?);
        self.suppression
            .lock()
            .await
            .insert(scope.tenant_id(), (now, Arc::clone(&fresh)));
        Ok(fresh)
    }

    async fn read_suppressions(
        &self,
        scope: &TenantScope,
        now: DateTime<Utc>,
    ) -> uops_core::Result<Suppressions> {
        let mut covered: Suppressions = HashMap::new();

        for window in self.pg.live_windows(scope, now).await? {
            if !window.schedule.is_open_at(now) {
                continue;
            }
            for resource in self.pg.covered_by(scope, window.target).await? {
                // Two windows over one resource combine to the stronger request. An
                // operator who scheduled work on a site and on one device in it means
                // both, not the second one.
                covered
                    .entry(resource)
                    .and_modify(|s| {
                        s.alerts |= window.suppression.alerts;
                        s.notifications |= window.suppression.notifications;
                    })
                    .or_insert(window.suppression);
            }
        }

        Ok(covered)
    }

    /// Evaluate every enabled rule in one tenant.
    pub async fn evaluate_tenant(&self, tenant: TenantId, now: DateTime<Utc>) -> Cycle {
        let scope = TenantScope::collector(tenant);
        let mut cycle = Cycle::default();

        let rules = match self.pg.alert_rules(&scope).await {
            Ok(rules) => rules,
            Err(e) => {
                cycle.failures.push(format!("tenant {tenant}: {e}"));
                return cycle;
            }
        };

        for rule in rules.iter().filter(|r| r.enabled) {
            cycle.rules += 1;
            match self.evaluate(&scope, rule, now).await {
                Ok(outcome) => {
                    cycle.series += outcome.decisions.len();
                    cycle.notifications += outcome.notifications();
                    cycle.suppressed += outcome.suppressed;
                    if outcome.truncated {
                        cycle.failures.push(format!(
                            "rule {} read the maximum of {} series and stopped; its grouping \
                             produces more alerts than one rule can hold",
                            rule.name,
                            plan::MAX_SERIES
                        ));
                    }
                }
                // One rule's failure is not a reason to stop evaluating the others: the
                // alternative is that a single broken rule silences the installation.
                Err(e) => cycle.failures.push(format!("rule {}: {e}", rule.name)),
            }
        }

        cycle
    }

    /// Evaluate every enabled rule in every tenant.
    ///
    /// Crosses tenants by iterating, the way the poller's fleet loader does: `TenantScope`
    /// has no "all tenants" constructor on purpose, so the crossing is a `for` loop
    /// somebody can see rather than a `WHERE` clause somebody eventually copies.
    ///
    /// # Errors
    ///
    /// Only when the tenant list itself cannot be read.
    pub async fn cycle(&self, now: DateTime<Utc>) -> uops_core::Result<Cycle> {
        let mut total = Cycle::default();

        for tenant in self.pg.all_tenant_ids().await? {
            let cycle = self.evaluate_tenant(tenant, now).await;
            total.rules += cycle.rules;
            total.series += cycle.series;
            total.notifications += cycle.notifications;
            total.suppressed += cycle.suppressed;
            total.failures.extend(cycle.failures);
        }

        Ok(total)
    }
}
