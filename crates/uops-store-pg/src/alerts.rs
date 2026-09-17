//! Alert rules and the state the engine keeps — SPEC §M4.
//!
//! A rule is a `Query` plus a [`Condition`], which is why so little is here: the query
//! half is the same object the Explorer posts and a saved search stores, and the
//! condition half is a small value type in `uops-core` with a state machine of its own.
//! This module is the part that has to be durable.
//!
//! # Why `record` is one statement
//!
//! The engine writes a state row on every evaluation of every series. That write has to
//! be atomic against another evaluator doing the same thing — a restart overlapping its
//! predecessor, or two replicas — or one problem becomes two alerts, and the older one
//! never resolves because nothing evaluates it any more. So it is a single
//! `INSERT … ON CONFLICT (tenant_id, dedup_key) DO UPDATE`, and the deduplication is the
//! database's rather than a read-then-write this code would have to hold a lock around.
//!
//! # What is deliberately not here
//!
//! The decision. Whether a series is breaching belongs to the evaluator, and what that
//! means for its phase belongs to [`uops_core::alert::step`] — which has no database and
//! no clock, and is where the flapping tests live.

use chrono::{DateTime, Utc};
use uops_core::alert::{AlertSeverity, Condition, Phase};
use uops_core::{ActorId, Error as CoreError, ResourceId, Result, TenantScope};
use uops_query::{Query, ResolvedResources, compile};

use crate::error::map;
use crate::store::PgStore;

/// A rule as stored.
#[derive(Clone, Debug)]
pub struct AlertRule {
    pub id: uuid::Uuid,
    pub tenant_id: uops_core::TenantId,
    pub name: String,
    pub description: String,
    /// What to read. The window is provenance — the evaluator substitutes its own.
    pub query: Query,
    pub condition: Condition,
    pub severity: AlertSeverity,
    pub enabled: bool,
    pub eval_interval: chrono::Duration,
    pub notify: serde_json::Value,
    pub created_by: Option<ActorId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What a caller supplies to create or replace a rule.
#[derive(Clone, Debug)]
pub struct NewRule {
    pub name: String,
    pub description: String,
    pub query: Query,
    pub condition: Condition,
    pub severity: AlertSeverity,
    pub enabled: bool,
    pub eval_interval: chrono::Duration,
    pub notify: serde_json::Value,
}

/// One series' current state.
#[derive(Clone, Debug)]
pub struct AlertStateRow {
    pub id: uuid::Uuid,
    pub rule_id: uuid::Uuid,
    pub resource_id: ResourceId,
    pub dedup_key: String,
    pub phase: Phase,
    pub since: DateTime<Utc>,
    pub last_eval: DateTime<Utc>,
    pub last_value: Option<f64>,
    pub acked_by: Option<ActorId>,
    pub acked_at: Option<DateTime<Utc>>,
}

/// One active alert, with the two names a person needs to read it.
///
/// The join is here rather than in the browser because the alternative is an N+1: a
/// screen showing forty alerts would make forty requests for forty resource names, and
/// the names are one indexed lookup each in a query that is already running.
#[derive(Clone, Debug)]
pub struct ActiveAlert {
    pub alert: AlertStateRow,
    pub rule: String,
    pub severity: AlertSeverity,
    /// What a person calls the device. A resource deleted between the evaluation and the
    /// read has none, and the id is better than an empty cell.
    pub resource: String,
}

/// What the engine writes after deciding one series' phase.
#[derive(Clone, Debug)]
pub struct Evaluated {
    pub rule_id: uuid::Uuid,
    pub resource_id: ResourceId,
    pub dedup_key: String,
    pub phase: Phase,
    pub since: DateTime<Utc>,
    pub at: DateTime<Utc>,
    pub value: Option<f64>,
}

struct RuleRow {
    id: uuid::Uuid,
    tenant_id: uops_core::TenantId,
    name: String,
    description: String,
    query: serde_json::Value,
    condition: serde_json::Value,
    severity: String,
    enabled: bool,
    eval_interval: sqlx::postgres::types::PgInterval,
    notify: serde_json::Value,
    created_by: Option<ActorId>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl RuleRow {
    fn parse(self) -> Result<AlertRule> {
        Ok(AlertRule {
            id: self.id,
            tenant_id: self.tenant_id,
            name: self.name,
            description: self.description,
            query: serde_json::from_value(self.query)?,
            condition: serde_json::from_value(self.condition)?,
            severity: severity_from(&self.severity)?,
            enabled: self.enabled,
            eval_interval: interval_to_duration(&self.eval_interval),
            notify: self.notify,
            created_by: self.created_by,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

struct StateRow {
    id: uuid::Uuid,
    rule_id: uuid::Uuid,
    resource_id: ResourceId,
    dedup_key: String,
    state: String,
    since: DateTime<Utc>,
    last_eval: DateTime<Utc>,
    last_value: Option<f64>,
    acked_by: Option<ActorId>,
    acked_at: Option<DateTime<Utc>>,
}

impl StateRow {
    fn parse(self) -> Result<AlertStateRow> {
        Ok(AlertStateRow {
            id: self.id,
            rule_id: self.rule_id,
            resource_id: self.resource_id,
            dedup_key: self.dedup_key,
            phase: phase_from(&self.state)?,
            since: self.since,
            last_eval: self.last_eval,
            last_value: self.last_value,
            acked_by: self.acked_by,
            acked_at: self.acked_at,
        })
    }
}

fn severity_from(text: &str) -> Result<AlertSeverity> {
    match text {
        "info" => Ok(AlertSeverity::Info),
        "warning" => Ok(AlertSeverity::Warning),
        "critical" => Ok(AlertSeverity::Critical),
        other => Err(CoreError::Invalid(format!(
            "unknown alert severity {other}"
        ))),
    }
}

fn phase_from(text: &str) -> Result<Phase> {
    match text {
        "ok" => Ok(Phase::Ok),
        "pending" => Ok(Phase::Pending),
        "firing" => Ok(Phase::Firing),
        "resolved" => Ok(Phase::Resolved),
        other => Err(CoreError::Invalid(format!("unknown alert state {other}"))),
    }
}

/// `interval` as a `chrono::Duration`.
///
/// Months are counted as thirty days and days as twenty-four hours, which is wrong in
/// general and exactly right here: the `CHECK` in migration 0014 bounds an evaluation
/// interval to between ten seconds and a day, so neither unit can appear in a stored
/// value. The conversion is total rather than fallible because there is nothing for a
/// caller to do about a row the schema cannot hold.
fn interval_to_duration(interval: &sqlx::postgres::types::PgInterval) -> chrono::Duration {
    chrono::Duration::microseconds(interval.microseconds)
        + chrono::Duration::days(i64::from(interval.days))
        + chrono::Duration::days(i64::from(interval.months) * 30)
}

fn duration_to_interval(d: chrono::Duration) -> sqlx::postgres::types::PgInterval {
    sqlx::postgres::types::PgInterval {
        months: 0,
        days: 0,
        microseconds: d.num_microseconds().unwrap_or(i64::MAX),
    }
}

/// Reject a rule whose query the compiler cannot answer.
///
/// The same check saving a search does, for the same reason and one more: a rule is
/// evaluated by a background task with nobody watching, so a query that fails at
/// evaluation time produces a log line every sixty seconds and an alert that never fires,
/// which is the failure mode people discover during the incident it should have caught.
fn must_compile(query: &Query, scope: &TenantScope) -> Result<()> {
    compile(query, scope, &ResolvedResources::whole_tenant(scope))
        .map(|_| ())
        .map_err(|e| CoreError::Invalid(e.to_string()))
}

/// Reject a rule whose query cannot produce the thing its condition compares.
///
/// Both of these are refusals at the moment somebody writes the rule, because the
/// alternative is a rule that saves, lists and looks healthy while evaluating to nothing
/// — and the symptom is an alert that never arrives, noticed during the incident it was
/// written for.
fn must_be_evaluable(query: &Query, condition: Condition) -> Result<()> {
    match condition {
        // A threshold compares one number. The query's own aggregate is that number —
        // `avg(system.cpu.utilization) > 90` — and several aggregates leave no way to say
        // which one the threshold is about.
        //
        // **Zero is allowed, and means the row count.** A saved search from the Log
        // Explorer has no aggregation: it is "the rows matching this". Alerting on it
        // means alerting on *how many* there are, which is exactly what somebody who
        // saved "errors mentioning CRC" means by "tell me when this happens" — and it is
        // what makes SPEC's "converts to an alert rule with no edits" literally true
        // rather than true-after-adding-a-count. The evaluator supplies the `count()`;
        // see `uops_alert::plan`.
        Condition::Threshold { .. } => {
            if query.aggregations.len() > 1 {
                return Err(CoreError::Invalid(format!(
                    "a threshold rule compares one number and this query produces {} —                      leave exactly one aggregation, or none to alert on the row count.",
                    query.aggregations.len()
                )));
            }
        }

        // An absence rule asks which of a set of resources has gone quiet, so the set has
        // to be nameable. `all` includes every resource that has never reported once — a
        // decommissioned switch, a device added this morning — and the rule would fire
        // for all of them on its first evaluation, which is the fastest way to teach an
        // operator to ignore this product.
        Condition::Absence { .. } => {
            if matches!(query.resources, uops_query::ResourceSelector::All) {
                return Err(CoreError::Invalid(
                    "an absence rule must name the resources it watches — a kind, a site, a                      group or a tag. Every resource in the tenant includes ones that have                      never reported, and the rule would fire for all of them."
                        .to_owned(),
                ));
            }
        }
    }

    Ok(())
}

impl PgStore {
    /// Create a rule.
    ///
    /// # Errors
    ///
    /// `Invalid` when the query does not compile or the tenant already has a rule by that
    /// name.
    pub async fn create_rule(
        &self,
        scope: &TenantScope,
        by: Option<ActorId>,
        new: &NewRule,
    ) -> Result<AlertRule> {
        must_compile(&new.query, scope)?;
        must_be_evaluable(&new.query, new.condition)?;

        // tenant-exempt: the tenant is the first bound parameter, from the scope.
        let row = sqlx::query_as!(
            RuleRow,
            r#"
            INSERT INTO alert_rule
                (tenant_id, name, description, kind, query, condition, severity,
                 enabled, eval_interval, notify, created_by)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            RETURNING
                id,
                tenant_id     AS "tenant_id: uops_core::TenantId",
                name, description, query, condition, severity, enabled,
                eval_interval AS "eval_interval: sqlx::postgres::types::PgInterval",
                notify,
                created_by    AS "created_by: ActorId",
                created_at, updated_at
            "#,
            scope.tenant_id() as uops_core::TenantId,
            new.name.trim(),
            new.description,
            // Never from the caller: the CHECK in migration 0014 would refuse a
            // disagreement, and taking it from the condition means there is nothing to
            // disagree with.
            new.condition.as_str(),
            serde_json::to_value(&new.query)?,
            serde_json::to_value(new.condition)?,
            new.severity.as_str(),
            new.enabled,
            duration_to_interval(new.eval_interval),
            new.notify,
            by as Option<ActorId>,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("alert_rule", new.name.clone(), e))?;

        row.parse()
    }

    /// A tenant's rules, newest first.
    pub async fn alert_rules(&self, scope: &TenantScope) -> Result<Vec<AlertRule>> {
        // tenant-exempt: the tenant is the only bound parameter, from the scope.
        let rows = sqlx::query_as!(
            RuleRow,
            r#"
            SELECT
                id,
                tenant_id     AS "tenant_id: uops_core::TenantId",
                name, description, query, condition, severity, enabled,
                eval_interval AS "eval_interval: sqlx::postgres::types::PgInterval",
                notify,
                created_by    AS "created_by: ActorId",
                created_at, updated_at
              FROM alert_rule
             WHERE tenant_id = $1
             ORDER BY created_at DESC
            "#,
            scope.tenant_id() as uops_core::TenantId,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("alert_rule", "list".to_owned(), e))?;

        rows.into_iter().map(RuleRow::parse).collect()
    }

    /// One rule.
    ///
    /// # Errors
    ///
    /// `NotFound` for another tenant's rule, which is the same answer as for one that
    /// does not exist.
    pub async fn alert_rule(&self, scope: &TenantScope, id: uuid::Uuid) -> Result<AlertRule> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query_as!(
            RuleRow,
            r#"
            SELECT
                id,
                tenant_id     AS "tenant_id: uops_core::TenantId",
                name, description, query, condition, severity, enabled,
                eval_interval AS "eval_interval: sqlx::postgres::types::PgInterval",
                notify,
                created_by    AS "created_by: ActorId",
                created_at, updated_at
              FROM alert_rule
             WHERE tenant_id = $1 AND id = $2
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("alert_rule", id.to_string(), e))?
        .ok_or(CoreError::NotFound {
            kind: "alert_rule",
            id: id.to_string(),
        })?;

        row.parse()
    }

    /// Replace a rule.
    pub async fn update_rule(
        &self,
        scope: &TenantScope,
        id: uuid::Uuid,
        new: &NewRule,
    ) -> Result<AlertRule> {
        must_compile(&new.query, scope)?;
        must_be_evaluable(&new.query, new.condition)?;

        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query_as!(
            RuleRow,
            r#"
            UPDATE alert_rule
               SET name = $3, description = $4, kind = $5, query = $6, condition = $7,
                   severity = $8, enabled = $9, eval_interval = $10, notify = $11
             WHERE tenant_id = $1 AND id = $2
            RETURNING
                id,
                tenant_id     AS "tenant_id: uops_core::TenantId",
                name, description, query, condition, severity, enabled,
                eval_interval AS "eval_interval: sqlx::postgres::types::PgInterval",
                notify,
                created_by    AS "created_by: ActorId",
                created_at, updated_at
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id,
            new.name.trim(),
            new.description,
            new.condition.as_str(),
            serde_json::to_value(&new.query)?,
            serde_json::to_value(new.condition)?,
            new.severity.as_str(),
            new.enabled,
            duration_to_interval(new.eval_interval),
            new.notify,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("alert_rule", new.name.clone(), e))?
        .ok_or(CoreError::NotFound {
            kind: "alert_rule",
            id: id.to_string(),
        })?;

        row.parse()
    }

    /// Enable or disable a rule without touching anything else.
    ///
    /// Its own verb because it is the one an operator reaches for at 3am, and making them
    /// round-trip the whole rule to flip a boolean is how a tired person overwrites a
    /// threshold by accident.
    pub async fn set_rule_enabled(
        &self,
        scope: &TenantScope,
        id: uuid::Uuid,
        enabled: bool,
    ) -> Result<AlertRule> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query_as!(
            RuleRow,
            r#"
            UPDATE alert_rule
               SET enabled = $3
             WHERE tenant_id = $1 AND id = $2
            RETURNING
                id,
                tenant_id     AS "tenant_id: uops_core::TenantId",
                name, description, query, condition, severity, enabled,
                eval_interval AS "eval_interval: sqlx::postgres::types::PgInterval",
                notify,
                created_by    AS "created_by: ActorId",
                created_at, updated_at
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id,
            enabled,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("alert_rule", id.to_string(), e))?
        .ok_or(CoreError::NotFound {
            kind: "alert_rule",
            id: id.to_string(),
        })?;

        row.parse()
    }

    /// Delete a rule, and with it everything it believed.
    pub async fn delete_rule(&self, scope: &TenantScope, id: uuid::Uuid) -> Result<()> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let done = sqlx::query!(
            "DELETE FROM alert_rule WHERE tenant_id = $1 AND id = $2",
            scope.tenant_id() as uops_core::TenantId,
            id,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("alert_rule", id.to_string(), e))?;

        if done.rows_affected() == 0 {
            return Err(CoreError::NotFound {
                kind: "alert_rule",
                id: id.to_string(),
            });
        }
        Ok(())
    }

    /// Write what an evaluation decided about one series.
    ///
    /// One statement, so two evaluators cannot turn one problem into two alerts. The
    /// conflict target is `(tenant_id, dedup_key)` — the `UNIQUE` that migration 0014
    /// exists to provide.
    ///
    /// `since` is written as given rather than as `now()`: the machine carries it forward
    /// while a phase persists, because `pending` measures from the first breach.
    pub async fn record_evaluation(
        &self,
        scope: &TenantScope,
        e: &Evaluated,
    ) -> Result<AlertStateRow> {
        // tenant-exempt: the tenant is the first bound parameter, from the scope.
        let row = sqlx::query_as!(
            StateRow,
            r#"
            INSERT INTO alert_state
                (tenant_id, rule_id, resource_id, dedup_key, state, since, last_eval,
                 last_value)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (tenant_id, dedup_key) DO UPDATE
               SET state      = EXCLUDED.state,
                   since      = EXCLUDED.since,
                   last_eval  = EXCLUDED.last_eval,
                   last_value = EXCLUDED.last_value,
                   -- An acknowledgement belongs to the episode it was made during. A
                   -- series that has gone back to ok and breached again is a new problem,
                   -- and carrying the old ack forward would silence a page nobody has
                   -- seen.
                   acked_by   = CASE WHEN alert_state.state = EXCLUDED.state
                                     THEN alert_state.acked_by ELSE NULL END,
                   acked_at   = CASE WHEN alert_state.state = EXCLUDED.state
                                     THEN alert_state.acked_at ELSE NULL END
            RETURNING
                id, rule_id,
                resource_id AS "resource_id: ResourceId",
                dedup_key, state, since, last_eval, last_value,
                acked_by    AS "acked_by: ActorId",
                acked_at
            "#,
            scope.tenant_id() as uops_core::TenantId,
            e.rule_id,
            e.resource_id as ResourceId,
            e.dedup_key,
            e.phase.as_str(),
            e.since,
            e.at,
            e.value,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|err| map("alert_state", e.dedup_key.clone(), err))?;

        row.parse()
    }

    /// Everything one rule currently believes.
    ///
    /// What an evaluation cycle reads before it decides: one query per rule rather than
    /// one per series, because a rule over five thousand resources would otherwise open a
    /// cycle with five thousand round trips.
    pub async fn rule_state(
        &self,
        scope: &TenantScope,
        rule_id: uuid::Uuid,
    ) -> Result<Vec<AlertStateRow>> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let rows = sqlx::query_as!(
            StateRow,
            r#"
            SELECT
                id, rule_id,
                resource_id AS "resource_id: ResourceId",
                dedup_key, state, since, last_eval, last_value,
                acked_by    AS "acked_by: ActorId",
                acked_at
              FROM alert_state
             WHERE tenant_id = $1 AND rule_id = $2
            "#,
            scope.tenant_id() as uops_core::TenantId,
            rule_id,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("alert_state", rule_id.to_string(), e))?;

        rows.into_iter().map(StateRow::parse).collect()
    }

    /// What is wrong in this tenant right now, most recent first.
    ///
    /// `pending` is deliberately included. Nobody has been notified about a pending alert
    /// and nobody should be — but an operator who has just been paged about one device
    /// wants to see the four that are one evaluation away from paging too.
    pub async fn active_alerts(&self, scope: &TenantScope) -> Result<Vec<ActiveAlert>> {
        // tenant-exempt: the tenant is the only bound parameter, from the scope, and both
        // joins carry it so a row cannot pick up another tenant's name.
        let rows = sqlx::query!(
            r#"
            SELECT
                s.id, s.rule_id,
                s.resource_id AS "resource_id: ResourceId",
                s.dedup_key, s.state, s.since, s.last_eval, s.last_value,
                s.acked_by    AS "acked_by: ActorId",
                s.acked_at,
                r.name        AS rule_name,
                r.severity    AS rule_severity,
                res.name      AS "resource_name?"
              FROM alert_state s
              JOIN alert_rule r
                ON r.id = s.rule_id AND r.tenant_id = s.tenant_id
              LEFT JOIN resource res
                ON res.id = s.resource_id AND res.tenant_id = s.tenant_id
             WHERE s.tenant_id = $1 AND s.state IN ('pending', 'firing')
             ORDER BY s.since DESC
            "#,
            scope.tenant_id() as uops_core::TenantId,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("alert_state", "active".to_owned(), e))?;

        rows.into_iter()
            .map(|r| {
                Ok(ActiveAlert {
                    rule: r.rule_name,
                    severity: severity_from(&r.rule_severity)?,
                    resource: r.resource_name.unwrap_or_else(|| r.resource_id.to_string()),
                    alert: AlertStateRow {
                        id: r.id,
                        rule_id: r.rule_id,
                        resource_id: r.resource_id,
                        dedup_key: r.dedup_key,
                        phase: phase_from(&r.state)?,
                        since: r.since,
                        last_eval: r.last_eval,
                        last_value: r.last_value,
                        acked_by: r.acked_by,
                        acked_at: r.acked_at,
                    },
                })
            })
            .collect()
    }

    /// Acknowledge an alert.
    ///
    /// Silences the notification, never the state: the alert stays in the list, still
    /// firing, with a name against it. An acknowledgement that hid the alert would mean
    /// the next person to look at the screen concludes the problem went away.
    ///
    /// # Errors
    ///
    /// `NotFound` for another tenant's alert or one that does not exist.
    pub async fn acknowledge_alert(
        &self,
        scope: &TenantScope,
        id: uuid::Uuid,
        by: ActorId,
        at: DateTime<Utc>,
    ) -> Result<AlertStateRow> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query_as!(
            StateRow,
            r#"
            UPDATE alert_state
               SET acked_by = $3, acked_at = $4
             WHERE tenant_id = $1 AND id = $2
            RETURNING
                id, rule_id,
                resource_id AS "resource_id: ResourceId",
                dedup_key, state, since, last_eval, last_value,
                acked_by    AS "acked_by: ActorId",
                acked_at
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id,
            by as ActorId,
            at,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("alert_state", id.to_string(), e))?
        .ok_or(CoreError::NotFound {
            kind: "alert_state",
            id: id.to_string(),
        })?;

        row.parse()
    }
}
