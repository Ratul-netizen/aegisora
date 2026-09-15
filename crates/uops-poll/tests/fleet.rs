//! SPEC §M2's poller acceptance criteria, measured.
//!
//! > 1 000 simulated SNMP agents polled at 60s with p95 poll latency < 5 s and no missed
//! > cycles
//!
//! > Dead device does not delay polling of healthy devices (**measured, not assumed**)
//!
//! Those two words are the reason this file exists. "A dead device does not delay
//! healthy ones" is easy to believe of any design with a timeout in it, and easy to be
//! wrong about: a global semaphore sized smaller than the number of dead devices turns
//! every slot into a ten-second wait, and nothing in the code looks different.
//!
//! So the healthy devices' latency is measured with dead ones in the fleet, and compared
//! against the same fleet with none. If the design is wrong, the numbers say so.

use std::sync::Arc;
use std::time::Duration;

use uops_poll::executor::{Executor, Limits, Outcome, percentile};
use uops_profile::Oid;
use uops_snmp::bulk::Tuning;
use uops_snmp::sim::{Agent, Behaviour, Fleet};
use uops_snmp::transport::Target;
use uops_snmp::walk;

fn if_table() -> Oid {
    "1.3.6.1.2.1.2.2.1".parse().unwrap()
}

fn address(n: u16) -> std::net::SocketAddr {
    format!("127.0.0.1:{}", 30_000 + n).parse().unwrap()
}

/// How a device in the fleet behaves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Healthy,
    /// Answers, slowly. A loaded switch.
    Slow,
    /// Never answers, and eats the whole timeout doing it. The expensive kind of dead:
    /// an agent behind a silently dropping firewall rule, not a closed port.
    Dead,
}

fn build(kinds: &[Kind]) -> Fleet {
    let mut fleet = Fleet::new();
    for (n, kind) in kinds.iter().enumerate() {
        let n = u16::try_from(n).expect("fleet fits in u16");
        let agent = Agent::with_table(&if_table(), 4, 12);
        let agent = match kind {
            Kind::Healthy => agent,
            Kind::Slow => agent.slow(Duration::from_millis(20)),
            // Silent *and* slow: the transport reports a timeout only after the device
            // has held the slot. A dead device that fails instantly is not the case
            // worth testing.
            Kind::Dead => agent
                .behaving(Behaviour::Silent)
                .slow(Duration::from_millis(400)),
        };
        fleet.insert(address(n), agent);
    }
    fleet
}

/// Poll every device once, concurrently, under `limits`. Returns the healthy devices'
/// latencies and how each device ended.
async fn round(fleet: Arc<Fleet>, kinds: &[Kind], limits: Limits) -> (Vec<Duration>, Vec<Outcome>) {
    let executor = Arc::new(Executor::new(limits));
    let mut tasks = Vec::with_capacity(kinds.len());

    for (n, kind) in kinds.iter().enumerate() {
        let n = u16::try_from(n).unwrap();
        let fleet = Arc::clone(&fleet);
        let executor = Arc::clone(&executor);
        let kind = *kind;
        tasks.push(tokio::spawn(async move {
            let done = executor
                .run(n, move |_device| async move {
                    let target = Target {
                        address: address(n),
                    };
                    let mut tuning = Tuning::default();
                    walk::walk(&*fleet, &target, &if_table(), &mut tuning)
                        .await
                        .map(drop)
                })
                .await;
            (kind, done)
        }));
    }

    let mut healthy = Vec::new();
    let mut outcomes = Vec::new();
    for t in tasks {
        let (kind, done) = t.await.unwrap();
        outcomes.push(done.outcome);
        if kind == Kind::Healthy {
            // total(), not elapsed(): waiting for a slot is exactly the delay SPEC's
            // criterion is about, and measuring only the device's own time makes a
            // strictly serialised fleet look identical to a concurrent one.
            healthy.push(done.total());
        }
    }
    (healthy, outcomes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_thousand_agents_poll_well_inside_the_cycle() {
    // SPEC: 1 000 agents at 60s, p95 poll latency < 5 s, no missed cycles. The
    // simulator answers in microseconds, so this is not a measurement of SNMP — it is a
    // measurement of the executor's own overhead at that scale, which is the part this
    // code is responsible for.
    let kinds = vec![Kind::Healthy; 1_000];
    let fleet = Arc::new(build(&kinds));

    let started = std::time::Instant::now();
    let (mut healthy, outcomes) = round(fleet, &kinds, Limits::default()).await;
    let wall = started.elapsed();

    assert_eq!(outcomes.len(), 1_000);
    assert!(
        outcomes.iter().all(|o| *o == Outcome::Ok),
        "every healthy agent must poll cleanly"
    );

    let p95 = percentile(&mut healthy, 95);
    assert!(
        p95 < Duration::from_secs(5),
        "p95 poll latency was {p95:?}, over SPEC's 5s"
    );
    assert!(
        wall < Duration::from_secs(60),
        "a whole cycle took {wall:?}; 1 000 devices must fit in 60s"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dead_devices_do_not_delay_healthy_ones() {
    // The criterion SPEC says to measure rather than assume.
    //
    // 100 devices, a tenth of them dead and each holding its slot for 400 ms. The
    // healthy ones are measured twice: in that fleet, and in an all-healthy one. If
    // dead devices were serialising the fleet, the first number would be several times
    // the second.
    let mut mixed = vec![Kind::Healthy; 100];
    for slot in mixed.iter_mut().step_by(10) {
        *slot = Kind::Dead;
    }
    let all_healthy = vec![Kind::Healthy; 100];

    let limits = Limits {
        global: 32,
        per_device: 1,
        device_budget: Duration::from_secs(2),
    };

    let (mut with_dead, outcomes) = round(Arc::new(build(&mixed)), &mixed, limits).await;
    let (mut without, _) = round(Arc::new(build(&all_healthy)), &all_healthy, limits).await;

    // The dead ones did fail, or this is measuring nothing.
    let failures = outcomes.iter().filter(|o| **o != Outcome::Ok).count();
    assert_eq!(failures, 10, "the dead devices must actually have failed");

    let p95_with = percentile(&mut with_dead, 95);
    let p95_without = percentile(&mut without, 95);

    // The healthy devices must not have waited on the dead ones. The bound is generous
    // — this runs on a shared CI runner — and still far below the 400 ms a single dead
    // device holds its slot for, which is what serialisation would look like.
    assert!(
        p95_with < Duration::from_millis(200),
        "healthy p95 was {p95_with:?} with dead devices present, {p95_without:?} without"
    );
    assert!(
        p95_with < p95_without + Duration::from_millis(150),
        "dead devices added {p95_with:?} - {p95_without:?} to healthy latency"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_device_is_cut_off_at_its_budget_not_at_its_own_pace() {
    // One device, hanging for far longer than its budget. Without the budget this is
    // the poll that never returns and the slot that never frees.
    let mut fleet = Fleet::new();
    fleet.insert(
        address(0),
        Agent::with_table(&if_table(), 4, 12)
            .behaving(Behaviour::Silent)
            .slow(Duration::from_secs(30)),
    );

    let kinds = [Kind::Dead];
    let started = std::time::Instant::now();
    let (_, outcomes) = round(
        Arc::new(fleet),
        &kinds,
        Limits {
            global: 4,
            per_device: 1,
            device_budget: Duration::from_millis(150),
        },
    )
    .await;

    assert_eq!(outcomes, vec![Outcome::BudgetExhausted]);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the budget did not cut off a 30s device: {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_fleet_still_finishes_every_device() {
    // No missed cycles, with every device answering slowly rather than not at all — the
    // case where a too-small global limit shows up as a cycle that does not complete.
    let kinds = vec![Kind::Slow; 200];
    let fleet = Arc::new(build(&kinds));

    let (_, outcomes) = round(
        fleet,
        &kinds,
        Limits {
            global: 64,
            per_device: 1,
            device_budget: Duration::from_secs(5),
        },
    )
    .await;

    assert_eq!(outcomes.len(), 200);
    assert!(
        outcomes.iter().all(|o| *o == Outcome::Ok),
        "a slow device is not a failed one: {:?}",
        outcomes.iter().filter(|o| **o != Outcome::Ok).count()
    );
}
