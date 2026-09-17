//! The loop.
//!
//! Every part below this has its own tests. This is the order they happen in, and the
//! order is the only thing here that cannot be unit-tested in isolation:
//!
//! ```text
//!   load      devices from PostgreSQL, profiles from PostgreSQL
//!   resolve   each device to a profile: explicit, sysObjectID, or generic-snmp
//!   plan      profile + device into jobs, grouped by interval
//!   schedule  jobs into the wheel, spread across their interval
//!   tick      advance one second, run what is due through the executor
//!   write     samples to ClickHouse
//! ```
//!
//! # A reload does not restart the schedule
//!
//! Devices are added and removed while the poller is running. [`Schedule::reload`] inserts
//! what is new and forgets what is gone, and leaves everything else where it is — a
//! device already in the wheel keeps its slot. Rebuilding the wheel on every reload
//! would re-jitter the whole fleet each time a customer added one switch, which is the
//! same load spike a restart causes and for a much sillier reason.
//!
//! # One tick is one second of work, not one cycle
//!
//! [`run_tick`] advances the wheel by one slot and runs what that slot holds. It
//! does not wait for the work to finish: the executor bounds concurrency and each job
//! bounds its own time, so a tick that dispatches a slow device does not delay the next
//! tick. That is what makes "a dead device does not delay healthy ones" true at the loop
//! level rather than only inside the executor.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use uops_core::{ResourceId, TenantId};
use uops_profile::{Oid, Profile, Scope, resolve};

use crate::executor::{Executor, Outcome};
use crate::plan::{Device, Job, Work, plan};
use crate::sample::Subject;
use crate::wheel::Wheel;

/// What identifies a scheduled job inside the wheel.
///
/// The wheel holds keys, not jobs: an entry is copied on every reschedule and a `Job`
/// carries its metric list, which would mean cloning a vector of OIDs several thousand
/// times a second for no reason.
pub type JobKey = usize;

/// How long a wheel slot is, and therefore the scheduler's resolution.
pub const SLOT: Duration = Duration::from_secs(1);

/// Slots in the wheel — an hour, which is `uops_profile::interval::MAX`.
pub const SLOTS: usize = 3600;

/// What one device's poll produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Jobs the wheel handed out.
    pub due: usize,
    pub ok: usize,
    pub failed: usize,
    pub budget_exhausted: usize,
    /// Metric rows written.
    pub samples: usize,
}

impl TickReport {
    fn record(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Ok => self.ok += 1,
            Outcome::Failed => self.failed += 1,
            Outcome::BudgetExhausted => self.budget_exhausted += 1,
        }
    }
}

/// The scheduling half of the poller.
///
/// Deliberately does not own a transport, a store, or a `ClickHouse` client. It owns the
/// *schedule*, which is a pure data structure with a clock — and that is what makes the
/// hard properties (nothing lost on reload, a device spread across its interval, a tick
/// that does not block) testable without a network or a database.
///
/// The binary wires it to the rest; see `uops-poll`'s `main`.
pub struct Schedule {
    wheel: Wheel<JobKey>,
    jobs: Vec<Job>,
    /// Which keys belong to a device, so a removed device can be forgotten.
    by_device: HashMap<ResourceId, Vec<JobKey>>,
    /// Keys whose device has gone. Each is dropped from the wheel the next time it comes
    /// due — see [`Schedule::due`] — and forgotten here at the same moment, so neither
    /// this set nor the wheel accumulates the churn of a long-running process.
    retired: std::collections::HashSet<JobKey>,
    devices: HashMap<ResourceId, Device>,
}

impl std::fmt::Debug for Schedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Schedule")
            .field("devices", &self.devices.len())
            .field("jobs", &self.jobs.len())
            .field("retired", &self.retired.len())
            .finish_non_exhaustive()
    }
}

impl Default for Schedule {
    fn default() -> Self {
        Self::new()
    }
}

impl Schedule {
    #[must_use]
    pub fn new() -> Self {
        Self {
            wheel: Wheel::new(SLOTS, SLOT),
            jobs: Vec::new(),
            by_device: HashMap::new(),
            retired: std::collections::HashSet::new(),
            devices: HashMap::new(),
        }
    }

    /// How many devices are scheduled.
    #[must_use]
    pub fn devices(&self) -> usize {
        self.devices.len()
    }

    /// How many jobs are live — retired ones excluded, since they are only waiting to
    /// be dropped.
    #[must_use]
    pub fn live_jobs(&self) -> usize {
        self.jobs.len() - self.retired.len()
    }

    /// A device's details, for the poll that is about to run.
    #[must_use]
    pub fn device(&self, id: ResourceId) -> Option<&Device> {
        self.devices.get(&id)
    }

    #[must_use]
    pub fn job(&self, key: JobKey) -> Option<&Job> {
        self.jobs.get(key)
    }

    /// Add or remove devices so the schedule matches `current`.
    ///
    /// Devices already scheduled are left alone — see the module docs on why rebuilding
    /// is worse than it looks. Returns `(added, removed)`.
    pub fn reload(&mut self, current: &[(Device, Profile)]) -> (usize, usize) {
        let present: std::collections::HashSet<ResourceId> =
            current.iter().map(|(d, _)| d.resource).collect();

        let gone: Vec<ResourceId> = self
            .devices
            .keys()
            .filter(|id| !present.contains(id))
            .copied()
            .collect();
        let removed = gone.len();
        for id in gone {
            self.devices.remove(&id);
            if let Some(keys) = self.by_device.remove(&id) {
                self.retired.extend(keys);
            }
        }

        let mut added = 0;
        for (device, profile) in current {
            if self.devices.contains_key(&device.resource) {
                continue;
            }
            self.devices.insert(device.resource, device.clone());
            added += 1;

            let mut keys = Vec::new();
            for job in plan(device, profile) {
                let key = self.jobs.len();
                let seed = job.seed();
                let interval = job.interval;
                self.jobs.push(job);
                // An interval the wheel cannot hold was refused by Profile::validate, so
                // this cannot fail for a validated profile — and if it somehow does, the
                // job is simply not scheduled rather than the whole reload failing.
                if self.wheel.insert(key, interval, seed).is_ok() {
                    keys.push(key);
                }
            }
            self.by_device.insert(device.resource, keys);
        }

        (added, removed)
    }

    /// Advance one slot and return the jobs that are due.
    ///
    /// A retired job is dropped from the wheel at the moment it comes due — see
    /// [`Wheel::advance_retaining`]. It used to be filtered out of the *result* instead,
    /// which left the entry rescheduling itself forever: a poller running for months with
    /// device churn held an entry per retired job and paid to move each one every
    /// interval. The tombstone is forgotten at the same time, so that set stops growing
    /// too.
    pub fn due(&mut self, out: &mut Vec<JobKey>) {
        out.clear();
        let retired = &mut self.retired;
        self.wheel
            .advance_retaining(out, |key| !retired.remove(key));
    }
}

/// Everything one job needs to run, resolved from the schedule.
#[derive(Clone, Debug)]
pub struct Task {
    pub key: JobKey,
    pub device: Device,
    pub work: Work,
}

impl Task {
    /// What a sample from this task is about.
    #[must_use]
    pub fn subject(&self) -> Subject {
        Subject {
            tenant: self.device.tenant,
            resource: self.device.resource,
            site: Some(self.device.site),
        }
    }
}

/// Resolve due keys into runnable tasks, skipping anything whose device has gone.
#[must_use]
pub fn tasks(schedule: &Schedule, due: &[JobKey]) -> Vec<Task> {
    due.iter()
        .filter_map(|key| {
            let job = schedule.job(*key)?;
            let device = schedule.device(job.device)?;
            Some(Task {
                key: *key,
                device: device.clone(),
                work: job.work.clone(),
            })
        })
        .collect()
}

/// Choose a profile for a device from what is known about it.
///
/// The cached `sysObjectID` is a string from the database and may be anything a hand
/// edit put there, so an unparseable one falls back rather than failing the device —
/// polling it with `generic-snmp` beats not polling it because a column is malformed.
#[must_use]
pub fn profile_for<'a>(
    profiles: &'a [Profile],
    explicit_key: Option<&str>,
    sysobjectid: Option<&str>,
) -> Option<&'a Profile> {
    let parsed: Option<Oid> = sysobjectid.and_then(|s| s.parse().ok());
    resolve::resolve(profiles, explicit_key, parsed.as_ref()).map(|r| r.profile)
}

/// The interface names a device reported, keyed by index.
///
/// Built from a discovery walk so that interface-scoped samples can be labelled. Kept
/// per device and refreshed on the discovery interval rather than per poll — SPEC's
/// discovery cadence, not the metric cadence.
pub type InterfaceNames = BTreeMap<Vec<u32>, String>;

/// Which metrics in a job are interface-scoped, for the caller deciding which walk to
/// make. A `Work` already separates them; this is for a profile being inspected.
#[must_use]
pub fn interface_metrics(profile: &Profile) -> Vec<&uops_profile::Metric> {
    profile
        .metrics
        .iter()
        .filter(|m| m.scope == Scope::Interface)
        .collect()
}

/// Run one tick's worth of tasks under the executor, returning what happened.
///
/// `run_one` is the whole of a device's poll: the transport, the walk, the conversion
/// and the write. It is a parameter rather than a field so that the loop can be tested
/// against a closure and run in production against SNMP, without the loop knowing the
/// difference.
pub async fn run_tick<F, Fut>(executor: &Executor, tasks: Vec<Task>, run_one: F) -> TickReport
where
    F: Fn(Task) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<usize, ()>> + Send,
{
    let mut report = TickReport {
        due: tasks.len(),
        ..TickReport::default()
    };

    let samples = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut handles = Vec::with_capacity(tasks.len());

    for task in tasks {
        let run_one = run_one.clone();
        let samples = Arc::clone(&samples);
        let key = task.key;
        handles.push(async move {
            executor
                .run(key, move |_device| async move {
                    let written = run_one(task).await?;
                    samples.fetch_add(written, std::sync::atomic::Ordering::Relaxed);
                    Ok::<(), ()>(())
                })
                .await
        });
    }

    for completed in futures_util::future::join_all(handles).await {
        report.record(completed.outcome);
    }
    report.samples = samples.load(std::sync::atomic::Ordering::Relaxed);
    report
}

/// The observed time for a tick's samples.
///
/// One instant for everything dispatched in a tick, so metrics from one device are
/// joinable at the same timestamp rather than smeared across however long the poll took.
#[must_use]
pub fn tick_instant() -> chrono::DateTime<Utc> {
    Utc::now()
}

/// A tenant's devices, for the binary that loads them.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub tenant: TenantId,
    pub devices: Vec<(Device, Profile)>,
}
