//! Running the work the wheel hands out.
//!
//! SPEC §M2 names three things this has to do, and one it has to prove:
//!
//! > **Per-device concurrency cap and a global semaphore.** Devices have small SNMP
//! > agent queues and will drop requests before they refuse them — a flooded switch
//! > silently returns nothing, which looks like an outage.
//!
//! > **Timeout budget per device** so one dead device cannot delay the wheel.
//!
//! > Dead device does not delay polling of healthy devices (**measured, not assumed**).
//!
//! # The two limits are not the same limit
//!
//! The global semaphore is about *this* process: file descriptors, memory, and the CPU
//! to parse what comes back. The per-device cap is about the *device*, and it is the one
//! that is easy to get wrong, because exceeding it does not produce an error. An SNMP
//! agent with a full queue drops requests. The poller sees silence, retries, and makes
//! it worse — and the operator sees a switch that looks down while it is merely being
//! shouted at. One in flight per device by default.
//!
//! # The budget is per device, not per request
//!
//! A per-request timeout bounds one round trip. A device that answers every request
//! slowly, or a table that needs forty of them, still consumes unbounded wall-clock. The
//! budget covers everything the poll does — walks, retries, halving — so a single device
//! cannot hold a slot for longer than its budget no matter how it misbehaves.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

/// What bounds a polling round.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Total polls in flight across every device.
    pub global: usize,
    /// Requests in flight to one device. See the module docs: one, unless a device is
    /// known to tolerate more.
    pub per_device: usize,
    /// Wall-clock for one device's entire poll.
    pub device_budget: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // Enough to keep a 10 000-device fleet inside a 60-second cycle at a few
            // hundred milliseconds a device, and far below any default fd limit.
            global: 256,
            per_device: 1,
            device_budget: Duration::from_secs(10),
        }
    }
}

/// How a device's poll ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// The device used its whole budget. Distinct from a transport timeout: one request
    /// timing out is a lost packet, and a device that cannot finish inside its budget
    /// is a different problem with a different fix.
    BudgetExhausted,
    /// The poll ran and failed — unreachable, refused, malformed.
    Failed,
}

/// One device's poll, and what it cost.
///
/// Three durations, because the first version of this had one and it measured the wrong
/// thing. `elapsed` alone — the time from acquiring a slot — says nothing about whether
/// a device was *delayed*, and a fleet polled strictly one device at a time reports the
/// same `elapsed` as a fully concurrent one. SPEC's criterion is that a dead device does
/// not delay healthy ones, and waiting for a slot is exactly that delay.
#[derive(Clone, Copy, Debug)]
pub struct Completed<K> {
    pub key: K,
    pub outcome: Outcome,
    /// Waiting for a slot. Rises when the fleet is contended, which is the signal that
    /// the global limit is too small or something is holding slots.
    pub waited: Duration,
    /// The device's own time, once it had a slot.
    pub elapsed: Duration,
}

impl<K> Completed<K> {
    /// What the poll actually cost, end to end. The number SPEC's p95 is about.
    #[must_use]
    pub fn total(&self) -> Duration {
        self.waited + self.elapsed
    }
}

/// Runs polls under the limits.
#[derive(Debug)]
pub struct Executor {
    limits: Limits,
    global: Arc<Semaphore>,
}

impl Executor {
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            global: Arc::new(Semaphore::new(limits.global)),
        }
    }

    #[must_use]
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// Slots currently free. For a health endpoint: a poller permanently at zero is one
    /// that is not keeping up, and that is worth showing before cycles start slipping.
    #[must_use]
    pub fn available(&self) -> usize {
        self.global.available_permits()
    }

    /// Run one device's poll under the global limit and its own budget.
    ///
    /// `poll` is the whole job for one device — every request it makes, including
    /// retries. It is given a per-device semaphore so that a job which fans out
    /// internally still respects the cap.
    ///
    /// Errors inside `poll` become [`Outcome::Failed`]; the error itself belongs to the
    /// caller, which knows what a failure means for the thing it was polling.
    pub async fn run<K, F, Fut, E>(&self, key: K, poll: F) -> Completed<K>
    where
        F: FnOnce(Arc<Semaphore>) -> Fut + Send,
        Fut: Future<Output = Result<(), E>> + Send,
    {
        let queued = Instant::now();
        let _slot = self
            .global
            .acquire()
            .await
            .expect("the executor's semaphore is never closed");
        let waited = queued.elapsed();

        let device = Arc::new(Semaphore::new(self.limits.per_device));
        let started = Instant::now();

        let outcome = match tokio::time::timeout(self.limits.device_budget, poll(device)).await {
            Ok(Ok(())) => Outcome::Ok,
            Ok(Err(_)) => Outcome::Failed,
            Err(_elapsed) => Outcome::BudgetExhausted,
        };

        Completed {
            key,
            outcome,
            waited,
            elapsed: started.elapsed(),
        }
    }
}

/// Percentile of a set of durations, for the measurement SPEC asks for.
///
/// Nearest-rank, which is the definition that does not invent a value that never
/// happened. p95 of twenty samples is the nineteenth, not an interpolation between the
/// nineteenth and twentieth.
///
/// `p` is whole percent. Not a float: nobody asks for p95.5, and a float would mean
/// rounding a percentage into an index — a cast in both directions for no benefit.
#[must_use]
pub fn percentile(samples: &mut [Duration], p: u8) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    samples.sort_unstable();

    let n = samples.len();
    let rank = (n * usize::from(p.min(100))).div_ceil(100).max(1);
    samples[rank.min(n) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn limits(global: usize, budget_ms: u64) -> Limits {
        Limits {
            global,
            per_device: 1,
            device_budget: Duration::from_millis(budget_ms),
        }
    }

    #[tokio::test]
    async fn a_poll_that_succeeds_is_ok() {
        let ex = Executor::new(limits(4, 1_000));
        let done = ex.run(1u32, |_| async { Ok::<(), ()>(()) }).await;
        assert_eq!(done.outcome, Outcome::Ok);
        assert_eq!(done.key, 1);
    }

    #[tokio::test]
    async fn a_poll_that_fails_is_failed_and_not_a_timeout() {
        // An unreachable device and a device that ate its budget need different fixes,
        // so they must not arrive as the same outcome.
        let ex = Executor::new(limits(4, 1_000));
        let done = ex
            .run(1u32, |_| async { Err::<(), &str>("unreachable") })
            .await;
        assert_eq!(done.outcome, Outcome::Failed);
    }

    #[tokio::test]
    async fn a_poll_that_never_finishes_is_cut_off_at_the_budget() {
        let ex = Executor::new(limits(4, 50));
        let done = ex
            .run(1u32, |_| async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok::<(), ()>(())
            })
            .await;
        assert_eq!(done.outcome, Outcome::BudgetExhausted);
        assert!(
            done.elapsed < Duration::from_secs(1),
            "the budget did not cut it off: {:?}",
            done.elapsed
        );
        // Nothing else was running, so it cannot have queued.
        assert!(done.waited < Duration::from_millis(50), "{:?}", done.waited);
    }

    #[tokio::test]
    async fn the_global_limit_is_the_number_in_flight() {
        let ex = Arc::new(Executor::new(limits(3, 5_000)));
        let concurrent = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for id in 0..50u32 {
            let ex = Arc::clone(&ex);
            let concurrent = Arc::clone(&concurrent);
            let peak = Arc::clone(&peak);
            tasks.push(tokio::spawn(async move {
                ex.run(id, |_| async move {
                    let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    concurrent.fetch_sub(1, Ordering::SeqCst);
                    Ok::<(), ()>(())
                })
                .await
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }

        assert!(
            peak.load(Ordering::SeqCst) <= 3,
            "{} polls were in flight against a cap of 3",
            peak.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn the_per_device_cap_is_enforced_inside_one_poll() {
        // The cap that matters, and the one whose violation produces no error: a
        // flooded agent drops requests silently and looks like an outage.
        let ex = Executor::new(Limits {
            global: 8,
            per_device: 2,
            device_budget: Duration::from_secs(5),
        });
        let peak = Arc::new(AtomicUsize::new(0));
        let inner = Arc::clone(&peak);

        let done = ex
            .run(1u32, move |device| async move {
                let concurrent = Arc::new(AtomicUsize::new(0));
                let mut tasks = Vec::new();
                for _ in 0..10 {
                    let device = Arc::clone(&device);
                    let concurrent = Arc::clone(&concurrent);
                    let peak = Arc::clone(&inner);
                    tasks.push(tokio::spawn(async move {
                        let _permit = device.acquire().await.unwrap();
                        let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        concurrent.fetch_sub(1, Ordering::SeqCst);
                    }));
                }
                for t in tasks {
                    t.await.unwrap();
                }
                Ok::<(), ()>(())
            })
            .await;

        assert_eq!(done.outcome, Outcome::Ok);
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "{} requests were in flight to one device against a cap of 2",
            peak.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn waiting_for_a_slot_is_visible_rather_than_hidden() {
        // The bug this field exists because of. With one slot and two polls, the second
        // waits — and a Completed that only carried `elapsed` would report both as
        // instant, which made a fully serialised fleet indistinguishable from a
        // concurrent one.
        let ex = Arc::new(Executor::new(limits(1, 5_000)));
        let first = {
            let ex = Arc::clone(&ex);
            tokio::spawn(async move {
                ex.run(1u32, |_| async {
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    Ok::<(), ()>(())
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = ex.run(2u32, |_| async { Ok::<(), ()>(()) }).await;
        first.await.unwrap();

        assert!(
            second.waited >= Duration::from_millis(50),
            "the second poll queued behind the first and must say so: {:?}",
            second.waited
        );
        assert!(second.total() > second.elapsed);
    }

    #[test]
    fn percentile_is_nearest_rank() {
        // No interpolation: p95 of twenty samples is the nineteenth, a duration that
        // actually happened, not an average of two that did.
        let mut samples: Vec<Duration> = (1..=20).map(Duration::from_millis).collect();
        assert_eq!(percentile(&mut samples, 95), Duration::from_millis(19));
        assert_eq!(percentile(&mut samples, 50), Duration::from_millis(10));
        assert_eq!(percentile(&mut samples, 100), Duration::from_millis(20));
        assert_eq!(percentile(&mut Vec::new(), 95), Duration::ZERO);
        assert_eq!(
            percentile(&mut [Duration::from_millis(7)], 95),
            Duration::from_millis(7)
        );
    }
}
