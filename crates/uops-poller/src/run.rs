//! The loop.
//!
//! Everything below this file has tests. This is the order those things happen in, which
//! is the part that cannot be unit-tested and the part an operator experiences:
//!
//! ```text
//!   reload    every tenant's devices and profiles, into the schedule
//!   tick      once a second: what the wheel says is due
//!   run       per task: ask the device, convert, write
//!   report    what failed, once per device per reload window
//! ```
//!
//! # Why a tick does not wait for its tasks
//!
//! [`uops_poll::poller::run_tick`] dispatches and returns; the executor bounds
//! concurrency and each task bounds its own time. A tick that waited would let one slow
//! device delay the next second's work, which is exactly the failure SPEC §M2 names —
//! and it would do so invisibly, as a schedule that gradually slips rather than a device
//! that reports a timeout.
//!
//! # Why failures are reported once per device per reload window
//!
//! A device that is down fails every poll. At a 60-second interval that is 1 440 lines a
//! day for one device, and a fleet with fifty such devices produces a log in which
//! nothing else can be found. Each device's first failure is printed; the rest are
//! counted and summarised, and the set is cleared on reload so a device that is still
//! down says so again every minute rather than never.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use uops_core::{ResourceId, TenantScope};
use uops_poll::plan::Device;
use uops_poll::poller::{JobKey, Schedule, Task, run_tick, tasks, tick_instant};
use uops_poll::{Executor, TickReport};
use uops_profile::Profile;
use uops_snmp::Target;
use uops_store_ch::ChStore;
use uops_store_pg::PgStore;

use crate::config::Config;
use crate::credentials::TransportSource;
use crate::fleet;
use crate::poll;

/// Everything a task needs, shared across the tick's tasks.
///
/// Behind an `Arc` because `run_tick` takes a `'static` closure — the tasks it spawns
/// outlive the call that made them, which is the whole point of not waiting for them.
pub struct Runner {
    store: PgStore,
    metrics: ChStore,
    /// Where a device's transport comes from. A trait object so the loop can be
    /// measured against simulated agents — see `tests/scale.rs`, which is how SPEC §M2's
    /// *1 000 simulated agents* criterion is checked through the binary rather than
    /// through the library underneath it.
    transports: Arc<dyn TransportSource>,
    devices: poll::Devices,
    /// Each device's discovery rule, from its profile. The schedule holds jobs, not
    /// profiles, and `Work::Discovery` carries only the table — not the column that
    /// names a row, nor the identifiers to read off it.
    discovery: Mutex<HashMap<ResourceId, Option<uops_profile::Discovery>>>,
    /// Devices whose failure has already been reported this reload window.
    reported: Mutex<HashSet<ResourceId>>,
    /// Failures not printed because the device had already been reported.
    suppressed: std::sync::atomic::AtomicUsize,
    timeout: Duration,
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl Runner {
    #[must_use]
    pub fn new(
        store: PgStore,
        metrics: ChStore,
        transports: Arc<dyn TransportSource>,
        timeout: Duration,
    ) -> Self {
        Self {
            store,
            metrics,
            transports,
            devices: poll::Devices::new(),
            discovery: Mutex::new(HashMap::new()),
            reported: Mutex::new(HashSet::new()),
            suppressed: std::sync::atomic::AtomicUsize::new(0),
            timeout,
        }
    }

    /// Put a fleet into the schedule, and keep what the schedule cannot hold.
    ///
    /// A `Schedule` holds jobs, and `Work::Discovery` carries only the table — not the
    /// column that names a row, nor the identifiers to read off it. Those live here, in
    /// a map beside it.
    ///
    /// The two go together and this is the only way to do either, deliberately. The
    /// first version had `reload` update the map and let callers call
    /// `Schedule::reload` themselves, and the scale test did exactly that: a thousand
    /// devices, correctly scheduled, every one of whose discovery jobs failed with "a
    /// discovery task with no discovery rule". Nothing was wrong with the poller; the
    /// trap was that two things had to be done and only one of them was hard to forget.
    ///
    /// Returns `(added, removed)`.
    pub async fn load(
        &self,
        schedule: &mut Schedule,
        devices: &[(Device, Profile)],
    ) -> (usize, usize) {
        {
            let mut discovery = self.discovery.lock().await;
            discovery.clear();
            for (device, profile) in devices {
                discovery.insert(device.resource, profile.discovery.first().cloned());
            }
        }
        schedule.reload(devices)
    }

    /// Run one task, reporting whatever went wrong.
    ///
    /// `Err(())` rather than the error: the executor counts outcomes and has no use for
    /// a reason, and the reason has already been reported here where the device it
    /// belongs to is known.
    async fn run_one(&self, task: Task) -> Result<usize, ()> {
        let device = task.device.resource;

        let transport = match self.transports.for_device(
            task.device.tenant,
            device,
            task.device.credential,
            self.timeout,
        ) {
            Ok(t) => t,
            Err(problem) => {
                self.report(device, &problem.to_string()).await;
                return Err(());
            }
        };

        let rule = self.discovery.lock().await.get(&device).cloned().flatten();
        // The profile's `resource_kind` is `uops_core::ResourceKind` already — a profile
        // is validated against the same vocabulary the schema uses, so there is nothing
        // to convert.
        let kind = rule.as_ref().map(|r| r.creates.resource_kind);
        let ctx = poll::Context {
            transport: Arc::clone(&transport) as Arc<dyn uops_snmp::Transport>,
            devices: &self.devices,
            metrics: &self.metrics,
            discovery: rule,
            observed_at: tick_instant(),
        };

        let polled = match poll::run(&task, &ctx).await {
            Ok(p) => p,
            Err(e) => {
                self.report(device, &e.to_string()).await;
                return Err(());
            }
        };

        // A discovery that succeeded is also the moment the device's sysObjectID is
        // known, and the moment its interfaces become resources. Both are writes to
        // PostgreSQL and both are here rather than inside `poll`, which deliberately has
        // no store.
        if matches!(task.work, uops_poll::plan::Work::Discovery { .. }) {
            self.record_sysobjectid(&task, transport.as_ref()).await;
            if let Some(kind) = kind {
                self.record_discovery(&task, kind, &polled.discovered).await;
            }
        }

        Ok(polled.rows)
    }

    /// Turn a discovery walk into child resources and `member_of` edges.
    ///
    /// Reported but not fatal. The walk that produced these also produced the interface
    /// names, which are already remembered and are what the next interface poll needs;
    /// failing the task because PostgreSQL was briefly unavailable would throw those away
    /// and make the device walk for them again.
    async fn record_discovery(
        &self,
        task: &Task,
        kind: uops_core::ResourceKind,
        found: &[poll::DiscoveredChild],
    ) {
        if found.is_empty() {
            return;
        }
        let children: Vec<uops_store_pg::DiscoveredChild> = found
            .iter()
            .map(|child| uops_store_pg::DiscoveredChild {
                name: child.name.clone(),
                kind,
                index: child
                    .index
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("."),
                identifiers: child.identifiers.clone(),
            })
            .collect();

        let scope = TenantScope::collector(task.device.tenant);
        match self
            .store
            .record_discovery(&scope, task.device.resource, &children)
            .await
        {
            // Worth a line, and only when something is new: after the first pass a device
            // rediscovers the same interfaces every fifteen minutes forever.
            Ok(report) if report.created > 0 => println!(
                "uops-poller: {} — {} new child resources, {} already known",
                task.device.resource, report.created, report.seen
            ),
            Ok(_) => {}
            Err(e) => {
                self.report(
                    task.device.resource,
                    &format!("its interfaces could not be recorded: {e}"),
                )
                .await;
            }
        }
    }

    /// Fetch and persist the device's `sysObjectID`, if it has changed.
    ///
    /// Best effort by design. This is what makes profile resolution work on the *next*
    /// poll; failing the current one over it would trade a working poll for a better
    /// profile later.
    async fn record_sysobjectid<T: uops_snmp::Transport + ?Sized>(
        &self,
        task: &Task,
        transport: &T,
    ) {
        let target = Target {
            address: task.device.address,
        };
        // The device does not answer it, or the request failed. Neither is worth a line:
        // the profile falls back to generic-snmp, which is what it was already doing.
        let Ok(Some(seen)) = poll::sysobjectid(transport, &target).await else {
            return;
        };

        let Some(changed) = self
            .devices
            .note_sysobjectid(task.device.resource, &seen)
            .await
        else {
            return;
        };

        let scope = TenantScope::collector(task.device.tenant);
        if let Err(e) = self
            .store
            .record_sysobjectid(&scope, task.device.resource, &changed)
            .await
        {
            // Worth a line: the poll worked and the cache did not, which means this
            // device will re-read and re-write its object id on every discovery.
            eprintln!(
                "uops-poller: {} answered sysObjectID {changed} but it could not be stored: {e}",
                task.device.resource
            );
        }
    }

    /// Print a device's failure, or count it if this device has already been reported.
    async fn report(&self, device: ResourceId, problem: &str) {
        if self.reported.lock().await.insert(device) {
            eprintln!("uops-poller: {device}: {problem}");
        } else {
            self.suppressed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Start a new reporting window, returning how many failures it suppressed.
    async fn new_window(&self) -> (usize, usize) {
        let devices = {
            let mut reported = self.reported.lock().await;
            let n = reported.len();
            reported.clear();
            n
        };
        let suppressed = self
            .suppressed
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        (devices, suppressed)
    }
}

/// Reload the fleet into the schedule, and say what changed.
///
/// Separate from [`serve`] so a reload can be run and asserted on without a clock.
///
/// # Errors
///
/// Only when the tenant list cannot be read — see [`crate::fleet::load`].
pub async fn reload(
    runner: &Runner,
    schedule: &mut Schedule,
    limit: i64,
) -> uops_core::Result<(usize, usize)> {
    let loaded = fleet::load(&runner.store, limit).await?;

    for skipped in &loaded.skipped {
        eprintln!(
            "uops-poller: tenant {} resource {}: {}",
            skipped.tenant, skipped.resource, skipped.reason
        );
    }

    let (added, removed) = runner.load(schedule, &loaded.devices).await;

    // Per-device memory follows the schedule. Without this the map grows for the life of
    // the process and a poller that has been up for a year holds state for every device
    // that ever existed.
    let live: HashSet<ResourceId> = loaded.devices.iter().map(|(d, _)| d.resource).collect();
    let forgotten = runner.devices.retain(&live).await;

    let retried = runner.transports.retry_failures();
    let (reported, suppressed) = runner.new_window().await;

    println!(
        "uops-poller: reload — {} tenants, {} devices (+{added} -{removed}), {} jobs; \
         forgot {forgotten}, retrying {retried} credentials; \
         last window: {reported} devices failed, {suppressed} repeats not printed",
        loaded.tenants,
        loaded.devices.len(),
        schedule.live_jobs(),
    );

    Ok((added, removed))
}

/// Poll until told to stop.
///
/// # Errors
///
/// Only the first reload's, which happens before the loop starts: a poller that cannot
/// read its fleet at all has nothing to do, and failing at startup is how an operator
/// finds out. A reload *inside* the loop that fails is reported and the previous fleet
/// is kept — the devices already scheduled are still real.
pub async fn serve(
    runner: Arc<Runner>,
    config: &Config,
    shutdown: impl Future<Output = ()> + Send,
) -> uops_core::Result<()> {
    let executor = Executor::new(config.limits.into());
    let mut schedule = Schedule::new();

    reload(&runner, &mut schedule, config.device_limit).await?;

    let mut tick = tokio::time::interval(uops_poll::poller::SLOT);
    // Skip missed ticks rather than firing them back to back. A process descheduled for
    // five seconds should resume polling, not try to catch up by running five seconds of
    // work at once against a fleet that has just been through whatever caused the pause.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut reload_at = tokio::time::interval(config.reload_every);
    reload_at.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    reload_at.tick().await; // the first tick of an interval is immediate

    let mut due: Vec<JobKey> = Vec::new();
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => {
                println!("uops-poller: stopping");
                return Ok(());
            }
            _ = reload_at.tick() => {
                if let Err(e) = reload(&runner, &mut schedule, config.device_limit).await {
                    // Keep going on the fleet we have. The devices already scheduled did
                    // not stop existing because PostgreSQL had a bad minute.
                    eprintln!("uops-poller: reload failed, keeping the current fleet: {e}");
                }
            }
            _ = tick.tick() => {
                note(&tick_once(&runner, &executor, &mut schedule, &mut due).await);
            }
        }
    }
}

/// Advance the wheel one slot and run what that slot holds.
///
/// Public so an end-to-end test can drive the loop without a clock: [`serve`] is a
/// `select!` around a timer, and a test that had to wait real seconds for a jittered job
/// to come due would be a test nobody runs. `due` is the caller's buffer, reused across
/// ticks so the loop allocates nothing per second.
///
/// Does not wait for the work — see the module docs on why.
pub async fn tick_once(
    runner: &Arc<Runner>,
    executor: &Executor,
    schedule: &mut Schedule,
    due: &mut Vec<JobKey>,
) -> TickReport {
    schedule.due(due);
    let batch = tasks(schedule, due);
    if batch.is_empty() {
        return TickReport::default();
    }
    let runner = Arc::clone(runner);
    run_tick(executor, batch, move |task| {
        let runner = Arc::clone(&runner);
        async move { runner.run_one(task).await }
    })
    .await
}

/// Say something about a tick, but only when it is worth saying.
///
/// A line per second is not a log. What is worth a line is a tick in which something
/// went wrong or the budget was hit — the two things that mean the schedule is not
/// keeping up.
fn note(report: &TickReport) {
    if report.failed == 0 && report.budget_exhausted == 0 {
        return;
    }
    eprintln!(
        "uops-poller: tick — {} due, {} ok, {} failed, {} out of time, {} samples",
        report.due, report.ok, report.failed, report.budget_exhausted, report.samples
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiet_tick_says_nothing() {
        // Asserting the decision rather than the output: a poller that printed a line a
        // second would bury everything else in it.
        let quiet = TickReport {
            due: 40,
            ok: 40,
            ..TickReport::default()
        };
        assert!(quiet.failed == 0 && quiet.budget_exhausted == 0);
        note(&quiet);

        let loud = TickReport {
            due: 40,
            ok: 39,
            failed: 1,
            ..TickReport::default()
        };
        assert!(loud.failed > 0);
    }

    #[test]
    fn a_poll_error_says_which_kind_it_is() {
        // The distinction that decides who gets paged: the device, or the poller.
        let store = poll::PollError::Store("clickhouse is unreachable".to_owned());
        assert!(store.to_string().contains("could not be stored"));
        let device = poll::PollError::Transport(uops_snmp::TransportError::Timeout);
        assert!(device.to_string().contains("timeout"));
    }
}
