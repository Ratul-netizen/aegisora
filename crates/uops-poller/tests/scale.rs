//! SPEC §M2's first acceptance criterion, measured through the binary.
//!
//! > 1 000 simulated SNMP agents polled at 60s with p95 poll latency < 5 s and no missed
//! > cycles
//!
//! `uops-poll`'s own `fleet.rs` measures this too, and measures something narrower: the
//! executor and the walk, called directly. Everything the binary adds is outside that
//! measurement — resolving a credential, matching readings to metrics, labelling them,
//! and writing the rows. Those are exactly the parts that scale with the *fleet* rather
//! than with one device, and a criterion that only ever held for the library underneath
//! is not the criterion SPEC states.
//!
//! So this drives `run::tick_once` — the same function `serve` calls once a second —
//! over a schedule built by `Schedule::reload`, against a thousand simulated agents.
//!
//! # What is simulated and what is not
//!
//! The agents are, because SPEC says *simulated*: a thousand real ones is a room full of
//! hardware, and the failure modes worth measuring — an agent that never answers — are
//! not something real equipment does on request. Everything else is real, including the
//! `ClickHouse` insert, because a poller that keeps up until it has to store anything
//! has not been measured.
//!
//! ```bash
//! CLICKHOUSE_DB=uops CLICKHOUSE_USER=uops CLICKHOUSE_PASSWORD=uops \
//!   cargo test -p uops-poller --test scale --release -- --nocapture
//! ```
//!
//! Run in release. A debug build measures `rustc -O0`, not the design.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uops_core::{CredentialRef, ResourceId, SiteId, TenantId};
use uops_poll::plan::Device;
use uops_poll::poller::Schedule;
use uops_poll::{Executor, Limits};
use uops_profile::{Oid, Profile};
use uops_snmp::bulk::Repetitions;
use uops_snmp::sim::{Agent, Behaviour, Fleet};
use uops_snmp::{Target, Transport, TransportError, VarBind};
use uops_store_ch::{ChClient, ChConfig, ChStore};
use uops_store_pg::{Config as PgConfig, PgStore};

use uops_poller::credentials::{CredentialProblem, TransportSource};
use uops_poller::run::{self, Runner};

/// How many agents. SPEC's number.
const FLEET: u16 = 1_000;

/// How many interfaces each agent has. A 24-port access switch, which is the shape of
/// the fleet an MSP actually polls — and it is the interface columns, not the scalars,
/// that make a poll expensive.
const PORTS: u32 = 24;

/// SPEC's ceiling.
const P95_BUDGET: Duration = Duration::from_secs(5);

fn address(n: u16) -> SocketAddr {
    format!("127.0.0.1:{}", 30_000 + n)
        .parse()
        .expect("a test address")
}

fn oid(s: &str) -> Oid {
    s.parse().expect("a constant OID")
}

/// An agent that implements what `generic-snmp` asks for, on `PORTS` interfaces.
fn agent() -> Agent {
    let mut agent = Agent::empty();
    agent.set(
        oid("1.3.6.1.2.1.1.3.0"),
        uops_snmp::Value::Unsigned(123_456),
    );
    agent.set(
        oid("1.3.6.1.2.1.1.2.0"),
        uops_snmp::Value::ObjectId(oid("1.3.6.1.4.1.9.1.1")),
    );
    for index in 1..=PORTS {
        agent.set(
            oid("1.3.6.1.2.1.31.1.1.1.1").child(index),
            uops_snmp::Value::Bytes(format!("Gi0/{index}").into_bytes()),
        );
        agent.set(
            oid("1.3.6.1.2.1.2.2.1.6").child(index),
            uops_snmp::Value::Bytes(vec![
                0x02,
                0x00,
                0x00,
                u8::try_from(index / 256).unwrap_or(0),
                u8::try_from(index % 256).unwrap_or(0),
                0x01,
            ]),
        );
        for column in ["1.3.6.1.2.1.31.1.1.1.6", "1.3.6.1.2.1.31.1.1.1.10"] {
            agent.set(
                oid(column).child(index),
                uops_snmp::Value::Counter64(u64::from(index) * 1_000),
            );
        }
        for column in ["1.3.6.1.2.1.2.2.1.14", "1.3.6.1.2.1.2.2.1.20"] {
            agent.set(oid(column).child(index), uops_snmp::Value::Unsigned(0));
        }
    }
    agent
}

/// A transport source that hands every device the same simulated fleet.
///
/// The credential is not consulted, deliberately: what this measures is the *loop*, and
/// a vault round trip per device would measure the vault. `Transports` covers the
/// credential path, and `tests/live.rs` covers it end to end against a real agent.
struct Simulated {
    fleet: Arc<Fleet>,
}

#[async_trait::async_trait]
impl Transport for Simulated {
    async fn get_bulk(
        &self,
        target: &Target,
        after: &Oid,
        max_repetitions: Repetitions,
    ) -> Result<Vec<VarBind>, TransportError> {
        self.fleet.get_bulk(target, after, max_repetitions).await
    }

    async fn get_scalars(
        &self,
        target: &Target,
        oids: &[Oid],
    ) -> Result<Vec<VarBind>, TransportError> {
        self.fleet.get_scalars(target, oids).await
    }
}

struct SimSource {
    transport: Arc<dyn Transport>,
}

impl TransportSource for SimSource {
    fn for_device(
        &self,
        _tenant: TenantId,
        _resource: ResourceId,
        _credential: Option<CredentialRef>,
        _timeout: Duration,
    ) -> Result<Arc<dyn Transport>, CredentialProblem> {
        Ok(Arc::clone(&self.transport))
    }

    fn retry_failures(&self) -> usize {
        0
    }
}

async fn stores() -> (PgStore, ChStore) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://uops:uops@localhost:5432/uops".into());
    let pg = PgStore::connect(&PgConfig {
        url,
        ..PgConfig::default()
    })
    .await
    .expect("connect to PostgreSQL");
    let ch = ChStore::new(ChClient::new(ChConfig::from_env()));
    (pg, ch)
}

fn generic() -> Profile {
    uops_profile::builtin::all()
        .expect("built-ins")
        .into_iter()
        .find(|p| p.id == "generic-snmp")
        .expect("generic-snmp")
}

/// `generic-snmp` with its availability check removed.
///
/// The check works — see `check.rs` — and it is left out because of where these agents
/// live. A simulated fleet is a thousand entries in a `HashMap` behind a thousand
/// loopback addresses, so an ICMP check here would ping `127.0.0.1` a thousand times and
/// measure the loopback interface rather than the poller. A number that good would be
/// meaningless, and a number that good in a scale test is how a regression hides.
///
/// It makes the measurement optimistic by a knowable amount: one check per device per
/// 30 seconds, about 2 000 over the 120 seconds measured here against roughly 6 000 SNMP
/// jobs. Said out loud rather than left for a reader to notice that the job count does
/// not match the profile.
fn measured_profile() -> Profile {
    let mut p = generic();
    p.availability.clear();
    p
}

/// Remove what a run wrote.
///
/// Four thousand polls of a 24-port switch is around 400 000 metric rows, and they would
/// sit there until the 30-day TTL took them. Not a tidiness point: an unswept
/// development database is what turned one poller reload into five thousand round trips
/// and cost a day to find. See STATUS.
async fn clean_up(tenant: TenantId) {
    let client = ChClient::new(ChConfig::from_env());
    // A lightweight DELETE, which ClickHouse marks rows deleted rather than rewriting
    // parts. Slow to reclaim disk and instantly correct for reads, which is the right
    // trade for a test fixture.
    let _ = client
        .run(
            "DELETE FROM metrics WHERE tenant_id = {tenant:UUID}",
            &[("tenant", tenant.into_uuid().to_string())],
        )
        .await;
}

/// What one run measured.
struct Measured {
    p95: Duration,
    worst: Duration,
    ok: usize,
    /// A poll that returned an error — the transport, the walk, or the store.
    failed: usize,
    /// A poll cut off by the per-device time budget. Counted apart from `failed`
    /// because they say different things: one is a device or a dependency saying no,
    /// the other is this poller not keeping up.
    budget: usize,
    ticks_over_budget: usize,
}

/// Poll `seconds` of schedule, timing every tick that had work.
///
/// A tick is what the criterion is about. `run_tick` dispatches and waits for that
/// second's batch; how long that takes, at the 95th percentile, is the poll latency.
async fn measure(dead: usize, seconds: u64) -> Measured {
    let (pg, ch) = stores().await;
    let tenant = TenantId::new();
    let site = SiteId::new();
    let profile = measured_profile();

    let mut fleet = Fleet::new();
    for n in 0..FLEET {
        let a = if usize::from(n) < dead {
            // The expensive kind of dead: silence, not a closed port, so the poll pays
            // its whole timeout.
            Agent::empty().behaving(Behaviour::Silent)
        } else {
            agent()
        };
        fleet.insert(address(n), a);
    }

    let devices: Vec<(Device, Profile)> = (0..FLEET)
        .map(|n| {
            (
                Device {
                    tenant,
                    resource: ResourceId::new(),
                    site,
                    address: address(n),
                    credential: Some(CredentialRef::new()),
                },
                profile.clone(),
            )
        })
        .collect();

    let source = SimSource {
        transport: Arc::new(Simulated {
            fleet: Arc::new(fleet),
        }),
    };
    let runner = Arc::new(Runner::new(
        pg,
        ch,
        Arc::new(source),
        // A device's own budget. Below the tick, so a slow device cannot still be in
        // flight when its next poll comes due — see config.rs.
        Duration::from_secs(1),
    ));

    // The schedule the loop would have built, without going through PostgreSQL: what is
    // being measured is the polling, and seeding a thousand devices and their
    // credentials would measure the seeding.
    //
    // `Runner::load` rather than `Schedule::reload` — it does both halves, which is the
    // point of it. The first version of this test called `Schedule::reload` directly and
    // measured a fleet whose every discovery job failed.
    let mut schedule = Schedule::new();
    let (added, _) = runner.load(&mut schedule, &devices).await;
    assert_eq!(added, usize::from(FLEET));

    let executor = Executor::new(Limits::default());
    let mut due = Vec::new();
    let mut elapsed: Vec<Duration> = Vec::new();
    let mut ok = 0;
    let mut failed = 0;
    let mut budget = 0;
    let mut ticks_over_budget = 0;

    for _ in 0..seconds {
        let started = Instant::now();
        let report = run::tick_once(&runner, &executor, &mut schedule, &mut due).await;
        let took = started.elapsed();
        if report.due == 0 {
            continue;
        }
        elapsed.push(took);
        if took > P95_BUDGET {
            ticks_over_budget += 1;
        }
        ok += report.ok;
        failed += report.failed;
        budget += report.budget_exhausted;
    }

    clean_up(tenant).await;

    assert!(!elapsed.is_empty(), "no tick had any work in it");
    let p95 = uops_poll::executor::percentile(&mut elapsed.clone(), 95);
    let worst = elapsed.iter().copied().max().unwrap_or_default();

    Measured {
        p95,
        worst,
        ok,
        failed,
        budget,
        ticks_over_budget,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "measures a 1 000-device fleet; run explicitly, in release"]
async fn a_thousand_agents_are_polled_within_the_budget() {
    // SPEC's criterion, through the binary's own loop.
    //
    // 120 seconds of schedule: generic-snmp's metrics are on a 60-second interval, so
    // two full cycles. Jitter spreads each device across its interval — that is the
    // whole point of the wheel — so a cycle is a couple of dozen devices a second rather
    // than a thousand at once, and a design without it would fail this on the first tick.
    let m = measure(0, 120).await;

    println!(
        "1 000 agents, {PORTS} ports each: p95 {:?}, worst {:?},          {} polls ok, {} failed, {} out of time",
        m.p95, m.worst, m.ok, m.failed, m.budget
    );

    assert_eq!(m.failed, 0, "a healthy fleet must not fail a poll");
    assert!(
        m.ok > 3_000,
        "three jobs per device over two 60-second cycles is about 6 000 polls;          {} is too few for this to have measured a fleet",
        m.ok
    );
    assert!(
        m.p95 < P95_BUDGET,
        "p95 poll latency {:?} exceeds SPEC's {P95_BUDGET:?}",
        m.p95
    );
    // "No missed cycles": a tick that took longer than the budget is a tick whose work
    // was still running when the next one came due.
    assert_eq!(
        m.ticks_over_budget, 0,
        "{} tick(s) ran past the budget; the schedule is slipping",
        m.ticks_over_budget
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "measures a 1 000-device fleet; run explicitly, in release"]
async fn a_hundred_dead_devices_do_not_delay_the_rest() {
    // SPEC's sixth criterion — *measured, not assumed* — at the loop level rather than
    // the executor's. `uops-poll`'s fleet test proves the executor does not serialise;
    // this proves nothing above it undoes that, which is a different claim: a lock held
    // across an await in the loop would serialise the fleet with the executor entirely
    // innocent.
    let healthy = measure(0, 120).await;
    let degraded = measure(100, 120).await;

    println!(
        "healthy: p95 {:?}; with 100 dead: p95 {:?}, {} failed, {} out of time",
        healthy.p95, degraded.p95, degraded.failed, degraded.budget
    );

    assert!(
        degraded.failed + degraded.budget > 0,
        "the dead devices must actually have failed, or this measures nothing"
    );
    assert!(
        degraded.p95 < P95_BUDGET,
        "p95 {:?} with 100 dead devices exceeds SPEC's {P95_BUDGET:?}",
        degraded.p95
    );
}

/// Sanity check on the fixture itself, cheap enough to run in the normal suite.
///
/// The measurement above is `#[ignore]`d because it takes minutes and needs a release
/// build. That makes it exactly the kind of test that quietly stops compiling, so this
/// exercises the same fixtures at a size that costs nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fixture_polls_a_simulated_device() {
    let a = agent();
    assert!(
        a.len() > usize::try_from(PORTS).unwrap_or(0) * 5,
        "the agent must implement every column generic-snmp reads"
    );

    let mut fleet = Fleet::new();
    fleet.insert(address(0), agent());
    let source = SimSource {
        transport: Arc::new(Simulated {
            fleet: Arc::new(fleet),
        }),
    };

    let device = Device {
        tenant: TenantId::new(),
        resource: ResourceId::new(),
        site: SiteId::new(),
        address: address(0),
        credential: Some(CredentialRef::new()),
    };
    let transport = source
        .for_device(
            device.tenant,
            device.resource,
            device.credential,
            Duration::from_secs(1),
        )
        .expect("the simulated source hands out a transport");

    let mut tuning = uops_snmp::bulk::Tuning::default();
    let rows = uops_snmp::walk::walk(
        transport.as_ref(),
        &Target {
            address: device.address,
        },
        &oid("1.3.6.1.2.1.31.1.1.1.1"),
        &mut tuning,
    )
    .await
    .expect("walk ifName");
    assert_eq!(
        rows.len(),
        usize::try_from(PORTS).unwrap_or(0),
        "every port must be walkable"
    );

    // And the schedule the measurement builds is the shape it expects.
    let mut schedule = Schedule::new();
    let (added, _) = schedule.reload(&[(device, generic())]);
    assert_eq!(added, 1);
    assert_eq!(
        schedule.live_jobs(),
        4,
        "generic-snmp plans scalars, interface columns, discovery and availability"
    );
}
