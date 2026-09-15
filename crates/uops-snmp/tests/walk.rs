//! Walking tables against simulated agents.
//!
//! Every test here is a device behaving in a way that is easy to describe, hard to
//! obtain on demand from real hardware, and fatal to a naive loop. That is the whole
//! argument for the transport seam: an agent that returns the same OID forever is a
//! firmware bug somebody will hit, and waiting to meet one is not a test strategy.

use std::net::SocketAddr;
use std::time::Duration;

use uops_profile::Oid;
use uops_snmp::bulk::{DEFAULT, Repetitions, Tuning};
use uops_snmp::sim::{Agent, Behaviour, Fleet};
use uops_snmp::transport::{Target, Value};
use uops_snmp::walk::{self, WalkError};

fn oid(s: &str) -> Oid {
    s.parse().expect("a test OID must parse")
}

/// `IF-MIB::ifEntry`.
fn if_table() -> Oid {
    oid("1.3.6.1.2.1.2.2.1")
}

fn address(n: u16) -> SocketAddr {
    format!("127.0.0.1:{}", 30_000 + n).parse().unwrap()
}

fn fleet_of(agent: Agent) -> (Fleet, Target) {
    let addr = address(1);
    let mut fleet = Fleet::new();
    fleet.insert(addr, agent);
    (fleet, Target { address: addr })
}

#[tokio::test]
async fn a_table_walk_returns_every_cell() {
    // 8 columns × 48 rows, which is an ordinary access switch.
    let (fleet, target) = fleet_of(Agent::with_table(&if_table(), 8, 48));
    let mut tuning = Tuning::default();

    let rows = walk::walk(&fleet, &target, &if_table(), &mut tuning)
        .await
        .expect("walk");

    assert_eq!(rows.len(), 8 * 48);
    assert!(rows.iter().all(|vb| vb.oid.starts_with(&if_table())));
}

#[tokio::test]
async fn a_walk_stops_at_the_end_of_its_table() {
    // The one that matters. GETBULK returns whatever is lexicographically next, which
    // at the end of ifTable is ifXTable — and a loop that does not stop keeps going
    // into the rest of the agent, turning a three-request poll into thousands.
    let mut agent = Agent::with_table(&if_table(), 2, 3);

    // The next table along, and something far away in the MIB.
    let if_x = oid("1.3.6.1.2.1.31.1.1.1");
    for column in 1..=2u32 {
        for index in 1..=3u32 {
            agent.set(if_x.child(column).child(index), Value::Unsigned(99));
        }
    }
    agent.set(oid("1.3.6.1.4.1.9.9.109.1.1.1.1.8.1"), Value::Unsigned(7));

    let (fleet, target) = fleet_of(agent);
    let mut tuning = Tuning::default();

    let rows = walk::walk(&fleet, &target, &if_table(), &mut tuning)
        .await
        .expect("walk");

    assert_eq!(rows.len(), 6, "only ifTable's own cells: {rows:?}");
    assert!(
        rows.iter().all(|vb| vb.oid.starts_with(&if_table())),
        "the walk ran past its table"
    );
}

#[tokio::test]
async fn a_neighbouring_table_with_a_longer_arc_is_not_swallowed() {
    // The arc-wise stop, specifically. 1.3.6.1.2.1.2.2.10 is a different subtree from
    // 1.3.6.1.2.1.2.2.1, and a textual prefix check says it is inside it.
    let table = oid("1.3.6.1.2.1.2.2.1");
    let mut agent = Agent::with_table(&table, 1, 2);
    agent.set(oid("1.3.6.1.2.1.2.2.10.1"), Value::Unsigned(1234));

    let (fleet, target) = fleet_of(agent);
    let mut tuning = Tuning::default();
    let rows = walk::walk(&fleet, &target, &table, &mut tuning)
        .await
        .expect("walk");

    assert_eq!(rows.len(), 2);
    assert!(
        !rows.iter().any(|vb| vb.value == Value::Unsigned(1234)),
        "a textual prefix check would have collected the neighbouring table"
    );
}

#[tokio::test]
async fn an_agent_that_cannot_answer_25_is_walked_at_what_it_can() {
    // What most real agents do at some size. The walk halves until it fits, finishes,
    // and — the part that matters — remembers.
    let agent = Agent::with_table(&if_table(), 4, 20).behaving(Behaviour::TooBigAbove(6));
    let (fleet, target) = fleet_of(agent);
    let mut tuning = Tuning::default();

    let rows = walk::walk(&fleet, &target, &if_table(), &mut tuning)
        .await
        .expect("walk");

    assert_eq!(rows.len(), 80, "the table must still be complete");
    assert!(
        tuning.current() <= Repetitions::new(6),
        "the discovered limit must be kept for the next poll, got {:?}",
        tuning.current()
    );
    assert!(tuning.current() < DEFAULT);
}

#[tokio::test]
async fn an_agent_that_refuses_everything_fails_rather_than_loops() {
    // An overloaded agent answering tooBig to one repetition. "Halve and retry" against
    // this is an infinite loop; the floor makes it a failed poll, which an operator can
    // see and act on.
    let agent = Agent::with_table(&if_table(), 2, 2).behaving(Behaviour::RefusesEverything);
    let (fleet, target) = fleet_of(agent);
    let mut tuning = Tuning::default();

    let err = walk::walk(&fleet, &target, &if_table(), &mut tuning)
        .await
        .expect_err("a device refusing every size must not walk");
    assert_eq!(err, WalkError::RefusesEverything);
}

#[tokio::test]
async fn an_agent_that_does_not_advance_is_detected() {
    // A real firmware bug: the agent returns the OID it was asked about, forever. Every
    // other signal says the device is healthy, and the walk never ends.
    let agent = Agent::with_table(&if_table(), 2, 2).behaving(Behaviour::NeverAdvances);
    let (fleet, target) = fleet_of(agent);
    let mut tuning = Tuning::default();

    let err = walk::walk(&fleet, &target, &if_table(), &mut tuning)
        .await
        .expect_err("a non-advancing agent must not be walked forever");
    assert!(
        matches!(err, WalkError::NotAdvancing { .. }),
        "expected NotAdvancing, got {err:?}"
    );
}

#[tokio::test]
async fn a_silent_device_is_a_timeout_not_a_hang() {
    let agent = Agent::with_table(&if_table(), 2, 2).behaving(Behaviour::Silent);
    let (fleet, target) = fleet_of(agent);
    let mut tuning = Tuning::default();

    let err = walk::walk(&fleet, &target, &if_table(), &mut tuning)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        WalkError::Transport(uops_snmp::TransportError::Timeout)
    ));
}

#[tokio::test]
async fn an_address_with_nothing_behind_it_times_out() {
    let fleet = Fleet::new();
    let target = Target {
        address: address(9),
    };
    let mut tuning = Tuning::default();

    let err = walk::walk(&fleet, &target, &if_table(), &mut tuning)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        WalkError::Transport(uops_snmp::TransportError::Timeout)
    ));
}

#[tokio::test]
async fn an_empty_table_is_no_rows_rather_than_an_error() {
    // A switch with the MIB compiled in and no interfaces up. Empty is an answer.
    let (fleet, target) = fleet_of(Agent::empty());
    let mut tuning = Tuning::default();

    let rows = walk::walk(&fleet, &target, &if_table(), &mut tuning)
        .await
        .expect("an empty agent is not a failure");
    assert!(rows.is_empty());
}

#[tokio::test]
async fn a_hole_in_a_row_is_skipped_and_the_walk_continues() {
    // A column the agent does not implement for one index. NoSuchInstance is a gap in
    // the table, not the end of it, and treating it as the end loses every row after.
    let table = oid("1.3.6.1.2.1.2.2.1");
    let mut agent = Agent::with_table(&table, 2, 4);
    agent.set(table.child(1).child(2), Value::NoSuchInstance);

    let (fleet, target) = fleet_of(agent);
    let mut tuning = Tuning::default();
    let rows = walk::walk(&fleet, &target, &table, &mut tuning)
        .await
        .expect("walk");

    assert_eq!(rows.len(), 7, "one hole in eight cells: {rows:?}");
    assert!(rows.iter().all(|vb| vb.value != Value::NoSuchInstance));
}

#[tokio::test]
async fn a_column_can_be_picked_out_and_indexed() {
    // What a profile actually consumes: one column across every row, tied to the index
    // that says which interface it belongs to.
    let table = if_table();
    let (fleet, target) = fleet_of(Agent::with_table(&table, 3, 5));
    let mut tuning = Tuning::default();
    let rows = walk::walk(&fleet, &target, &table, &mut tuning)
        .await
        .unwrap();

    let column_two = table.child(2);
    let picked = walk::column(&rows, &column_two);
    assert_eq!(picked.len(), 5);

    let indices: Vec<Vec<u32>> = picked
        .iter()
        .filter_map(|vb| walk::index_of(&vb.oid, &column_two))
        .collect();
    assert_eq!(indices, vec![vec![1], vec![2], vec![3], vec![4], vec![5]]);

    // And a column that is not this one yields nothing rather than everything.
    assert!(walk::column(&rows, &oid("1.3.6.1.2.1.99")).is_empty());
    assert_eq!(walk::index_of(&oid("1.3.6.1.2.1.99.1"), &column_two), None);
}

#[tokio::test]
async fn a_thousand_agents_are_a_vec_rather_than_a_lab() {
    // The shape of SPEC §M2's first acceptance criterion, which is a poller test and
    // not this crate's. What this asserts is that the seam supports it: a thousand
    // agents, each walked, with no hardware and no ports.
    let mut fleet = Fleet::new();
    for n in 0..1_000u16 {
        let agent = match n % 100 {
            0 => Agent::with_table(&if_table(), 4, 12).behaving(Behaviour::TooBigAbove(8)),
            1 => Agent::with_table(&if_table(), 4, 12).slow(Duration::from_millis(1)),
            _ => Agent::with_table(&if_table(), 4, 12),
        };
        fleet.insert(address(n), agent);
    }
    assert_eq!(fleet.len(), 1_000);

    let mut total = 0usize;
    for n in 0..1_000u16 {
        let target = Target {
            address: address(n),
        };
        let mut tuning = Tuning::default();
        total += walk::walk(&fleet, &target, &if_table(), &mut tuning)
            .await
            .expect("every agent in the fleet must walk")
            .len();
    }
    assert_eq!(total, 1_000 * 4 * 12);
}
