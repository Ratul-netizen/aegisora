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
use uops_store_pg::PgStore;

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
#[derive(Debug, Default)]
struct Window {
    rules: usize,
    notifications: usize,
    suppressed: usize,
    failures: usize,
    /// One line per distinct failure, printed once.
    said: HashSet<String>,
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
            let permits = Arc::clone(&permits);
            let window = Arc::clone(&window);

            tokio::spawn(async move {
                // Dropped at the end of the task, which is what bounds concurrency. A
                // closed semaphore means the process is going away.
                let Ok(_permit) = permits.acquire().await else {
                    return;
                };

                let scope = uops_core::TenantScope::collector(tenant);
                let now = Utc::now();

                // A rule deleted between the reload and now is not a failure: the next
                // reload drops it from the schedule.
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

                        // Until channels exist, a notification is a line. It is the one
                        // thing here that must not be summarised away: an alert nobody
                        // can see is the failure this whole crate is about.
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
                    }
                    Err(e) => {
                        w.failures += 1;
                        let line =
                            format!("alerts: rule {} could not be evaluated: {e}", rule.name);
                        if w.said.insert(line.clone()) {
                            eprintln!("{line}");
                        }
                    }
                }
            });
        }
    }
}
