//! A poll, end to end, against simulated agents.
//!
//! No database and no network. What this covers is the join the binary exists to make:
//! a planned job becomes a request, the agent's answer becomes rows, and the rows are
//! labelled with the things a person reads on a graph. Each half is tested in its own
//! crate; nothing but this asserts that they line up — and the ways they can fail to are
//! quiet ones, like an interface counter that arrives with no name because discovery ran
//! and its result went nowhere.
//!
//! The agents are `uops_snmp::sim`, which is also how the failure modes get tested: a
//! device that never answers is a `Behaviour`, not a firewall rule somebody has to set
//! up.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use uops_core::{CredentialRef, IdentifierKind, ResourceId, SiteId, TenantId};
use uops_poll::plan::{Device, Work, plan};
use uops_poll::poller::Task;
use uops_profile::{Oid, Profile};
use uops_snmp::sim::{Agent, Behaviour, Fleet};
use uops_snmp::{Transport, Value};
use uops_store_ch::MetricRow;

use uops_poller::poll::{self, Context, Devices, PollError, Sink};

const ADDRESS: &str = "10.0.0.1:161";

/// A sink that keeps what it was given.
///
/// `std::sync::Mutex` rather than tokio's: nothing here is held across an await, and a
/// synchronous lock in a test is one fewer thing to get wrong.
#[derive(Debug, Default)]
struct Recorder {
    rows: Mutex<Vec<MetricRow>>,
    /// When set, every write fails with this. For the one property the real store's
    /// error path has that nothing else does.
    fails_with: Option<String>,
}

#[async_trait::async_trait]
impl Sink for Recorder {
    async fn write(&self, rows: &[MetricRow]) -> Result<(), String> {
        if let Some(problem) = &self.fails_with {
            return Err(problem.clone());
        }
        self.rows.lock().unwrap().extend_from_slice(rows);
        Ok(())
    }
}

impl Recorder {
    fn rows(&self) -> Vec<MetricRow> {
        self.rows.lock().unwrap().clone()
    }

    fn metric(&self, name: &str) -> Vec<MetricRow> {
        self.rows()
            .into_iter()
            .filter(|r| r.metric == name)
            .collect()
    }
}

fn generic() -> Profile {
    uops_profile::builtin::all()
        .expect("built-ins")
        .into_iter()
        .find(|p| p.id == "generic-snmp")
        .expect("generic-snmp")
}

fn device() -> Device {
    Device {
        tenant: TenantId::new(),
        resource: ResourceId::new(),
        site: SiteId::new(),
        address: ADDRESS.parse().unwrap(),
        credential: Some(CredentialRef::new()),
    }
}

/// The job of the given shape from the profile's plan.
///
/// Taken from `plan()` rather than hand-built: a test that constructs its own `Work` is
/// a test that keeps passing after the planner stops producing that shape.
fn task(device: &Device, profile: &Profile, wanted: fn(&Work) -> bool) -> Task {
    let job = plan(device, profile)
        .into_iter()
        .find(|j| wanted(&j.work))
        .expect("the planner must produce this shape of job");
    Task {
        key: 0,
        device: device.clone(),
        work: job.work,
    }
}

fn oid(s: &str) -> Oid {
    s.parse().expect("a test OID must parse")
}

/// An agent that implements what `generic-snmp` asks for.
fn healthy(interfaces: u32) -> Fleet {
    let mut agent = Agent::empty();
    agent.set(oid("1.3.6.1.2.1.1.3.0"), Value::Unsigned(123_456));
    agent.set(
        oid("1.3.6.1.2.1.1.2.0"),
        Value::ObjectId(oid("1.3.6.1.4.1.9.1.1")),
    );

    for index in 1..=interfaces {
        // ifName, which discovery reads.
        agent.set(
            oid("1.3.6.1.2.1.31.1.1.1.1").child(index),
            Value::Bytes(format!("Gi0/{index}").into_bytes()),
        );
        // The two 64-bit octet counters the profile polls.
        agent.set(
            oid("1.3.6.1.2.1.31.1.1.1.6").child(index),
            Value::Counter64(u64::from(index) * 1_000),
        );
        agent.set(
            oid("1.3.6.1.2.1.31.1.1.1.10").child(index),
            Value::Counter64(u64::from(index) * 2_000),
        );
        // ifInErrors / ifOutErrors.
        agent.set(
            oid("1.3.6.1.2.1.2.2.1.14").child(index),
            Value::Unsigned(u64::from(index)),
        );
        agent.set(oid("1.3.6.1.2.1.2.2.1.20").child(index), Value::Unsigned(0));
    }

    let mut fleet = Fleet::new();
    fleet.insert(ADDRESS.parse::<SocketAddr>().unwrap(), agent);
    fleet
}

fn context<'a>(
    transport: Arc<dyn Transport>,
    devices: &'a Devices,
    metrics: &'a Recorder,
    profile: &Profile,
) -> Context<'a, Recorder> {
    Context {
        transport,
        devices,
        metrics,
        discovery: profile.discovery.first().cloned(),
        observed_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn a_scalar_poll_becomes_a_row_with_the_profiles_name_and_unit() {
    let (profile, device) = (generic(), device());
    let devices = Devices::new();
    let recorder = Recorder::default();
    let transport: Arc<dyn Transport> = Arc::new(healthy(2));
    let ctx = context(transport, &devices, &recorder, &profile);

    let task = task(&device, &profile, |w| matches!(w, Work::Scalars { .. }));
    let written = poll::run(&task, &ctx)
        .await
        .expect("the poll must succeed")
        .rows;

    assert_eq!(written, 1, "generic-snmp has one device-scoped metric");
    let rows = recorder.metric("system.uptime");
    assert_eq!(rows.len(), 1);
    // The value, the unit and the subject: everything that makes the row findable.
    assert!((rows[0].value - 123_456.0).abs() < f64::EPSILON);
    assert_eq!(rows[0].unit, "s");
    assert_eq!(rows[0].tenant_id, device.tenant);
    assert_eq!(rows[0].resource_id, device.resource);
    assert_eq!(rows[0].site_id, device.site);
}

#[tokio::test]
async fn an_agent_answering_x_dot_zero_still_matches_the_profiles_x() {
    // The instance-zero convention, which is what a scalar request actually gets back.
    // The profile writes `1.3.6.1.2.1.1.3.0` because that is what the MIB document
    // shows; a profile that wrote `1.3.6.1.2.1.1.3` must work too, because both are
    // written in the wild and neither is wrong.
    let mut profile = generic();
    for metric in &mut profile.metrics {
        if metric.name == "system.uptime" {
            metric.oid = oid("1.3.6.1.2.1.1.3");
        }
    }

    let device = device();
    let devices = Devices::new();
    let recorder = Recorder::default();
    let transport: Arc<dyn Transport> = Arc::new(healthy(1));
    let ctx = context(transport, &devices, &recorder, &profile);

    let task = task(&device, &profile, |w| matches!(w, Work::Scalars { .. }));
    poll::run(&task, &ctx).await.expect("poll");
    assert_eq!(recorder.metric("system.uptime").len(), 1);
}

#[tokio::test]
async fn interface_counters_are_labelled_by_the_discovery_that_preceded_them() {
    // The join this file exists for. Discovery and the column walk are separate jobs on
    // separate intervals; between them the only thing carrying the names is the
    // per-device memory, and a poller that discovered into a value it then dropped would
    // produce rows labelled by index forever — which looks like working software right
    // up until somebody opens a graph.
    let (profile, device) = (generic(), device());
    let devices = Devices::new();
    let recorder = Recorder::default();
    let transport: Arc<dyn Transport> = Arc::new(healthy(3));
    let ctx = context(Arc::clone(&transport), &devices, &recorder, &profile);

    let discovery = task(&device, &profile, |w| matches!(w, Work::Discovery { .. }));
    let found = poll::run(&discovery, &ctx).await.expect("discovery").rows;
    assert_eq!(found, 0, "discovery writes no metric rows");

    let columns = task(&device, &profile, |w| {
        matches!(w, Work::InterfaceColumns { .. })
    });
    let written = poll::run(&columns, &ctx).await.expect("columns").rows;
    // Four interface metrics × three interfaces.
    assert_eq!(written, 12);

    let received = recorder.metric("network.io.receive");
    assert_eq!(received.len(), 3);
    // Rendered as strings so the tuples can be sorted: f64 is not Ord, and a row's value
    // here is an exact small integer from the fixture rather than the result of any
    // arithmetic, so there is nothing to lose to formatting.
    let mut named: Vec<(String, String, String)> = received
        .iter()
        .map(|r| {
            (
                r.labels
                    .get("network.interface.index")
                    .cloned()
                    .unwrap_or_default(),
                r.labels
                    .get("network.interface.name")
                    .cloned()
                    .unwrap_or_default(),
                r.value.to_string(),
            )
        })
        .collect();
    named.sort();
    assert_eq!(
        named,
        vec![
            ("1".to_owned(), "Gi0/1".to_owned(), "1000".to_owned()),
            ("2".to_owned(), "Gi0/2".to_owned(), "2000".to_owned()),
            ("3".to_owned(), "Gi0/3".to_owned(), "3000".to_owned()),
        ]
    );
}

#[tokio::test]
async fn a_column_walk_before_any_discovery_is_labelled_by_index_not_dropped() {
    // The negative control for the test above, and a real case: the first poll of a
    // device happens before its first discovery. A counter from an interface whose name
    // is not known yet is still a real measurement.
    let (profile, device) = (generic(), device());
    let devices = Devices::new();
    let recorder = Recorder::default();
    let transport: Arc<dyn Transport> = Arc::new(healthy(2));
    let ctx = context(transport, &devices, &recorder, &profile);

    let columns = task(&device, &profile, |w| {
        matches!(w, Work::InterfaceColumns { .. })
    });
    let written = poll::run(&columns, &ctx).await.expect("columns").rows;
    assert_eq!(written, 8);

    let received = recorder.metric("network.io.receive");
    assert_eq!(received.len(), 2);
    for row in &received {
        assert!(row.labels.contains_key("network.interface.index"));
        assert!(
            !row.labels.contains_key("network.interface.name"),
            "there was nothing to name it with"
        );
    }
}

#[tokio::test]
async fn a_metric_the_agent_does_not_implement_is_skipped_rather_than_invented() {
    // A profile is written for a family of devices and any one of them may not implement
    // every OID in it. The wrong behaviours are both worse than a missing row: failing
    // the whole poll, or writing a zero that looks like a measurement.
    let (profile, device) = (generic(), device());
    let devices = Devices::new();
    let recorder = Recorder::default();

    let mut fleet = Fleet::new();
    let mut agent = Agent::empty();
    // An agent with interfaces but no sysUpTime.
    agent.set(
        oid("1.3.6.1.2.1.31.1.1.1.6").child(1),
        Value::Counter64(500),
    );
    fleet.insert(ADDRESS.parse::<SocketAddr>().unwrap(), agent);
    let transport: Arc<dyn Transport> = Arc::new(fleet);
    let ctx = context(transport, &devices, &recorder, &profile);

    let task = task(&device, &profile, |w| matches!(w, Work::Scalars { .. }));
    let written = poll::run(&task, &ctx)
        .await
        .expect("the poll must not fail")
        .rows;
    assert_eq!(written, 0);
    assert!(recorder.rows().is_empty());
}

#[tokio::test]
async fn a_dead_device_fails_the_task_and_writes_nothing() {
    let (profile, device) = (generic(), device());
    let devices = Devices::new();
    let recorder = Recorder::default();

    let mut fleet = Fleet::new();
    fleet.insert(
        ADDRESS.parse::<SocketAddr>().unwrap(),
        Agent::empty().behaving(Behaviour::Silent),
    );
    let transport: Arc<dyn Transport> = Arc::new(fleet);
    let ctx = context(transport, &devices, &recorder, &profile);

    let task = task(&device, &profile, |w| matches!(w, Work::Scalars { .. }));
    let err = poll::run(&task, &ctx).await.unwrap_err();
    assert!(
        matches!(err, PollError::Transport(_)),
        "a dead device is a transport failure, not a store one: {err:?}"
    );
    assert!(recorder.rows().is_empty());
}

#[tokio::test]
async fn a_store_failure_is_not_reported_as_a_device_failure() {
    // The distinction that decides who gets paged. A poller whose ClickHouse is down
    // reports every device as failing, and an operator reading that goes looking at the
    // network — which is the one place the problem is not.
    let (profile, device) = (generic(), device());
    let devices = Devices::new();
    let recorder = Recorder {
        fails_with: Some("clickhouse is unreachable".to_owned()),
        ..Recorder::default()
    };
    let transport: Arc<dyn Transport> = Arc::new(healthy(1));
    let ctx = context(transport, &devices, &recorder, &profile);

    let task = task(&device, &profile, |w| matches!(w, Work::Scalars { .. }));
    let err = poll::run(&task, &ctx).await.unwrap_err();
    assert!(matches!(err, PollError::Store(_)), "{err:?}");
    assert!(err.to_string().contains("could not be stored"));
}

#[tokio::test]
async fn an_availability_check_is_counted_as_unsupported_rather_than_passing() {
    // ICMP needs a raw socket and is not implemented. The honest failure is the one that
    // says so: a check that silently "succeeded" would put every device permanently up,
    // which is worse than no availability at all.
    let (profile, device) = (generic(), device());
    let devices = Devices::new();
    let recorder = Recorder::default();
    let transport: Arc<dyn Transport> = Arc::new(healthy(1));
    let ctx = context(transport, &devices, &recorder, &profile);

    let task = task(&device, &profile, |w| {
        matches!(w, Work::Availability { .. })
    });
    let err = poll::run(&task, &ctx).await.unwrap_err();
    assert!(matches!(err, PollError::Unsupported(_)), "{err:?}");
}

#[tokio::test]
async fn a_sysobjectid_is_read_from_the_device_that_has_one() {
    // What makes profile resolution work on the second poll: a device polled under
    // generic-snmp answers its object id, and the next reload picks the right profile.
    let transport = healthy(1);
    let target = uops_snmp::Target {
        address: ADDRESS.parse().unwrap(),
    };
    assert_eq!(
        poll::sysobjectid(&transport, &target).await.expect("ask"),
        Some("1.3.6.1.4.1.9.1.1".to_owned())
    );

    // An agent that does not answer it is not an error — most of the fleet will be a
    // device that answers everything else.
    let mut bare = Fleet::new();
    bare.insert(ADDRESS.parse::<SocketAddr>().unwrap(), Agent::empty());
    assert_eq!(poll::sysobjectid(&bare, &target).await.expect("ask"), None);
}

#[tokio::test]
async fn a_devices_tuning_survives_between_polls() {
    // The remembered max-repetitions. Against an agent that refuses anything above four,
    // the first walk pays for the halving and every later one does not — which over a
    // fleet is the difference between one wasted request per device and one per poll.
    let (profile, device) = (generic(), device());
    let devices = Devices::new();
    let recorder = Recorder::default();

    let mut fleet = Fleet::new();
    let mut agent = Agent::empty();
    for index in 1..=4u32 {
        agent.set(
            oid("1.3.6.1.2.1.31.1.1.1.6").child(index),
            Value::Counter64(u64::from(index)),
        );
    }
    fleet.insert(
        ADDRESS.parse::<SocketAddr>().unwrap(),
        agent.behaving(Behaviour::TooBigAbove(4)),
    );
    let transport: Arc<dyn Transport> = Arc::new(fleet);
    let ctx = context(transport, &devices, &recorder, &profile);

    let columns = task(&device, &profile, |w| {
        matches!(w, Work::InterfaceColumns { .. })
    });
    poll::run(&columns, &ctx).await.expect("first poll");

    let (tuning, _) = devices.snapshot(device.resource).await;
    assert!(
        tuning.current().get() <= 4,
        "the halving must be remembered, not repeated: {tuning:?}"
    );
}

#[tokio::test]
async fn a_discovery_walk_reports_a_child_for_every_named_row() {
    // What the store turns into resources and edges. Names and indexes both: the name is
    // what a child is matched on across runs, the index is what joins a sample's label
    // back to it.
    let (profile, device) = (generic(), device());
    let devices = Devices::new();
    let recorder = Recorder::default();
    let transport: Arc<dyn Transport> = Arc::new(healthy(3));
    let ctx = context(transport, &devices, &recorder, &profile);

    let task = task(&device, &profile, |w| matches!(w, Work::Discovery { .. }));
    let polled = poll::run(&task, &ctx).await.expect("discovery");

    assert_eq!(polled.rows, 0, "discovery writes no metric rows");
    let found: Vec<(&str, Vec<u32>)> = polled
        .discovered
        .iter()
        .map(|c| (c.name.as_str(), c.index.clone()))
        .collect();
    assert_eq!(
        found,
        vec![("Gi0/1", vec![1]), ("Gi0/2", vec![2]), ("Gi0/3", vec![3]),]
    );
}

#[tokio::test]
async fn an_interfaces_mac_is_read_from_the_column_the_profile_names() {
    // `generic-snmp` declares ifPhysAddress as a `mac` identifier. It is how a flow
    // record or an LLDP neighbour, which arrive with a MAC and nothing else, later reach
    // this interface.
    let (profile, device) = (generic(), device());
    let devices = Devices::new();
    let recorder = Recorder::default();

    let mut agent = Agent::empty();
    for index in 1..=2u32 {
        agent.set(
            oid("1.3.6.1.2.1.31.1.1.1.1").child(index),
            Value::Bytes(format!("Gi0/{index}").into_bytes()),
        );
        // ifPhysAddress: six raw bytes, which is what a MAC is on the wire.
        agent.set(
            oid("1.3.6.1.2.1.2.2.1.6").child(index),
            Value::Bytes(vec![
                0x02,
                0x00,
                0x00,
                0x00,
                0x00,
                u8::try_from(index).unwrap(),
            ]),
        );
    }
    let mut fleet = Fleet::new();
    fleet.insert(ADDRESS.parse::<SocketAddr>().unwrap(), agent);
    let transport: Arc<dyn Transport> = Arc::new(fleet);
    let ctx = context(transport, &devices, &recorder, &profile);

    let task = task(&device, &profile, |w| matches!(w, Work::Discovery { .. }));
    let polled = poll::run(&task, &ctx).await.expect("discovery");

    let macs: Vec<(&str, &str)> = polled
        .discovered
        .iter()
        .filter_map(|c| {
            c.identifiers
                .first()
                .map(|i| (c.name.as_str(), i.value.as_str()))
        })
        .collect();
    assert_eq!(
        macs,
        vec![
            ("Gi0/1", "02:00:00:00:00:01"),
            ("Gi0/2", "02:00:00:00:00:02")
        ]
    );
    assert!(
        polled
            .discovered
            .iter()
            .all(|c| c.identifiers.iter().all(|i| i.kind == IdentifierKind::Mac)),
        "the kind comes from the profile, not from guessing at the value"
    );
}

#[tokio::test]
async fn an_all_zero_phys_address_is_not_treated_as_a_mac() {
    // Most rows on a router. Loopbacks, tunnels, VLAN interfaces and the null interface
    // all report six zero bytes, because they have no physical address.
    //
    // `resource_identifier` is unique on (tenant_id, kind, value), so treating that as a
    // MAC would mean the first loopback in a tenant claims `00:00:00:00:00:00` and every
    // other one silently attaches to nothing — an identifier pointing at an arbitrary
    // interface, which is worse than no identifier.
    let (profile, device) = (generic(), device());
    let devices = Devices::new();
    let recorder = Recorder::default();

    let mut agent = Agent::empty();
    agent.set(
        oid("1.3.6.1.2.1.31.1.1.1.1").child(1),
        Value::Bytes(b"Loopback0".to_vec()),
    );
    agent.set(
        oid("1.3.6.1.2.1.2.2.1.6").child(1),
        Value::Bytes(vec![0; 6]),
    );
    // And a length that is not six: some agents return an empty string for the same idea.
    agent.set(
        oid("1.3.6.1.2.1.31.1.1.1.1").child(2),
        Value::Bytes(b"Tunnel1".to_vec()),
    );
    agent.set(
        oid("1.3.6.1.2.1.2.2.1.6").child(2),
        Value::Bytes(Vec::new()),
    );

    let mut fleet = Fleet::new();
    fleet.insert(ADDRESS.parse::<SocketAddr>().unwrap(), agent);
    let transport: Arc<dyn Transport> = Arc::new(fleet);
    let ctx = context(transport, &devices, &recorder, &profile);

    let task = task(&device, &profile, |w| matches!(w, Work::Discovery { .. }));
    let polled = poll::run(&task, &ctx).await.expect("discovery");

    assert_eq!(
        polled.discovered.len(),
        2,
        "both interfaces are still found"
    );
    for child in &polled.discovered {
        assert!(
            child.identifiers.is_empty(),
            "{} was given an identifier it does not have: {:?}",
            child.name,
            child.identifiers
        );
    }
}

#[tokio::test]
async fn a_row_with_no_name_produces_telemetry_but_not_a_resource() {
    // An interface whose ifName the agent does not answer. It cannot become a resource —
    // the name is what it would be matched on across runs, and a child keyed on nothing
    // would be re-created every fifteen minutes. Its counters are still real, and are
    // still written, labelled by index.
    let (profile, device) = (generic(), device());
    let devices = Devices::new();
    let recorder = Recorder::default();

    let mut agent = Agent::empty();
    agent.set(
        oid("1.3.6.1.2.1.31.1.1.1.1").child(1),
        Value::Bytes(b"Gi0/1".to_vec()),
    );
    // Index 2 has a counter and no name.
    agent.set(oid("1.3.6.1.2.1.31.1.1.1.6").child(1), Value::Counter64(10));
    agent.set(oid("1.3.6.1.2.1.31.1.1.1.6").child(2), Value::Counter64(20));
    let mut fleet = Fleet::new();
    fleet.insert(ADDRESS.parse::<SocketAddr>().unwrap(), agent);
    let transport: Arc<dyn Transport> = Arc::new(fleet);
    let ctx = context(transport, &devices, &recorder, &profile);

    let discovery = task(&device, &profile, |w| matches!(w, Work::Discovery { .. }));
    let polled = poll::run(&discovery, &ctx).await.expect("discovery");
    assert_eq!(
        polled.discovered.len(),
        1,
        "only the named row can become a resource"
    );

    let columns = task(&device, &profile, |w| {
        matches!(w, Work::InterfaceColumns { .. })
    });
    poll::run(&columns, &ctx).await.expect("columns");
    assert_eq!(
        recorder.metric("network.io.receive").len(),
        2,
        "both interfaces' counters are real measurements"
    );
}
