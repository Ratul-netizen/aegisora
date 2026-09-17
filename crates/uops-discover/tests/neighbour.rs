//! Reading neighbour tables off a simulated switch.
//!
//! The walk itself is `uops-snmp`'s and is tested there. What is tested here is the
//! *decoding* — which is where a neighbour walk actually goes wrong. An LLDP chassis ID
//! is an `OCTET STRING` whose meaning lives in a different column, and reading it as text
//! turns the six bytes of a MAC address into mojibake that is then fingerprinted, stored,
//! and can never match anything again.

use std::net::{IpAddr, Ipv4Addr};

use uops_discover::neighbour::{Protocol, arp, cdp, lldp, neighbours};
use uops_discover::sweep::PROBE_TIMEOUT;
use uops_profile::Oid;
use uops_snmp::sim::{Agent, Behaviour, Fleet};
use uops_snmp::transport::{Target, Value};
use uops_snmp::walk::default_repetitions;
use uops_snmp::{Repetitions, Tuning};

const LLDP_REM: &str = "1.0.8802.1.1.2.1.4.1.1";
const CDP_CACHE: &str = "1.3.6.1.4.1.9.9.23.1.2.1.1";
const ARP_TABLE: &str = "1.3.6.1.2.1.4.22.1";

fn oid(s: &str) -> Oid {
    s.parse().expect("a constant OID must parse")
}

fn target(address: &str) -> Target {
    Target {
        address: format!("{address}:161")
            .parse()
            .expect("a test address must parse"),
    }
}

fn tuning() -> Tuning {
    Tuning::default()
}

/// `lldpRemEntry.<column>.<timeMark>.<localPort>.<index>`
fn lldp_at(agent: &mut Agent, column: u32, row: (u32, u32, u32), value: Value) {
    let oid = oid(LLDP_REM)
        .child(column)
        .child(row.0)
        .child(row.1)
        .child(row.2);
    agent.set(oid, value);
}

/// `cdpCacheEntry.<column>.<ifIndex>.<deviceIndex>`
fn cdp_at(agent: &mut Agent, column: u32, row: (u32, u32), value: Value) {
    agent.set(
        oid(CDP_CACHE).child(column).child(row.0).child(row.1),
        value,
    );
}

/// `ipNetToMediaEntry.<column>.<ifIndex>.<a>.<b>.<c>.<d>`
fn arp_at(agent: &mut Agent, column: u32, ifindex: u32, address: [u8; 4], value: Value) {
    let mut oid = oid(ARP_TABLE).child(column).child(ifindex);
    for octet in address {
        oid = oid.child(u32::from(octet));
    }
    agent.set(oid, value);
}

/// A switch with one LLDP neighbour, encoded the way a real one does it.
fn switch_with_an_lldp_neighbour() -> Agent {
    let mut agent = Agent::empty();
    let row = (1u32, 3u32, 1u32);
    // lldpRemChassisIdSubtype = 4 (macAddress) — which is what makes the next line six
    // bytes rather than a string.
    lldp_at(&mut agent, 4, row, Value::Integer(4));
    lldp_at(
        &mut agent,
        5,
        row,
        Value::Bytes(vec![0x00, 0x1b, 0x21, 0x3c, 0x4d, 0x5e]),
    );
    // lldpRemPortIdSubtype = 5 (interfaceName), so the port ID really is text.
    lldp_at(&mut agent, 6, row, Value::Integer(5));
    lldp_at(
        &mut agent,
        7,
        row,
        Value::Bytes(b"GigabitEthernet0/1".to_vec()),
    );
    lldp_at(&mut agent, 9, row, Value::Bytes(b"core-01".to_vec()));
    lldp_at(
        &mut agent,
        10,
        row,
        Value::Bytes(b"Cisco IOS Software, C2960".to_vec()),
    );
    agent
}

#[tokio::test]
async fn a_mac_chassis_id_is_decoded_rather_than_read_as_text() {
    // The trap this whole module exists for. Six raw bytes read as UTF-8 give mojibake
    // that is then fingerprinted and stored, and the device is found and permanently
    // unidentifiable.
    let mut fleet = Fleet::new();
    fleet.insert(target("10.0.0.1").address, switch_with_an_lldp_neighbour());

    let found = lldp(&fleet, &target("10.0.0.1"), &mut tuning())
        .await
        .expect("walk");

    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].chassis_id.as_deref(),
        Some("00:1b:21:3c:4d:5e"),
        "a subtype-4 chassis id is a MAC address, not a string"
    );
    // And in the spelling `macaddr` renders, so comparing against the database works.
    assert!(
        found[0]
            .chassis_id
            .as_deref()
            .is_some_and(|c| c.chars().all(|ch| ch.is_ascii_hexdigit() || ch == ':')),
        "it must be printable: {:?}",
        found[0].chassis_id
    );
}

#[tokio::test]
async fn the_other_lldp_columns_survive_the_walk() {
    let mut fleet = Fleet::new();
    fleet.insert(target("10.0.0.1").address, switch_with_an_lldp_neighbour());

    let found = lldp(&fleet, &target("10.0.0.1"), &mut tuning())
        .await
        .expect("walk");

    assert_eq!(found[0].port_id.as_deref(), Some("GigabitEthernet0/1"));
    assert_eq!(found[0].sys_name.as_deref(), Some("core-01"));
    assert_eq!(
        found[0].sys_descr.as_deref(),
        Some("Cisco IOS Software, C2960")
    );
}

#[tokio::test]
async fn a_port_id_subtype_means_something_different_from_a_chassis_one() {
    // Subtype 4 is macAddress for a chassis ID and networkAddress for a port ID. One
    // shared table would be wrong half the time.
    let mut agent = Agent::empty();
    let row = (1u32, 1u32, 1u32);
    lldp_at(&mut agent, 4, row, Value::Integer(4)); // chassis: macAddress
    lldp_at(
        &mut agent,
        5,
        row,
        Value::Bytes(vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]),
    );
    lldp_at(&mut agent, 6, row, Value::Integer(3)); // port: macAddress
    lldp_at(
        &mut agent,
        7,
        row,
        Value::Bytes(vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66]),
    );

    let mut fleet = Fleet::new();
    fleet.insert(target("10.0.0.2").address, agent);
    let found = lldp(&fleet, &target("10.0.0.2"), &mut tuning())
        .await
        .expect("walk");

    assert_eq!(found[0].chassis_id.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
    assert_eq!(
        found[0].port_id.as_deref(),
        Some("11:22:33:44:55:66"),
        "a subtype-3 port id is a MAC, and subtype 3 on a chassis would not be"
    );
}

#[tokio::test]
async fn two_neighbours_on_one_port_are_two_rows() {
    // A switch with a hypervisor behind it, or a phone with a PC through it. The LLDP
    // index is (timeMark, localPort, remIndex) and all three are needed to tell them
    // apart — keying on the port alone would silently keep one.
    let mut agent = Agent::empty();
    for (n, name) in [(1u32, "esx-01"), (2u32, "esx-02")] {
        let row = (1u32, 7u32, n);
        lldp_at(&mut agent, 4, row, Value::Integer(4));
        lldp_at(
            &mut agent,
            5,
            row,
            Value::Bytes(vec![0x00, 0x50, 0x56, 0x00, 0x00, u8::try_from(n).unwrap()]),
        );
        lldp_at(&mut agent, 9, row, Value::Bytes(name.as_bytes().to_vec()));
    }

    let mut fleet = Fleet::new();
    fleet.insert(target("10.0.0.3").address, agent);
    let found = lldp(&fleet, &target("10.0.0.3"), &mut tuning())
        .await
        .expect("walk");

    assert_eq!(
        found.len(),
        2,
        "two neighbours on one port are two neighbours"
    );
    let names: Vec<&str> = found.iter().filter_map(|n| n.sys_name.as_deref()).collect();
    assert!(
        names.contains(&"esx-01") && names.contains(&"esx-02"),
        "{names:?}"
    );
}

#[tokio::test]
async fn a_cdp_address_is_four_raw_bytes_not_a_dotted_string() {
    // The same class of mistake as the chassis id. `cdpCacheAddress` is four bytes;
    // reading it as text gives four unprintable characters and loses the one thing CDP
    // contributes that LLDP usually does not — an address to probe.
    let mut agent = Agent::empty();
    let row = (3u32, 1u32);
    cdp_at(&mut agent, 4, row, Value::Bytes(vec![10, 1, 2, 3]));
    cdp_at(
        &mut agent,
        6,
        row,
        Value::Bytes(b"edge-02.example.net".to_vec()),
    );
    cdp_at(
        &mut agent,
        7,
        row,
        Value::Bytes(b"FastEthernet0/2".to_vec()),
    );
    cdp_at(&mut agent, 8, row, Value::Bytes(b"cisco WS-C2960".to_vec()));

    let mut fleet = Fleet::new();
    fleet.insert(target("10.0.0.4").address, agent);
    let found = cdp(&fleet, &target("10.0.0.4"), &mut tuning())
        .await
        .expect("walk");

    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].address,
        Some(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)))
    );
    assert_eq!(found[0].platform.as_deref(), Some("cisco WS-C2960"));
    // And the device id lands in sys_name rather than chassis_id: it is a hostname on
    // most platforms, and calling it a chassis id would give a tier-4 value a tier-1
    // field's authority.
    assert_eq!(found[0].sys_name.as_deref(), Some("edge-02.example.net"));
    assert_eq!(found[0].chassis_id, None);
}

#[tokio::test]
async fn arp_gives_an_address_and_a_mac_and_nothing_else() {
    let mut agent = Agent::empty();
    arp_at(
        &mut agent,
        2,
        3,
        [10, 1, 2, 50],
        Value::Bytes(vec![0x00, 0x0c, 0x29, 0xaa, 0xbb, 0xcc]),
    );
    arp_at(
        &mut agent,
        3,
        3,
        [10, 1, 2, 50],
        Value::Bytes(vec![10, 1, 2, 50]),
    );

    let mut fleet = Fleet::new();
    fleet.insert(target("10.0.0.5").address, agent);
    let found = arp(&fleet, &target("10.0.0.5"), &mut tuning())
        .await
        .expect("walk");

    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].address,
        Some(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 50)))
    );
    assert_eq!(found[0].mac.as_deref(), Some("00:0c:29:aa:bb:cc"));
    assert_eq!(found[0].chassis_id, None, "ARP proves no identity at all");
}

#[tokio::test]
async fn an_incomplete_arp_entry_is_not_a_device() {
    // An address with no MAC is a resolution in progress: somebody tried to reach it and
    // nothing answered. Recording it would put an empty address on the candidate list.
    let mut agent = Agent::empty();
    arp_at(
        &mut agent,
        3,
        3,
        [10, 1, 2, 99],
        Value::Bytes(vec![10, 1, 2, 99]),
    );

    let mut fleet = Fleet::new();
    fleet.insert(target("10.0.0.6").address, agent);
    let found = arp(&fleet, &target("10.0.0.6"), &mut tuning())
        .await
        .expect("walk");
    assert!(
        found.is_empty(),
        "an unresolved ARP entry is not a neighbour"
    );
}

#[tokio::test]
async fn broadcast_and_multicast_arp_entries_are_not_devices() {
    // Every ARP table has them and none of them is something to go and probe.
    let mut agent = Agent::empty();
    arp_at(
        &mut agent,
        2,
        3,
        [239, 1, 1, 1],
        Value::Bytes(vec![0x01, 0x00, 0x5e, 0x01, 0x01, 0x01]),
    );
    arp_at(
        &mut agent,
        3,
        3,
        [239, 1, 1, 1],
        Value::Bytes(vec![239, 1, 1, 1]),
    );
    arp_at(
        &mut agent,
        2,
        3,
        [10, 1, 2, 7],
        Value::Bytes(vec![0x00, 0x0c, 0x29, 0x11, 0x22, 0x33]),
    );
    arp_at(
        &mut agent,
        3,
        3,
        [10, 1, 2, 7],
        Value::Bytes(vec![10, 1, 2, 7]),
    );

    let mut fleet = Fleet::new();
    fleet.insert(target("10.0.0.7").address, agent);
    let found = arp(&fleet, &target("10.0.0.7"), &mut tuning())
        .await
        .expect("walk");

    assert_eq!(found.len(), 1, "only the real host survives");
    assert_eq!(
        found[0].address,
        Some(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 7)))
    );
}

#[tokio::test]
async fn a_switch_that_runs_only_cdp_is_not_a_failure() {
    // A walk that gave up on the first empty table would find nothing on a Cisco estate.
    let mut agent = Agent::empty();
    let row = (1u32, 1u32);
    cdp_at(&mut agent, 4, row, Value::Bytes(vec![192, 168, 5, 1]));
    cdp_at(&mut agent, 6, row, Value::Bytes(b"only-cdp".to_vec()));

    let mut fleet = Fleet::new();
    fleet.insert(target("10.0.0.8").address, agent);
    let found = neighbours(&fleet, &target("10.0.0.8"), &mut tuning()).await;

    assert!(found.lldp.is_empty());
    assert_eq!(found.cdp.len(), 1);
    assert!(
        found.unreadable.is_empty(),
        "an empty table is an answer, not an error: {:?}",
        found.unreadable
    );
    assert_eq!(found.merged().len(), 1);
}

#[tokio::test]
async fn a_device_that_refuses_the_walk_is_recorded_rather_than_lost() {
    let mut fleet = Fleet::new();
    fleet.insert(
        target("10.0.0.9").address,
        switch_with_an_lldp_neighbour().behaving(Behaviour::AuthFails),
    );

    let found = neighbours(&fleet, &target("10.0.0.9"), &mut tuning()).await;
    assert!(found.merged().is_empty());
    assert_eq!(
        found.unreadable.len(),
        3,
        "all three tables were refused, and all three say so: {:?}",
        found.unreadable
    );
    assert!(found.unreadable.iter().any(|(p, _)| *p == Protocol::Lldp));
}

#[tokio::test]
async fn a_neighbour_seen_by_both_protocols_is_one_neighbour() {
    // A Cisco switch typically runs LLDP and CDP at once. Two rows for one device would
    // put the same switch on the candidate list twice, which is the sort of thing that
    // makes an operator stop trusting the list.
    let mut agent = Agent::empty();
    let row = (1u32, 3u32, 1u32);
    lldp_at(&mut agent, 4, row, Value::Integer(4));
    lldp_at(
        &mut agent,
        5,
        row,
        Value::Bytes(vec![0x00, 0x1b, 0x21, 0x3c, 0x4d, 0x5e]),
    );
    lldp_at(&mut agent, 9, row, Value::Bytes(b"core-01".to_vec()));
    // The same device over CDP, which is the one that knows its address.
    cdp_at(&mut agent, 4, (3, 1), Value::Bytes(vec![10, 1, 0, 1]));
    cdp_at(&mut agent, 6, (3, 1), Value::Bytes(b"core-01".to_vec()));
    cdp_at(
        &mut agent,
        8,
        (3, 1),
        Value::Bytes(b"cisco WS-C3850".to_vec()),
    );

    let mut fleet = Fleet::new();
    fleet.insert(target("10.0.0.10").address, agent);
    let found = neighbours(&fleet, &target("10.0.0.10"), &mut tuning()).await;
    let merged = found.merged();

    assert_eq!(
        merged.len(),
        1,
        "one device, however many protocols describe it"
    );
    // LLDP's identifiers win, and CDP's address fills the gap LLDP left — which is
    // exactly what makes the neighbour probeable.
    assert_eq!(merged[0].chassis_id.as_deref(), Some("00:1b:21:3c:4d:5e"));
    assert_eq!(
        merged[0].address,
        Some(IpAddr::V4(Ipv4Addr::new(10, 1, 0, 1)))
    );
    assert_eq!(merged[0].platform.as_deref(), Some("cisco WS-C3850"));
}

#[tokio::test]
async fn an_arp_entry_is_never_folded_into_an_lldp_neighbour() {
    // An ARP entry proves an address is in use, not that it belongs to the device LLDP
    // just described. Merging them would attach an address on a coincidence -- and the
    // address is what the next sweep would go and probe.
    let mut agent = Agent::empty();
    let row = (1u32, 3u32, 1u32);
    lldp_at(&mut agent, 4, row, Value::Integer(4));
    lldp_at(
        &mut agent,
        5,
        row,
        Value::Bytes(vec![0x00, 0x1b, 0x21, 0x3c, 0x4d, 0x5e]),
    );
    lldp_at(&mut agent, 9, row, Value::Bytes(b"core-01".to_vec()));
    arp_at(
        &mut agent,
        2,
        3,
        [10, 1, 2, 50],
        Value::Bytes(vec![0x00, 0x0c, 0x29, 0xaa, 0xbb, 0xcc]),
    );
    arp_at(
        &mut agent,
        3,
        3,
        [10, 1, 2, 50],
        Value::Bytes(vec![10, 1, 2, 50]),
    );

    let mut fleet = Fleet::new();
    fleet.insert(target("10.0.0.11").address, agent);
    let merged = neighbours(&fleet, &target("10.0.0.11"), &mut tuning())
        .await
        .merged();

    assert_eq!(
        merged.len(),
        2,
        "an LLDP neighbour and an ARP sighting are two things"
    );
    let lldp_one = merged
        .iter()
        .find(|n| n.chassis_id.is_some())
        .expect("the LLDP neighbour");
    assert_eq!(
        lldp_one.address, None,
        "LLDP said nothing about an address and ARP must not supply one"
    );
}

#[tokio::test]
async fn a_row_with_an_index_and_no_columns_is_dropped() {
    // A neighbour that ages out mid-walk leaves exactly this. Keeping it would write a
    // candidate with neither an address nor a chassis id, which the schema refuses --
    // and correctly, because its fingerprint would collide with every other empty row.
    let mut agent = Agent::empty();
    let row = (1u32, 3u32, 1u32);
    lldp_at(&mut agent, 6, row, Value::Integer(5));
    lldp_at(&mut agent, 8, row, Value::Bytes(b"uplink".to_vec()));

    let mut fleet = Fleet::new();
    fleet.insert(target("10.0.0.12").address, agent);
    let found = lldp(&fleet, &target("10.0.0.12"), &mut tuning())
        .await
        .expect("walk");
    assert!(
        found.is_empty(),
        "an index with no identity is not a neighbour"
    );
}

#[tokio::test]
async fn the_walk_is_bounded_by_the_same_things_a_probe_is() {
    // Sanity: a neighbour walk uses uops-snmp's walk, which has its own protections
    // against an agent that never advances. This asserts the constants it is handed are
    // the real ones rather than a test's invention.
    assert!(default_repetitions().get() >= Repetitions::new(1).get());
    assert!(PROBE_TIMEOUT.as_secs() >= 1);
}
