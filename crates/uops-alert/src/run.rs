//! The loop.
//!
//! Everything below this file has tests. This is the order those things happen in, which
//! is the part that cannot be unit-tested and the part an operator experiences:
//!
//! ```text
//!   reload    every tenant's enabled rules, into the wheel      every RELOAD
//!   tick      once a second: what the wheel says is due
//!   dispatch  per rule: evaluate, decide, write                 bounded concurrency
//!   report    what fired and what failed                        once per reload window
//! ```
//!
//! # Why a tick does not wait for its evaluations
//!
//! A rule's evaluation is a `ClickHouse` query, and one slow query must not delay every
//! other rule's schedule — that is how an engine starts evaluating a 60-second rule every
//! ninety seconds without anything appearing to be wrong. So a tick dispatches and
//! returns, and [`IN_FLIGHT`] is what bounds the damage instead: an installation whose
//! `ClickHouse` has gone slow ends up with a queue rather than with a thousand concurrent
//! statements.
//!
//! # Why failures are summarised rather than printed
//!
//! A rule whose query fails, fails every cycle. At a 60-second interval that is 1 440
//! lines a day for one rule, and an installation with fifty such rules produces a log in
//! which nothing else can be found. The first of each is printed and the rest are
//! counted, and the set is cleared on reload — so a rule that is still broken says so
//! again every minute rather than never.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::{Mutex, Semaphore};
use uops_notify::{Notification, Notifier};
use uops_store_pg::{Outcome, PgStore};

use crate::engine::Engine;
use crate::scheduler::{RELOAD, Scheduler, TICK};

/// How many rules are evaluated at once.
///
/// Sixteen. Each one is a single `ClickHouse` query over a window measured in minutes, so
/// this is a limit on how much work one installation asks of its telemetry store at once
/// rather than a throughput target — SPEC's 1 000 rules in a 60-second cycle needs about
/// seventeen evaluations a second, which this reaches with room to spare as long as the
/// queries themselves are fast.
pub const IN_FLIGHT: usize = 16;

/// What the loop has seen since the last reload.
///
/// Public because [`evaluate_and_deliver`] takes one, which is what lets a test drive one
/// rule's turn without a scheduler. Nothing outside reads the counters; they are printed
/// once a reload and reset.
#[derive(Debug, Default)]
pub struct Window {
    rules: usize,
    notifications: usize,
    suppressed: usize,
    failures: usize,
    /// One line per distinct failure, printed once.
    said: HashSet<String>,
}

impl Window {
    /// How many rules have been evaluated since the last reload.
    ///
    /// The only counter anything outside reads, and it is read by the scale measurement
    /// to check that a cycle evaluated every rule rather than dropping some of them
    /// quietly — a cycle that finishes inside its budget by doing less is the failure the
    /// measurement exists to rule out.
    #[must_use]
    pub const fn evaluated(&self) -> usize {
        self.rules
    }
}

/// Evaluate rules until `shutdown` completes.
///
/// Returns when the shutdown future does, after the evaluations already dispatched have
/// been left to finish on their own — an evaluation is a read and a state write, and
/// cancelling one mid-write is how a phase ends up recorded without the notification that
/// should have accompanied it.
pub async fn run<F>(engine: Engine, store: PgStore, shutdown: F)
where
    F: Future<Output = ()> + Send,
{
    let notifier = Notifier::new(store.clone());
    let mut scheduler = Scheduler::new();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let permits = Arc::new(Semaphore::new(IN_FLIGHT));
    let window = Arc::new(Mutex::new(Window::default()));
    let mut since_reload = RELOAD;

    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => {
                println!("alerts: stopping");
                return;
            }
            _ = ticker.tick() => {}
        }

        since_reload += TICK;
        if since_reload >= RELOAD {
            since_reload = std::time::Duration::ZERO;

            match scheduler.reload(&store).await {
                Ok(problems) => {
                    for problem in problems {
                        eprintln!("alerts: {problem}");
                    }
                }
                // The tenant list itself. Keep the schedule that is already loaded rather
                // than stopping: a database blip must not silence an installation.
                Err(e) => eprintln!("alerts: the rule list could not be read: {e}"),
            }

            let mut w = window.lock().await;
            if w.rules > 0 || w.failures > 0 {
                println!(
                    "alerts: {} evaluations, {} notifications, {} suppressed, {} failures \
                     ({} rules scheduled)",
                    w.rules,
                    w.notifications,
                    w.suppressed,
                    w.failures,
                    scheduler.len()
                );
            }
            *w = Window::default();
        }

        for (tenant, rule_id) in scheduler.due() {
            let engine = engine.clone();
            let store = store.clone();
            let notifier = notifier.clone();
            let permits = Arc::clone(&permits);
            let window = Arc::clone(&window);

            tokio::spawn(async move {
                // Dropped at the end of the task, which is what bounds concurrency. A
                // closed semaphore means the process is going away.
                let Ok(_permit) = permits.acquire().await else {
                    return;
                };
                evaluate_and_deliver(&engine, &notifier, &store, tenant, rule_id, &window).await;
            });
        }
    }
}

/// One rule's whole turn: read it, evaluate it, say what happened, deliver it.
///
/// Extracted from the spawn above so that the *wiring* — evaluate, then notify, with the
/// rule's channels and the alert's own `since` — is testable without a scheduler and a
/// clock. The loop above is then only the part that decides when this is called, which is
/// what [`Scheduler`] already has its own tests for.
///
/// [`Scheduler`]: crate::Scheduler
pub async fn evaluate_and_deliver(
    engine: &Engine,
    notifier: &Notifier,
    store: &PgStore,
    tenant: uops_core::TenantId,
    rule_id: uuid::Uuid,
    window: &Arc<Mutex<Window>>,
) {
    let scope = uops_core::TenantScope::collector(tenant);
    let now = Utc::now();

    // A rule deleted between the reload and now is not a failure: the next reload drops
    // it from the schedule.
    let Ok(rule) = store.alert_rule(&scope, rule_id).await else {
        return;
    };
    if !rule.enabled {
        return;
    }

    let outcome = engine.evaluate(&scope, &rule, now).await;
    let mut w = window.lock().await;
    w.rules += 1;

    match outcome {
        Ok(outcome) => {
            w.notifications += outcome.notifications();
            w.suppressed += outcome.suppressed;

            // A line for every notification, whatever the channels do with it. This is
            // the one thing here that must not be summarised away: an alert nobody can
            // see is the failure this whole crate is about, and an installation with no
            // channels configured yet still has a log.
            for decision in outcome.decisions.iter().filter(|d| d.notify) {
                println!(
                    "alerts: {} {} — {} (value {})",
                    decision.phase.as_str(),
                    rule.name,
                    decision.dedup_key,
                    decision
                        .value
                        .map_or_else(|| "none".to_owned(), |v| format!("{v:.3}"))
                );
            }

            // Delivery is outside the lock: it opens sockets, and holding the window's
            // mutex across a five-second webhook timeout would stall every other
            // evaluation's reporting behind one slow endpoint.
            drop(w);
            deliver(notifier, &scope, &rule, &outcome, window).await;
        }
        Err(e) => {
            w.failures += 1;
            let line = format!("alerts: rule {} could not be evaluated: {e}", rule.name);
            if w.said.insert(line.clone()) {
                eprintln!("{line}");
            }
        }
    }
}

/// Send one rule's notifications, and record what the channels did with them.
///
/// A rule with no channels reaches here and sends nothing, which is a rule being tuned
/// rather than a mistake — the line above has already been printed.
async fn deliver(
    notifier: &Notifier,
    scope: &uops_core::TenantScope,
    rule: &uops_store_pg::AlertRule,
    outcome: &crate::engine::RuleOutcome,
    window: &Arc<Mutex<Window>>,
) {
    for decision in outcome.decisions.iter().filter(|d| d.notify) {
        let notification = Notification {
            phase: decision.phase,
            severity: rule.severity,
            rule: rule.name.clone(),
            rule_id: rule.id,
            resource_id: decision.resource,
            resource: notifier.resource_name(scope, decision.resource).await,
            dedup_key: decision.dedup_key.clone(),
            value: decision.value,
            since: decision.since,
            at: Utc::now(),
        };

        match notifier.deliver(scope, &rule.notify, &notification).await {
            Ok(delivered) => {
                for one in delivered.iter().filter(|d| d.outcome != Outcome::Sent) {
                    // Refusals are said once each, through the same summarising path as
                    // an evaluation failure: a channel being rate-limited produces one of
                    // these per alert, and a storm is exactly when it does.
                    let line = match one.outcome {
                        Outcome::RateLimited => format!(
                            "alerts: channel {} is at its rate limit; notifications for {}                              are being refused",
                            one.channel, rule.name
                        ),
                        Outcome::OverBudget => format!(
                            "alerts: this tenant has spent its notification budget for                              today; {} was not delivered",
                            rule.name
                        ),
                        _ => format!(
                            "alerts: channel {} did not take {}: {}",
                            one.channel, rule.name, one.detail
                        ),
                    };
                    let mut w = window.lock().await;
                    if w.said.insert(line.clone()) {
                        eprintln!("{line}");
                    }
                }
            }
            Err(e) => {
                let line = format!("alerts: {} could not be delivered: {e}", rule.name);
                let mut w = window.lock().await;
                if w.said.insert(line.clone()) {
                    eprintln!("{line}");
                }
            }
        }
    }
}
