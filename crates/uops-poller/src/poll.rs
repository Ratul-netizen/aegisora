//! One device's poll: the request, the conversion, and the write.
//!
//! Everything here is per task. The scheduling that decides *which* tasks is
//! `uops_poll::poller`; the concurrency and the time budget are `uops_poll::executor`.
//! What this owns is the part between a [`Task`] and rows in `ClickHouse`.
//!
//! # What a device remembers between polls
//!
//! Two things, and both would be wrong to recompute:
//!
//! * its `max-repetitions`, which is the result of however many `tooBig` halvings it
//!   took to find a size the agent accepts. Forgetting it means rediscovering it —
//!   and every rediscovery is a wasted request against a device that already told us.
//! * its interface names, from the discovery walk. Interface-scoped metrics are polled
//!   far more often than discovery runs, so between discoveries the names come from
//!   here. A poll with no names still writes rows, labelled by index — see
//!   `uops_poll::sample::interface_columns` on why dropping them would be worse.
//!
//! # Failure is per task, not per device
//!
//! A task that fails returns `Err(())` to the executor, which counts it. It does not
//! retry: the next poll of that job is already scheduled, and a retry inside the budget
//! is a second request competing with the first for the same device's attention.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use uops_core::ResourceId;
use uops_poll::plan::{MetricRequest, Work};
use uops_poll::poller::{InterfaceNames, Task};
use uops_poll::sample::{self, Numeric, Reading};
use uops_profile::Oid;
use uops_snmp::bulk::Tuning;
use uops_snmp::{Target, Transport, TransportError, Value, VarBind, walk};
use uops_store_ch::MetricRow;

/// `SNMPv2-MIB::sysObjectID`. What profile resolution matches on.
const SYSOBJECTID: &str = "1.3.6.1.2.1.1.2";

/// What one device remembers between polls.
#[derive(Debug, Default)]
pub struct DeviceState {
    pub tuning: Tuning,
    pub names: InterfaceNames,
    /// The `sysObjectID` last seen on the wire, so a re-write to `PostgreSQL` only
    /// happens when it actually changed — which is when the hardware was replaced.
    pub sysobjectid: Option<String>,
}

/// Per-device state for the whole devices.
///
/// One lock over a map rather than a lock per device: the critical section is a clone of
/// a `Tuning` (two integers) and, for interface work, of a name map. Both are short, and
/// a lock per device is a second allocation per device to save contention that a
/// once-a-second workload does not generate.
#[derive(Debug, Default)]
pub struct Devices {
    devices: tokio::sync::Mutex<HashMap<ResourceId, DeviceState>>,
}

impl Devices {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// This device's remembered `max-repetitions` and interface names.
    pub async fn snapshot(&self, device: ResourceId) -> (Tuning, InterfaceNames) {
        let mut devices = self.devices.lock().await;
        let state = devices.entry(device).or_default();
        (state.tuning, state.names.clone())
    }

    /// Store the `max-repetitions` a walk settled on.
    pub async fn remember_tuning(&self, device: ResourceId, tuning: Tuning) {
        self.devices.lock().await.entry(device).or_default().tuning = tuning;
    }

    /// Replace a device's interface names after a discovery walk.
    pub async fn remember_names(&self, device: ResourceId, names: InterfaceNames) {
        self.devices.lock().await.entry(device).or_default().names = names;
    }

    /// Record a `sysObjectID`, returning it when it is new or changed.
    ///
    /// `Some` is the signal to write it to `PostgreSQL`. A device whose object id has
    /// not moved since the last discovery produces `None`, which is every discovery
    /// after the first.
    pub async fn note_sysobjectid(&self, device: ResourceId, seen: &str) -> Option<String> {
        let mut devices = self.devices.lock().await;
        let state = devices.entry(device).or_default();
        if state.sysobjectid.as_deref() == Some(seen) {
            return None;
        }
        state.sysobjectid = Some(seen.to_owned());
        Some(seen.to_owned())
    }

    /// Forget a device entirely. For one that has left the devices.
    pub async fn forget(&self, device: ResourceId) {
        self.devices.lock().await.remove(&device);
    }

    /// Forget every device not in `keep`, returning how many went.
    pub async fn retain(&self, keep: &std::collections::HashSet<ResourceId>) -> usize {
        let mut devices = self.devices.lock().await;
        let before = devices.len();
        devices.retain(|id, _| keep.contains(id));
        before - devices.len()
    }

    /// How many devices have remembered state.
    pub async fn len(&self) -> usize {
        self.devices.lock().await.len()
    }

    /// Whether anything is remembered. Present because clippy asks for it beside `len`.
    pub async fn is_empty(&self) -> bool {
        self.devices.lock().await.is_empty()
    }
}

/// What a poll produced, or why it did not.
#[derive(Debug)]
pub enum PollError {
    Transport(TransportError),
    Walk(walk::WalkError),
    /// The rows were read but could not be stored. Distinguished from a transport
    /// failure because it says the *poller* is broken, not the device — and a fleet
    /// where every device "fails" is a very different page than one where one does.
    Store(String),
    /// Work this binary does not carry out yet. Counted rather than silently succeeding.
    Unsupported(&'static str),
}

impl std::fmt::Display for PollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "{e}"),
            Self::Walk(e) => write!(f, "{e}"),
            Self::Store(e) => write!(f, "the samples could not be stored: {e}"),
            Self::Unsupported(what) => write!(f, "{what} is not implemented yet"),
        }
    }
}

/// Somewhere to put rows.
///
/// Narrower than `uops_store_ch::MetricStore`, which also carries `query` and `health`.
/// A poll writes and never reads, and depending on the wider trait would mean this
/// module could only be tested against something that can answer a compiled query —
/// which is `ClickHouse` and nothing else. The implementation for the real store is one
/// method below.
#[async_trait::async_trait]
pub trait Sink: Send + Sync {
    /// Store these rows. The error is a string because nothing here can act on a
    /// structured one: it is reported and the task fails.
    async fn write(&self, rows: &[MetricRow]) -> Result<(), String>;
}

#[async_trait::async_trait]
impl Sink for uops_store_ch::ChStore {
    async fn write(&self, rows: &[MetricRow]) -> Result<(), String> {
        uops_store_ch::MetricStore::insert_metrics(self, rows)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Everything a task needs that is not in the task.
///
/// A struct rather than five parameters because it is threaded through every arm of
/// [`run`] and clippy counts arguments.
pub struct Context<'a, S: Sink + ?Sized> {
    pub transport: Arc<dyn Transport>,
    pub devices: &'a Devices,
    pub metrics: &'a S,
    /// The column a discovered interface's name is read from — the profile's
    /// `discovery.creates.name_from`. `None` for a profile that discovers nothing, in
    /// which case a discovery task cannot have been scheduled.
    pub name_from: Option<Oid>,
    pub observed_at: DateTime<Utc>,
}

impl<S: Sink + ?Sized> std::fmt::Debug for Context<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because `Arc<dyn Transport>` is not `Debug` and should not be:
        // the one implementation holds a credential, and a trait object that could be
        // printed is a credential that could be printed by accident.
        f.debug_struct("Context")
            .field("name_from", &self.name_from)
            .field("observed_at", &self.observed_at)
            .finish_non_exhaustive()
    }
}

/// Run one task: ask the device, convert what it said, write the rows.
///
/// Returns how many rows were written.
///
/// # Errors
///
/// A transport or walk failure, a store failure, or work this binary does not do yet.
pub async fn run<S: Sink + ?Sized>(task: &Task, ctx: &Context<'_, S>) -> Result<usize, PollError> {
    let target = Target {
        address: task.device.address,
    };

    match &task.work {
        Work::Scalars { metrics } => scalars(task, ctx, &target, metrics).await,
        Work::InterfaceColumns { metrics } => columns(task, ctx, &target, metrics).await,
        Work::Discovery { table } => discovery(task, ctx, &target, table).await,
        // ICMP needs a raw socket, which needs a privilege this process should not have
        // by default; TCP needs a port the built-in profiles do not set. Both are real
        // work rather than a line of code, and counting them is the honest thing to do
        // until they exist. See STATUS.
        Work::Availability { .. } => Err(PollError::Unsupported("the availability check")),
    }
}

/// The scalar metrics, in one request.
async fn scalars<S: Sink + ?Sized>(
    task: &Task,
    ctx: &Context<'_, S>,
    target: &Target,
    metrics: &[MetricRequest],
) -> Result<usize, PollError> {
    let oids: Vec<Oid> = metrics.iter().map(|m| m.oid.clone()).collect();
    let varbinds = ctx
        .transport
        .get_scalars(target, &oids)
        .await
        .map_err(PollError::Transport)?;

    let readings = numeric(&varbinds);
    let batch = sample::scalars(task.subject(), metrics, &readings, ctx.observed_at);
    write(ctx, &batch.rows).await
}

/// One walk per interface-scoped metric.
///
/// Per metric rather than one walk of the whole table: a column walk returns only the
/// column asked for, and walking `ifTable` to extract two of its twenty-two columns
/// would move twenty more columns across the network per device per poll.
async fn columns<S: Sink + ?Sized>(
    task: &Task,
    ctx: &Context<'_, S>,
    target: &Target,
    metrics: &[MetricRequest],
) -> Result<usize, PollError> {
    let (mut tuning, names) = ctx.devices.snapshot(task.device.resource).await;
    let mut rows: Vec<MetricRow> = Vec::new();

    for metric in metrics {
        let varbinds = walk::walk(ctx.transport.as_ref(), target, &metric.oid, &mut tuning)
            .await
            .map_err(PollError::Walk)?;
        let readings = numeric(&varbinds);
        let batch =
            sample::interface_columns(task.subject(), metric, &readings, &names, ctx.observed_at);
        rows.extend(batch.rows);
    }

    // After the loop: the tuning a later metric settled on is the one worth keeping, and
    // writing it per metric would mean a lock acquisition per column for no extra
    // information.
    ctx.devices
        .remember_tuning(task.device.resource, tuning)
        .await;
    write(ctx, &rows).await
}

/// The discovery walk: interface names, and the device's `sysObjectID`.
///
/// Writes no metric rows. What it produces is the labelling every interface-scoped
/// sample between now and the next discovery depends on, and the object id that decides
/// which profile the device is polled under after a restart.
async fn discovery<S: Sink + ?Sized>(
    task: &Task,
    ctx: &Context<'_, S>,
    target: &Target,
    table: &Oid,
) -> Result<usize, PollError> {
    let Some(name_from) = ctx.name_from.clone() else {
        // Profile::validate refuses interface metrics with no discovery rule, and plan()
        // only schedules discovery when there is one — so this is a consistency check on
        // the caller rather than a case that arises.
        return Err(PollError::Unsupported(
            "a discovery task with no name column",
        ));
    };

    let (mut tuning, _) = ctx.devices.snapshot(task.device.resource).await;

    let rows = walk::walk(ctx.transport.as_ref(), target, &name_from, &mut tuning)
        .await
        .map_err(PollError::Walk)?;

    let mut names = InterfaceNames::new();
    for vb in &rows {
        // Both from the same walk, so an index with no readable name simply has no
        // entry — which is what interface_columns expects.
        if let (Some(index), Value::Bytes(bytes)) = (walk::index_of(&vb.oid, &name_from), &vb.value)
        {
            // from_utf8_lossy, not from_utf8: an ifName with one bad byte is still the
            // name a person reads on a graph, and refusing it would label the interface
            // by index forever.
            names.insert(index, String::from_utf8_lossy(bytes).into_owned());
        }
    }
    ctx.devices
        .remember_names(task.device.resource, names)
        .await;
    ctx.devices
        .remember_tuning(task.device.resource, tuning)
        .await;

    // The table itself is walked for the child resources it becomes — see STATUS; that
    // is the last M2 criterion and it is not here yet. Named so that a reader does not
    // conclude `table` is unused by accident.
    let _ = table;

    Ok(0)
}

/// Ask a device for its `sysObjectID`.
///
/// Separate from the discovery walk because it is a scalar and because its failure is
/// not a reason to lose the interface names: a device that answers `ifName` and not
/// `sysObjectID` is unusual but it is still a device worth polling.
///
/// # Errors
///
/// A transport failure.
pub async fn sysobjectid<T: Transport + ?Sized>(
    transport: &T,
    target: &Target,
) -> Result<Option<String>, TransportError> {
    let oid: Oid = SYSOBJECTID.parse().expect("a constant OID must parse");
    let varbinds = transport
        .get_scalars(target, std::slice::from_ref(&oid))
        .await?;

    Ok(varbinds.iter().find_map(|vb| match &vb.value {
        Value::ObjectId(value) if vb.oid.starts_with(&oid) => Some(value.to_string()),
        _ => None,
    }))
}

/// The varbinds that hold a number, as readings.
///
/// Everything else — an `OCTET STRING`, an `EndOfMibView`, a type no profile reads — is
/// dropped here rather than in the conversion, so `Batch::skipped` counts metrics the
/// agent did not answer rather than varbinds that were never numbers.
fn numeric(varbinds: &[VarBind]) -> Vec<Reading> {
    varbinds
        .iter()
        .filter_map(|vb| {
            let value = match vb.value {
                Value::Integer(n) => Numeric::Signed(n),
                Value::Unsigned(n) | Value::Counter64(n) => Numeric::Unsigned(n),
                _ => return None,
            };
            Some(Reading {
                oid: vb.oid.clone(),
                value,
            })
        })
        .collect()
}

/// Write the rows, or nothing if there are none.
async fn write<S: Sink + ?Sized>(
    ctx: &Context<'_, S>,
    rows: &[MetricRow],
) -> Result<usize, PollError> {
    if rows.is_empty() {
        // Not an error. A device that implements none of a profile's optional OIDs
        // produces no rows and is working correctly; the conversion counted the misses.
        return Ok(0);
    }
    ctx.metrics.write(rows).await.map_err(PollError::Store)?;
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_numbers_become_readings() {
        let oid: Oid = "1.3.6.1.2.1.1.3.0".parse().unwrap();
        let vb = |value: Value| VarBind {
            oid: oid.clone(),
            value,
        };

        // A Counter64 is unsigned even though it arrives as its own variant; collapsing
        // it to Signed would turn a counter past 2^63 negative.
        assert_eq!(
            numeric(&[vb(Value::Counter64(u64::MAX))]),
            vec![Reading {
                oid: oid.clone(),
                value: Numeric::Unsigned(u64::MAX)
            }]
        );
        assert_eq!(
            numeric(&[vb(Value::Integer(-1))]),
            vec![Reading {
                oid: oid.clone(),
                value: Numeric::Signed(-1)
            }]
        );

        // Everything that is not a number, dropped rather than guessed at.
        for value in [
            Value::Bytes(b"eth0".to_vec()),
            Value::EndOfMibView,
            Value::NoSuchInstance,
            Value::Other,
        ] {
            assert!(numeric(&[vb(value.clone())]).is_empty(), "{value:?}");
        }
    }

    #[tokio::test]
    async fn a_device_remembers_its_tuning_and_names() {
        let devices = Devices::new();
        let device = ResourceId::new();

        let (tuning, names) = devices.snapshot(device).await;
        assert_eq!(tuning, Tuning::default());
        assert!(names.is_empty());

        let mut names = InterfaceNames::new();
        names.insert(vec![1], "eth0".to_owned());
        devices.remember_names(device, names).await;

        let (_, remembered) = devices.snapshot(device).await;
        assert_eq!(remembered.get(&vec![1]).map(String::as_str), Some("eth0"));
    }

    #[tokio::test]
    async fn a_sysobjectid_is_reported_once_until_it_changes() {
        // The point: a write to PostgreSQL per discovery per device forever, versus one
        // when the hardware is replaced.
        let devices = Devices::new();
        let device = ResourceId::new();

        assert_eq!(
            devices.note_sysobjectid(device, "1.3.6.1.4.1.9.1.1").await,
            Some("1.3.6.1.4.1.9.1.1".to_owned())
        );
        assert_eq!(
            devices.note_sysobjectid(device, "1.3.6.1.4.1.9.1.1").await,
            None
        );
        assert_eq!(
            devices
                .note_sysobjectid(device, "1.3.6.1.4.1.2021.250.10")
                .await,
            Some("1.3.6.1.4.1.2021.250.10".to_owned())
        );
    }

    #[tokio::test]
    async fn a_device_that_leaves_the_fleet_is_forgotten() {
        // Otherwise the map grows for the life of the process, and a poller that has
        // been up for a year holds state for every device that ever existed.
        let devices = Devices::new();
        let (kept, gone) = (ResourceId::new(), ResourceId::new());
        devices.snapshot(kept).await;
        devices.snapshot(gone).await;
        assert_eq!(devices.len().await, 2);

        let keep: std::collections::HashSet<_> = std::iter::once(kept).collect();
        assert_eq!(devices.retain(&keep).await, 1);
        assert_eq!(devices.len().await, 1);
    }
}
