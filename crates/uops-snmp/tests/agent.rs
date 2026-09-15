//! Against the real `net-snmp` agent.
//!
//! SPEC §M2: *"`SNMPv3` authPriv (SHA-256 / AES-256) against a real device."* The
//! simulator proves every failure mode; this proves the wire format, the USM key
//! derivation and the crypto — the things a simulator cannot, because it is the same
//! code on both ends.
//!
//! ```bash
//! docker compose -f deploy/docker-compose.yml --profile test up -d snmp-agent
//! UOPS_SNMP_AGENT=127.0.0.1:16100 cargo test -p uops-snmp --test agent
//! ```
//!
//! Skipped when `UOPS_SNMP_AGENT` is unset, rather than failing. A developer without the
//! fixture running should not get a red suite for a container they did not start — and a
//! skipped test says so out loud, so it cannot be mistaken for a passing one.

use std::time::Duration;

use uops_core::{AuthProtocol, CredentialMaterial, PrivProtocol, Secret};
use uops_profile::Oid;
use uops_snmp::bulk::Tuning;
use uops_snmp::transport::{Target, Transport, TransportError, Value};
use uops_snmp::udp::UdpTransport;
use uops_snmp::walk;

/// Everything the fixture is configured with. See `deploy/snmp-agent/README.md` — all
/// of it is public on purpose.
const USER: &str = "uops-v3";
const AUTH_PASS: &str = "uops-auth-passphrase";
const PRIV_PASS: &str = "uops-priv-passphrase";
const COMMUNITY: &str = "uops-test";

/// The agent's address, or `None` if the fixture is not running.
fn agent() -> Option<Target> {
    let raw = std::env::var("UOPS_SNMP_AGENT").ok()?;
    let address = raw
        .parse()
        .unwrap_or_else(|e| panic!("UOPS_SNMP_AGENT={raw} is not an address: {e}"));
    Some(Target { address })
}

macro_rules! agent_or_skip {
    () => {
        match agent() {
            Some(t) => t,
            None => {
                println!(
                    "SKIPPED: UOPS_SNMP_AGENT is unset. Start the fixture with \
                     `docker compose -f deploy/docker-compose.yml --profile test up -d snmp-agent`"
                );
                return;
            }
        }
    };
}

fn v3() -> CredentialMaterial {
    CredentialMaterial::SnmpV3 {
        username: USER.to_owned(),
        auth: AuthProtocol::Sha256,
        auth_key: AUTH_PASS.to_owned(),
        privacy: PrivProtocol::Aes256,
        priv_key: PRIV_PASS.to_owned(),
    }
}

fn oid(s: &str) -> Oid {
    s.parse().expect("test OID")
}

/// `SNMPv2-MIB::system`.
fn system() -> Oid {
    oid("1.3.6.1.2.1.1")
}

/// `IF-MIB::ifName`.
fn if_name() -> Oid {
    oid("1.3.6.1.2.1.31.1.1.1.1")
}

#[tokio::test]
async fn sha256_aes256_authpriv_reads_the_system_group() {
    // SPEC's exact pair, against a real USM implementation. Everything about this path
    // — key localisation, the privacy IV, the HMAC — is code neither side shares.
    let target = agent_or_skip!();
    let transport = UdpTransport::new(Secret::new(v3())).with_timeout(Duration::from_secs(5));

    let mut tuning = Tuning::default();
    let rows = walk::walk(&transport, &target, &system(), &mut tuning)
        .await
        .expect("SHA-256/AES-256 authPriv must read the system group");

    assert!(!rows.is_empty(), "the system group is never empty");

    // sysName.0 is what the fixture sets, so this is end-to-end: our request, its
    // encryption, its agent, its answer, our decryption.
    let sys_name = rows
        .iter()
        .find(|vb| vb.oid == oid("1.3.6.1.2.1.1.5.0"))
        .expect("sysName.0 must be in the system group");
    assert_eq!(
        sys_name.value,
        Value::Bytes(b"uops-test-agent".to_vec()),
        "got {:?}",
        sys_name.value
    );
}

#[tokio::test]
async fn a_walk_of_a_real_interface_table_stops_at_its_boundary() {
    // The stop condition, against an agent that really does have the next table. On the
    // simulator this is a fixture I wrote; here it is IF-MIB as net-snmp implements it.
    let target = agent_or_skip!();
    let transport = UdpTransport::new(Secret::new(v3()));

    let mut tuning = Tuning::default();
    let rows = walk::walk(&transport, &target, &if_name(), &mut tuning)
        .await
        .expect("walk ifName");

    assert!(!rows.is_empty(), "the container has interfaces");
    assert!(
        rows.iter().all(|vb| vb.oid.starts_with(&if_name())),
        "the walk ran past ifName into the rest of ifXTable"
    );

    // Container interfaces: loopback and at least one ethernet.
    let names: Vec<String> = rows
        .iter()
        .filter_map(|vb| match &vb.value {
            Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        })
        .collect();
    assert!(names.iter().any(|n| n == "lo"), "{names:?}");
}

#[tokio::test]
async fn a_v2c_community_also_works() {
    // The other credential kind, on the same agent. A poller that only spoke v3 would
    // be unable to monitor most of the equipment that exists.
    let target = agent_or_skip!();
    let transport = UdpTransport::new(Secret::new(CredentialMaterial::SnmpCommunity(
        COMMUNITY.to_owned(),
    )));

    let mut tuning = Tuning::default();
    let rows = walk::walk(&transport, &target, &system(), &mut tuning)
        .await
        .expect("v2c must read the system group");
    assert!(!rows.is_empty());
}

#[tokio::test]
async fn a_wrong_passphrase_is_refused_rather_than_answered() {
    // The assertion that proves the one above is not passing by accident. If the agent
    // answered anything to a bad credential, `sha256_aes256_authpriv_reads_the_system_group`
    // would prove nothing about authentication.
    let target = agent_or_skip!();
    let transport = UdpTransport::new(Secret::new(CredentialMaterial::SnmpV3 {
        username: USER.to_owned(),
        auth: AuthProtocol::Sha256,
        auth_key: "not-the-passphrase".to_owned(),
        privacy: PrivProtocol::Aes256,
        priv_key: PRIV_PASS.to_owned(),
    }))
    .with_timeout(Duration::from_secs(3));

    let mut tuning = Tuning::default();
    let result = walk::walk(&transport, &target, &system(), &mut tuning).await;
    assert!(
        result.is_err(),
        "a wrong auth passphrase must not read the system group: {result:?}"
    );
}

#[tokio::test]
async fn the_session_pool_reuses_one_socket_per_device() {
    // The design decision in udp.rs, observed rather than asserted from the code: many
    // requests to one device open one session, so v3 engine discovery is paid once.
    let target = agent_or_skip!();
    let transport = UdpTransport::new(Secret::new(v3()));

    for _ in 0..5 {
        let mut tuning = Tuning::default();
        walk::walk(&transport, &target, &system(), &mut tuning)
            .await
            .expect("walk");
    }

    assert_eq!(
        transport.open_sessions().await,
        1,
        "five walks of one device must share one session"
    );
}

#[tokio::test]
async fn an_address_with_no_agent_times_out_rather_than_hanging() {
    // Port 1 has nothing on it. A poller that hangs here holds an executor slot until
    // its budget expires, which is the behaviour the budget exists to bound — but the
    // transport should not need rescuing by it.
    if agent().is_none() {
        println!("SKIPPED: UOPS_SNMP_AGENT is unset");
        return;
    }
    let transport = UdpTransport::new(Secret::new(v3())).with_timeout(Duration::from_millis(400));
    let target = Target {
        address: "127.0.0.1:1".parse().unwrap(),
    };

    let started = std::time::Instant::now();
    let mut tuning = Tuning::default();
    let err = walk::walk(&transport, &target, &system(), &mut tuning)
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            uops_snmp::WalkError::Transport(
                TransportError::Timeout | TransportError::Unreachable(_)
            )
        ),
        "{err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_set_of_scalars_comes_back_in_one_request() {
    // The path a metric poll actually takes, and the one the simulator cannot check: the
    // simulator uses the trait's default `get_scalars`, which is a GETNEXT per OID, and
    // UdpTransport overrides it with a single GET. Two different PDUs, and only this
    // test sends the one that runs in production.
    //
    // Both spellings a profile may use are asked for at once. `sysUpTime` names the
    // object and `sysUpTime.0` names its only instance; a profile author writes whichever
    // the MIB document in front of them shows, and the answer must be the same.
    let target = agent_or_skip!();
    let transport = UdpTransport::new(Secret::new(v3())).with_timeout(Duration::from_secs(5));

    let wanted = [
        oid("1.3.6.1.2.1.1.3"),   // sysUpTime, as the object
        oid("1.3.6.1.2.1.1.3.0"), // sysUpTime, as the instance
        oid("1.3.6.1.2.1.1.7.0"), // sysServices
    ];
    let rows = transport
        .get_scalars(&target, &wanted)
        .await
        .expect("the agent must answer a GET");

    // Every answer is at the instance, whichever spelling was asked for.
    let uptime = oid("1.3.6.1.2.1.1.3.0");
    let answers: Vec<&Value> = rows
        .iter()
        .filter(|vb| vb.oid == uptime)
        .map(|vb| &vb.value)
        .collect();
    assert_eq!(
        answers.len(),
        2,
        "both spellings of sysUpTime must be answered, at its instance: {rows:?}"
    );
    assert!(
        answers
            .iter()
            .all(|v| matches!(v, Value::Unsigned(_) | Value::Integer(_))),
        "sysUpTime must be a number: {answers:?}"
    );

    assert!(
        rows.iter().any(|vb| vb.oid == oid("1.3.6.1.2.1.1.7.0")),
        "sysServices was asked for and is not in the answer: {rows:?}"
    );
}

#[tokio::test]
async fn a_scalar_that_the_agent_does_not_have_does_not_fail_the_others() {
    // A profile is written for a family of devices and one of them will not implement
    // every OID in it. GET answers per varbind — noSuchObject for the missing one — and
    // the rest of the response is still a response. A transport that treated this as a
    // failed request would lose every metric on the device over one absent OID.
    let target = agent_or_skip!();
    let transport = UdpTransport::new(Secret::new(v3()));

    let rows = transport
        .get_scalars(
            &target,
            &[
                oid("1.3.6.1.2.1.1.3.0"),
                // Under a private enterprise number that is not assigned to anything
                // this agent implements.
                oid("1.3.6.1.4.1.99999.1.1.0"),
            ],
        )
        .await
        .expect("a missing OID is not a failed request");

    assert!(
        rows.iter().any(|vb| vb.oid == oid("1.3.6.1.2.1.1.3.0")
            && matches!(vb.value, Value::Unsigned(_) | Value::Integer(_))),
        "the OID the agent does have must still be answered: {rows:?}"
    );
}
