//! Probing a simulated estate.
//!
//! `uops_snmp::sim::Fleet` models the behaviours that are hard to obtain on demand from
//! real equipment, which is exactly what a sweep spends its time meeting: silent
//! addresses, agents that refuse credentials, and agents that answer with less than the
//! MIB says they should.

use std::net::SocketAddr;

use uops_core::identity::IdentifierKind;
use uops_discover::{Answer, Range, Sweep, probe};
use uops_profile::Oid;
use uops_snmp::sim::{Agent, Behaviour, Fleet};
use uops_snmp::transport::Value;

const SYSOBJECTID: &str = "1.3.6.1.2.1.1.2.0";
const SYSDESCR: &str = "1.3.6.1.2.1.1.1.0";
const SYSNAME: &str = "1.3.6.1.2.1.1.5.0";

fn oid(s: &str) -> Oid {
    s.parse().expect("a constant OID must parse")
}

/// An agent that answers the three questions a probe asks.
fn device(name: &str, sysobjectid: &str, descr: &str) -> Agent {
    let mut agent = Agent::empty();
    agent.set(oid(SYSOBJECTID), Value::ObjectId(oid(sysobjectid)));
    agent.set(oid(SYSNAME), Value::Bytes(name.as_bytes().to_vec()));
    agent.set(oid(SYSDESCR), Value::Bytes(descr.as_bytes().to_vec()));
    agent
}

fn at(address: &str) -> SocketAddr {
    format!("{address}:161")
        .parse()
        .expect("a test address must parse")
}

#[tokio::test]
async fn a_sweep_finds_every_agent_in_the_range() {
    // §4, the first acceptance criterion: *a /24 sweep against the simulator finds every
    // agent in it*. Eight devices scattered through a /24 of mostly-empty addresses,
    // which is what a real branch office looks like.
    let mut fleet = Fleet::new();
    let placed = [7u8, 12, 31, 64, 65, 128, 200, 254];
    for (n, host) in placed.iter().enumerate() {
        fleet.insert(
            at(&format!("192.168.1.{host}")),
            device(
                &format!("branch-sw-{n:02}"),
                "1.3.6.1.4.1.9.1.2494",
                "Cisco IOS Software",
            ),
        );
    }

    let sweep = Sweep::new(&["192.168.1.0/24".parse::<Range>().expect("a /24 parses")])
        .expect("a /24 is within the limits");

    let mut found = Vec::new();
    for address in sweep.targets(161) {
        if let Answer::Device(sighting) = probe(&fleet, address).await {
            found.push(sighting);
        }
    }

    assert_eq!(
        found.len(),
        placed.len(),
        "every agent in the range must be found"
    );
    assert_eq!(
        sweep.len(),
        254,
        "and the addresses with nothing on them were still probed"
    );

    let names: Vec<&str> = found.iter().filter_map(|s| s.sys_name.as_deref()).collect();
    assert_eq!(names.first(), Some(&"branch-sw-00"));
    assert_eq!(names.len(), placed.len());
}

#[tokio::test]
async fn an_empty_address_is_silent_and_not_a_failure() {
    // 246 of the 254 addresses in the test above. A sweep that treated each one as an
    // error would abort on the first, and a run that aborts at address 9 000 has learned
    // nothing and must start again.
    let fleet = Fleet::new();
    assert_eq!(probe(&fleet, at("192.168.1.99")).await, Answer::Silent);
}

#[tokio::test]
async fn an_agent_that_refuses_the_credentials_is_not_a_silent_address() {
    // §2.2, and the distinction the whole section turns on. This is the outcome that
    // would tempt a scanner into trying another community string; recording it as a
    // distinct fact is what makes the *operator* supply the right credential instead.
    let mut fleet = Fleet::new();
    fleet.insert(
        at("192.168.1.5"),
        device("locked", "1.3.6.1.4.1.9.1.1", "something").behaving(Behaviour::AuthFails),
    );

    assert_eq!(probe(&fleet, at("192.168.1.5")).await, Answer::Refused);
    assert_ne!(
        probe(&fleet, at("192.168.1.5")).await,
        Answer::Silent,
        "a device that refused us is not an empty address, and conflating them is how a \
         product ends up guessing credentials"
    );
}

#[tokio::test]
async fn an_agent_with_no_system_group_is_still_a_device() {
    // Embedded stacks that answer on 161 and have almost nothing in the MIB are real --
    // UPSs, PDUs, environmental sensors. It answered, so something is there; it is simply
    // something nothing can classify, which is a candidate rather than a resource.
    let mut fleet = Fleet::new();
    fleet.insert(at("192.168.1.6"), Agent::empty());

    let Answer::Device(sighting) = probe(&fleet, at("192.168.1.6")).await else {
        panic!("an agent that answers is a device");
    };
    assert_eq!(sighting.sys_object_id, None);
    assert_eq!(sighting.sys_name, None);
}

#[tokio::test]
async fn a_sighting_identifies_the_device_by_address_and_name() {
    let mut fleet = Fleet::new();
    fleet.insert(
        at("10.2.0.9"),
        device("core-01", "1.3.6.1.4.1.9.1.2494", "Cisco IOS Software"),
    );

    let Answer::Device(sighting) = probe(&fleet, at("10.2.0.9")).await else {
        panic!("the agent is there");
    };
    let observed = sighting.observed();

    let kinds: Vec<IdentifierKind> = observed.identifiers.iter().map(|i| i.kind).collect();
    assert_eq!(
        kinds,
        vec![IdentifierKind::MgmtIp, IdentifierKind::Hostname]
    );
    assert_eq!(observed.identifiers[0].value, "10.2.0.9");
    assert_eq!(observed.identifiers[1].value, "core-01");

    // Both are weak by design, and that is the point: neither is tier one, so
    // `uops_identity::classify` will not auto-merge on them alone. A sweep that could
    // silently merge two devices on a hostname would merge every site built from one
    // template.
    assert!(
        !IdentifierKind::MgmtIp.is_tier_one() && !IdentifierKind::Hostname.is_tier_one(),
        "a sweep proves nothing globally unique, which is why the review queue exists"
    );
}

#[tokio::test]
async fn the_sysobjectid_is_what_profile_resolution_will_match_on() {
    // Discovery does not classify: `uops_profile::resolve` does, and it needs this OID.
    // The probe's job is to carry it back intact, prefix and all.
    let mut fleet = Fleet::new();
    fleet.insert(
        at("10.2.0.10"),
        device("edge-01", "1.3.6.1.4.1.9.1.2494", "Cisco IOS Software"),
    );

    let Answer::Device(sighting) = probe(&fleet, at("10.2.0.10")).await else {
        panic!("the agent is there");
    };
    let reported = sighting
        .sys_object_id
        .expect("the agent answered sysObjectID");
    assert!(
        reported.starts_with(&oid("1.3.6.1.4.1.9")),
        "an enterprise prefix must survive the probe: {reported}"
    );
}

#[tokio::test]
async fn a_probe_reads_a_scalar_at_its_instance_or_as_written() {
    // The simulator models a GET the way an agent does -- each object at its instance,
    // silence for one it does not have -- and a profile may spell a scalar either way.
    // `uops-poller` found this against real hardware; the same must hold here.
    let mut fleet = Fleet::new();
    let mut agent = Agent::empty();
    // Written without the trailing .0, which some agents accept and some profiles use.
    agent.set(oid("1.3.6.1.2.1.1.5"), Value::Bytes(b"bare".to_vec()));
    fleet.insert(at("10.2.0.11"), agent);

    let Answer::Device(sighting) = probe(&fleet, at("10.2.0.11")).await else {
        panic!("the agent is there");
    };
    assert_eq!(sighting.sys_name.as_deref(), Some("bare"));
}

#[tokio::test]
async fn a_name_with_bytes_that_are_not_utf8_does_not_lose_the_device() {
    // sysDescr on real equipment carries copyright symbols in whatever encoding the
    // vendor's build machine was set to. Losing the whole description over one byte would
    // lose the only thing that identifies a device with no profile.
    let mut fleet = Fleet::new();
    let mut agent = Agent::empty();
    agent.set(
        oid(SYSDESCR),
        Value::Bytes(vec![b'A', b'C', b'M', b'E', 0xA9, b' ', b'O', b'S']),
    );
    fleet.insert(at("10.2.0.12"), agent);

    let Answer::Device(sighting) = probe(&fleet, at("10.2.0.12")).await else {
        panic!("the agent is there");
    };
    let descr = sighting
        .sys_descr
        .expect("a lossy description is still a description");
    assert!(descr.starts_with("ACME"), "{descr}");
}

#[tokio::test]
async fn an_agent_answering_with_an_empty_name_has_not_told_us_its_name() {
    let mut fleet = Fleet::new();
    let mut agent = Agent::empty();
    agent.set(oid(SYSNAME), Value::Bytes(b"   ".to_vec()));
    fleet.insert(at("10.2.0.13"), agent);

    let Answer::Device(sighting) = probe(&fleet, at("10.2.0.13")).await else {
        panic!("the agent is there");
    };
    assert_eq!(sighting.sys_name, None);
    assert_eq!(
        sighting.observed().identifiers.len(),
        1,
        "a blank name must not become a Hostname identifier that matches every other \
         device with a blank name"
    );
}
