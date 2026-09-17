//! When each rule is evaluated.
//!
//! A [`uops_poll::Wheel`] keyed by `(tenant, rule)`. Every rule carries its own
//! `eval_interval`, and the wheel places each one's first firing anywhere inside that
//! interval from a seed derived from its id — so a thousand rules created in one
//! migration do not all evaluate at `:00`, and a restart puts each rule back roughly
//! where it was rather than reshuffling the whole installation into a new set of
//! collisions.
//!
//! # Why the rule list is reloaded rather than watched
//!
//! Rules change rarely and a `LISTEN/NOTIFY` channel is a connection to keep alive, a
//! reconnect path to get right, and a failure mode where the engine quietly evaluates
//! last week's rules. A reload every `RELOAD` seconds is a single indexed query per
//! tenant, and the cost of being at most that stale is that a rule somebody just wrote
//! starts firing up to a minute later — which is the same order as its own evaluation
//! interval.

use std::collections::HashSet;
use std::time::Duration;

use uops_core::TenantId;
use uops_poll::{Wheel, WheelError};
use uops_store_pg::PgStore;

/// The wheel's resolution, and how often the loop wakes.
///
/// One second. The shortest evaluation interval the schema allows is ten seconds, so a
/// finer tick would only cost wake-ups; a coarser one would make a ten-second rule
/// arrive late by a visible fraction of its own period.
pub const TICK: Duration = Duration::from_secs(1);

/// How long the wheel is, and therefore the longest interval it can hold.
///
/// A day and a second's resolution is 86 400 slots — a few megabytes of empty `VecDeque`s
/// and nothing else, which is the price of never having to explain why a rule set to
/// evaluate daily fired every hour instead. Migration 0014 bounds an interval to a day
/// for exactly this reason.
const SLOTS: usize = 86_400 + 1;

/// How often the rule list is re-read.
pub const RELOAD: Duration = Duration::from_secs(60);

/// A rule that is due now.
pub type Due = (TenantId, uuid::Uuid);

/// The schedule.
#[derive(Debug)]
pub struct Scheduler {
    wheel: Wheel<Due>,
    /// What is already in the wheel, so a reload adds and removes rather than rebuilding.
    /// Rebuilding would re-seed every rule's placement on every reload, which is the
    /// lockstep problem arriving by another door — once a minute, forever.
    scheduled: HashSet<Due>,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            wheel: Wheel::new(SLOTS, TICK),
            scheduled: HashSet::new(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.scheduled.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scheduled.is_empty()
    }

    /// Add a rule to the schedule, if it is not already there.
    ///
    /// # Errors
    ///
    /// When the interval is longer than the wheel — which migration 0014's `CHECK` makes
    /// unreachable from the database, and which is still an error rather than a clamp
    /// because a rule silently evaluating at the wrong period is worse than one that is
    /// reported and skipped.
    pub fn insert(
        &mut self,
        tenant: TenantId,
        rule: uuid::Uuid,
        interval: Duration,
    ) -> Result<(), WheelError> {
        let key = (tenant, rule);
        if self.scheduled.contains(&key) {
            return Ok(());
        }

        // The seed is the rule's own id, so the placement survives a restart. A random
        // seed would move every rule on every deploy and make a busy installation's load
        // pattern impossible to reason about.
        let seed = seed_of(rule);
        self.wheel.insert(key, interval, seed)?;
        self.scheduled.insert(key);
        Ok(())
    }

    /// Forget a rule that has been deleted or disabled.
    ///
    /// The wheel keeps the entry until it next comes round — removing from the middle of
    /// a wheel is O(slot) and buys nothing, because [`due`](Self::due) filters against
    /// this set on the way out.
    pub fn remove(&mut self, tenant: TenantId, rule: uuid::Uuid) {
        self.scheduled.remove(&(tenant, rule));
    }

    /// Advance one tick and return the rules that are now due.
    ///
    /// Entries for rules that have since been removed are dropped here rather than
    /// hunted down in the wheel.
    pub fn due(&mut self) -> Vec<Due> {
        let mut out = Vec::new();
        self.wheel.advance(&mut out);
        out.retain(|key| self.scheduled.contains(key));
        out
    }

    /// Bring the schedule in line with what the database holds.
    ///
    /// Enabled rules that are not scheduled are added; scheduled rules that are gone or
    /// disabled are forgotten. Everything already there keeps its place — see
    /// [`Scheduler::scheduled`].
    ///
    /// # Errors
    ///
    /// Only when the tenant list cannot be read. A tenant whose rules cannot be read is
    /// reported and the rest are still scheduled, because one tenant's broken row is not
    /// a reason to stop alerting for everybody else.
    pub async fn reload(&mut self, store: &PgStore) -> uops_core::Result<Vec<String>> {
        let mut problems = Vec::new();
        let mut live: HashSet<Due> = HashSet::new();

        for tenant in store.all_tenant_ids().await? {
            let scope = uops_core::TenantScope::collector(tenant);
            let rules = match store.alert_rules(&scope).await {
                Ok(rules) => rules,
                Err(e) => {
                    problems.push(format!("tenant {tenant}: rules could not be read: {e}"));
                    continue;
                }
            };

            for rule in rules.iter().filter(|r| r.enabled) {
                live.insert((tenant, rule.id));
                let interval = rule
                    .eval_interval
                    .to_std()
                    .unwrap_or(Duration::from_secs(60));
                if let Err(e) = self.insert(tenant, rule.id, interval) {
                    problems.push(format!("rule {}: {e}", rule.name));
                }
            }
        }

        // Anything scheduled that is no longer enabled or no longer exists.
        let stale: Vec<Due> = self
            .scheduled
            .iter()
            .filter(|key| !live.contains(key))
            .copied()
            .collect();
        for (tenant, rule) in stale {
            self.remove(tenant, rule);
        }

        Ok(problems)
    }
}

/// A rule's id as a wheel seed.
///
/// The low 64 bits of the uuid. `UUIDv7` puts the timestamp in the *high* bits, so rules
/// created in the same millisecond — a migration, a bulk import — differ here and are
/// placed apart, which is precisely the case that would otherwise land them in one slot.
fn seed_of(rule: uuid::Uuid) -> u64 {
    rule.as_u64_pair().1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(n: usize) -> Vec<uuid::Uuid> {
        (0..n).map(|_| uuid::Uuid::now_v7()).collect()
    }

    #[test]
    fn a_thousand_rules_do_not_all_evaluate_in_the_same_second() {
        // The property the wheel exists for, on the number SPEC names: 1 000 rules on a
        // 60-second interval. A task-per-rule design would have all thousand wake at the
        // same instant and open a thousand ClickHouse connections at :00.
        let tenant = TenantId::new();
        let mut scheduler = Scheduler::new();
        for rule in rules(1_000) {
            scheduler
                .insert(tenant, rule, Duration::from_secs(60))
                .expect("schedule");
        }

        // Ten intervals, so the ±10% jitter averages out instead of deciding the result.
        let mut per_second = Vec::new();
        for _ in 0..600 {
            per_second.push(scheduler.due().len());
        }

        let total: usize = per_second.iter().sum();
        assert!(
            (9_000..=11_000).contains(&total),
            "1 000 rules over ten intervals evaluated {total} times"
        );

        // SPEC's acceptance criterion is that 1 000 rules fit inside one 60-second cycle.
        // What makes that achievable is that they are spread across it: the busiest
        // second must hold a small fraction of the fleet, not all of it.
        let worst = per_second.iter().copied().max().unwrap_or(0);
        assert!(
            worst < 60,
            "the busiest second held {worst} of 1 000 rules, which is a spike rather than              a spread"
        );
        assert!(
            per_second.iter().filter(|n| **n > 0).count() > 500,
            "the load is in a few seconds rather than across the minute"
        );
    }

    #[test]
    fn every_rule_comes_round_about_once_per_interval() {
        // "About", because the ±10% jitter is the point: two rules that happen to collide
        // must not collide forever. What must hold is that no rule is starved and none is
        // evaluated at twice the rate its operator asked for.
        let tenant = TenantId::new();
        let mut scheduler = Scheduler::new();
        let ids = rules(50);
        for rule in &ids {
            scheduler
                .insert(tenant, *rule, Duration::from_secs(30))
                .expect("schedule");
        }

        // Ten intervals.
        let mut counts = std::collections::HashMap::new();
        for _ in 0..300 {
            for (_, rule) in scheduler.due() {
                *counts.entry(rule).or_insert(0) += 1;
            }
        }

        assert_eq!(counts.len(), ids.len(), "every rule came round");
        assert!(
            counts.values().all(|n| (9..=11).contains(n)),
            "a rule fired the wrong number of times in ten intervals: {counts:?}"
        );
    }

    #[test]
    fn scheduling_the_same_rule_twice_does_not_evaluate_it_twice() {
        // A reload runs every minute and sees the same rules. Re-inserting them would
        // double the evaluation rate on every reload, and the symptom would be a
        // ClickHouse bill rather than anything visible in the UI.
        let tenant = TenantId::new();
        let rule = uuid::Uuid::now_v7();
        let mut scheduler = Scheduler::new();

        for _ in 0..10 {
            scheduler
                .insert(tenant, rule, Duration::from_secs(10))
                .expect("schedule");
        }

        assert_eq!(scheduler.len(), 1);
        let fired: usize = (0..10).map(|_| scheduler.due().len()).sum();
        assert_eq!(fired, 1);
    }

    #[test]
    fn a_removed_rule_stops_being_due() {
        let tenant = TenantId::new();
        let rule = uuid::Uuid::now_v7();
        let mut scheduler = Scheduler::new();
        scheduler
            .insert(tenant, rule, Duration::from_secs(5))
            .expect("schedule");

        scheduler.remove(tenant, rule);
        assert!(scheduler.is_empty());

        let fired: usize = (0..20).map(|_| scheduler.due().len()).sum();
        assert_eq!(fired, 0, "a disabled rule must not keep evaluating");
    }

    #[test]
    fn two_rules_created_in_the_same_millisecond_are_placed_apart() {
        // UUIDv7 puts the timestamp in the high bits, so a bulk import produces ids that
        // differ only low down. Seeding from the high half would put all of them in one
        // slot — the exact lockstep the wheel exists to prevent.
        let a = uuid::Uuid::now_v7();
        let b = uuid::Uuid::now_v7();
        assert_ne!(seed_of(a), seed_of(b));
    }
}
