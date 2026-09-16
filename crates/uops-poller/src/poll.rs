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

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use uops_core::{Identifier, ResourceId};
use uops_poll::plan::{MetricRequest, Work};
use uops_poll::poller::{InterfaceNames, Task};
use uops_poll::sample::{self, Numeric, Reading};
use uops_profile::{Oid, Profile};
use uops_snmp::bulk::Tuning;
use uops_snmp::{Target, Transport, TransportError, Value, VarBind, walk};
use uops_store_ch::MetricRow;

use crate::check;

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
    /// The availability check could not be carried out — which is not the same as the
    /// device being down. See `check::CheckError`.
    Check(check::CheckError),
}

impl std::fmt::Display for PollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "{e}"),
            Self::Walk(e) => write!(f, "{e}"),
            Self::Store(e) => write!(f, "the samples could not be stored: {e}"),
            Self::Unsupported(what) => write!(f, "{what} is not implemented yet"),
            Self::Check(e) => write!(f, "{e}"),
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
    /// The device's profile.
    ///
    /// The whole profile, not the one field each arm needs. The first version carried
    /// the discovery rule alone, and when availability arrived it wanted a different
    /// field of the same document — two side maps, populated in one place and read in
    /// another, which is the shape that had already drifted once. `Arc` because a fleet
    /// is a thousand devices and a handful of distinct profiles.
    pub profile: Option<Arc<Profile>>,
    pub observed_at: DateTime<Utc>,
}

impl<S: Sink + ?Sized> std::fmt::Debug for Context<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because `Arc<dyn Transport>` is not `Debug` and should not be:
        // the one implementation holds a credential, and a trait object that could be
        // printed is a credential that could be printed by accident.
        f.debug_struct("Context")
            .field("profile", &self.profile.as_ref().map(|p| p.id.as_str()))
            .field("observed_at", &self.observed_at)
            .finish_non_exhaustive()
    }
}

/// What one task produced.
#[derive(Debug, Default)]
pub struct Polled {
    /// Metric rows written.
    pub rows: usize,
    /// What an availability check found, and the sentence describing it. `None` for
    /// every other kind of task.
    ///
    /// Returned rather than acted on here for the same reason as `discovered`: deciding
    /// that this is a *transition* means knowing what the status was, and writing one
    /// means a `PostgreSQL` update and a `ClickHouse` row.
    pub reachability: Option<(check::Reachability, String)>,
    /// Rows of a discovery walk, for the caller to persist. Empty for every other kind
    /// of task.
    ///
    /// Returned rather than written here because this module deliberately has no store —
    /// [`Sink`] is the only thing it can write to, and that takes metric rows and nothing
    /// else. Persisting a child resource is a `PostgreSQL` transaction and belongs with
    /// the code that owns that connection.
    pub discovered: Vec<DiscoveredChild>,
}

/// One row of a discovery walk, as the device reported it.
///
/// `uops_store_pg::DiscoveredChild` is the same idea one layer down. They are separate
/// types because this one is what SNMP said and that one is what the database takes;
/// collapsing them would put a `ResourceKind` in the transport's vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredChild {
    /// The row's index within its table — `ifIndex`, as the walk's instance suffix.
    pub index: Vec<u32>,
    pub name: String,
    /// What the rule's `identifiers` columns held for this row, already rendered.
    pub identifiers: Vec<Identifier>,
}

/// Run one task: ask the device, convert what it said, write the rows.
///
/// # Errors
///
/// A transport or walk failure, a store failure, or work this binary does not do yet.
pub async fn run<S: Sink + ?Sized>(task: &Task, ctx: &Context<'_, S>) -> Result<Polled, PollError> {
    let target = Target {
        address: task.device.address,
    };

    match &task.work {
        Work::Scalars { metrics } => scalars(task, ctx, &target, metrics).await.map(rows_only),
        Work::InterfaceColumns { metrics } => {
            columns(task, ctx, &target, metrics).await.map(rows_only)
        }
        Work::Discovery { table } => discovery(task, ctx, &target, table).await,
        Work::Availability { index } => availability(ctx, &target, *index).await,
    }
}

/// Is the device there at all.
///
/// Writes no metric rows: availability is a *state*, and a state row is written on a
/// transition rather than on every check — see `uops_store_ch::StateRow`. What the
/// caller does with this is decide whether anything changed.
async fn availability<S: Sink + ?Sized>(
    ctx: &Context<'_, S>,
    target: &Target,
    index: usize,
) -> Result<Polled, PollError> {
    let check = ctx
        .profile
        .as_ref()
        .and_then(|p| p.availability.get(index))
        .ok_or(PollError::Unsupported(
            "an availability task whose profile has no check at that index",
        ))?;

    let outcome = check::run(check, target.address)
        .await
        .map_err(PollError::Check)?;

    Ok(Polled {
        reachability: Some((outcome, check::describe(check, outcome))),
        ..Polled::default()
    })
}

/// A task that discovers nothing, as a [`Polled`].
fn rows_only(rows: usize) -> Polled {
    Polled {
        rows,
        ..Polled::default()
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

/// The discovery walk: what rows the table has, what they are called, and what
/// identifies them.
///
/// Writes no metric rows. What it produces is two things with different lifetimes: the
/// names, which label every interface-scoped sample until the next discovery, and the
/// children, which the caller turns into resources and `member_of` edges.
///
/// # One walk per column, not one of the table
///
/// The rule names a table — `ifEntry` — and this walks the columns it actually reads
/// instead. `ifTable` has twenty-two columns; a device with forty-eight ports would move
/// a thousand varbinds to extract ninety-six. The table OID stays in the profile because
/// it is what documents *what is being discovered*, and a future rule that needed a
/// column this code does not know about would be read from there.
async fn discovery<S: Sink + ?Sized>(
    task: &Task,
    ctx: &Context<'_, S>,
    target: &Target,
    table: &Oid,
) -> Result<Polled, PollError> {
    let Some(rule) = ctx
        .profile
        .as_ref()
        .and_then(|p| p.discovery.first().cloned())
    else {
        // Profile::validate refuses interface metrics with no discovery rule, and plan()
        // only schedules discovery when there is one — so this is a consistency check on
        // the caller rather than a case that arises.
        return Err(PollError::Unsupported(
            "a discovery task with no discovery rule",
        ));
    };
    debug_assert!(
        rule.walk == *table,
        "the scheduled table and the rule's table must be the same"
    );

    let (mut tuning, _) = ctx.devices.snapshot(task.device.resource).await;

    let name_rows = walk::walk(
        ctx.transport.as_ref(),
        target,
        &rule.creates.name_from,
        &mut tuning,
    )
    .await
    .map_err(PollError::Walk)?;

    // Index → name. An index with no readable name simply has no entry, which is what
    // `interface_columns` expects and why a nameless interface is still labelled.
    let mut names = InterfaceNames::new();
    for vb in &name_rows {
        if let (Some(index), Value::Bytes(bytes)) =
            (walk::index_of(&vb.oid, &rule.creates.name_from), &vb.value)
        {
            // from_utf8_lossy, not from_utf8: an ifName with one bad byte is still the
            // name a person reads on a graph, and refusing it would label the interface
            // by index forever.
            let name = String::from_utf8_lossy(bytes).trim().to_owned();
            if !name.is_empty() {
                names.insert(index, name);
            }
        }
    }

    // One walk per declared identifier column, collected by index so each row gets its
    // own. A column the device does not implement returns nothing and contributes
    // nothing, which is the same tolerance a missing metric gets.
    let mut identifiers: BTreeMap<Vec<u32>, Vec<Identifier>> = BTreeMap::new();
    for source in &rule.creates.identifiers {
        let found = walk::walk(ctx.transport.as_ref(), target, &source.oid, &mut tuning)
            .await
            .map_err(PollError::Walk)?;
        for vb in &found {
            let Some(index) = walk::index_of(&vb.oid, &source.oid) else {
                continue;
            };
            if let Some(value) = identifier_value(source.kind, &vb.value) {
                identifiers
                    .entry(index)
                    .or_default()
                    .push(Identifier::new(source.kind, value));
            }
        }
    }

    ctx.devices
        .remember_names(task.device.resource, names.clone())
        .await;
    ctx.devices
        .remember_tuning(task.device.resource, tuning)
        .await;

    // Keyed off the names: a row with no name has nothing to be called and nothing to
    // match on across runs — see migration 0008 on why the name and not the index — so
    // it cannot become a resource. It still produces telemetry, labelled by index.
    let discovered = names
        .into_iter()
        .map(|(index, name)| DiscoveredChild {
            identifiers: identifiers.remove(&index).unwrap_or_default(),
            index,
            name,
        })
        .collect();

    Ok(Polled {
        discovered,
        ..Polled::default()
    })
}

/// Render a varbind as an identifier value, or `None` if it is not one.
///
/// Every identifier is a string by the time it reaches `resource_identifier`, and how a
/// value becomes that string depends on what kind it is: a MAC is six raw bytes and a
/// serial number is text, and both arrive as `OCTET STRING`.
fn identifier_value(kind: uops_core::IdentifierKind, value: &Value) -> Option<String> {
    let Value::Bytes(bytes) = value else {
        // An identifier that is not an OCTET STRING is not one this code knows how to
        // read. Skipped rather than guessed at — a number rendered as a serial would be
        // an identifier that matches the wrong device.
        return None;
    };

    if kind == uops_core::IdentifierKind::Mac {
        return mac(bytes);
    }
    let text = String::from_utf8_lossy(bytes).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Six bytes as `aa:bb:cc:dd:ee:ff`.
///
/// # Why an all-zero address is not an identifier
///
/// `ifPhysAddress` is all-zero on every interface that has no physical address — loopbacks,
/// tunnels, VLAN interfaces, the null interface. That is most of the rows on a router.
/// `resource_identifier` is unique on `(tenant_id, kind, value)`, so treating it as a MAC
/// would mean the first loopback in a tenant claims it and every other one silently
/// attaches to nothing. It is an absence, not an address.
fn mac(bytes: &[u8]) -> Option<String> {
    if bytes.len() != 6 || bytes.iter().all(|b| *b == 0) {
        return None;
    }
    Some(
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
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
