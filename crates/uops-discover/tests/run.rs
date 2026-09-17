//! A whole sweep, and the limits it stays inside.
//!
//! §4's seventh acceptance criterion — *a sweep stays within its concurrency and rate
//! caps, measured* — is the reason this file exists. It is measured against tokio's
//! paused clock rather than the wall: a test that genuinely waited for 65 536 addresses
//! at 200/s would take five and a half minutes, so it would be marked `#[ignore]` and
//! then it would never run, and a rate cap nobody checks is a comment.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use uops_discover::sweep::{IN_FLIGHT, MAX_ADDRESSES, PROBE_TIMEOUT, PROBES_PER_SECOND};
use uops_discover::{Range, Sweep, run};
use uops_profile::Oid;
use uops_snmp::Repetitions;
use uops_snmp::sim::{Agent, Behaviour, Fleet};
use uops_snmp::transport::{Target, Transport, TransportError, Value, VarBind};

const SYSNAME: &str = "1.3.6.1.2.1.1.5.0";

fn oid(s: &str) -> Oid {
    s.parse().expect("a constant OID must parse")
}

fn at(address: &str) -> SocketAddr {
    format!("{address}:161")
        .parse()
        .expect("a test address must parse")
}

fn sweep_of(range: &str) -> Sweep {
    Sweep::new(&[range.parse::<Range>().expect("a test range parses")])
        .expect("a test range is within the limits")
}

fn named(name: &str) -> Agent {
    let mut agent = Agent::empty();
    agent.set(oid(SYSNAME), Value::Bytes(name.as_bytes().to_vec()));
    agent
}

#[tokio::test]
async fn a_sweep_reports_every_address_it_probed() {
    let mut fleet = Fleet::new();
    fleet.insert(at("192.168.1.10"), named("sw-01"));
    fleet.insert(at("192.168.1.20"), named("sw-02"));
    fleet.insert(
        at("192.168.1.30"),
        named("locked").behaving(Behaviour::AuthFails),
    );

    let sweep = sweep_of("192.168.1.0/24");
    let findings = run(&fleet, &sweep, 161).await;

    assert_eq!(findings.probed, 254);
    assert_eq!(findings.devices.len(), 2);
    assert_eq!(findings.refused.len(), 1);
    assert_eq!(findings.silent, 251);

    // The identity the schema's CHECK asserts: nothing answered that was not probed, and
    // the difference between the two is the empty addresses and nothing else.
    assert!(findings.answered() <= findings.probed);
    assert_eq!(findings.probed - findings.answered(), findings.silent);
}

#[tokio::test]
async fn a_refusal_counts_as_an_answer() {
    // An agent that rejected the credentials proved it exists, which is the whole reason
    // §2.2 can afford to record it rather than guess at another community string. If this
    // counted as silent, `probed - answered` would overstate the empty addresses and the
    // operator's one signal that their credential list is wrong would be gone.
    let mut fleet = Fleet::new();
    fleet.insert(
        at("10.0.0.5"),
        named("locked").behaving(Behaviour::AuthFails),
    );

    let findings = run(&fleet, &sweep_of("10.0.0.0/29"), 161).await;
    assert_eq!(findings.answered(), 1);
    assert!(findings.devices.is_empty());
}

#[tokio::test]
async fn findings_come_back_in_address_order() {
    // `buffer_unordered` yields as probes complete, so without the sort an estate with one
    // slow device reports a different order every run -- and an operator comparing this
    // morning's candidates against last night's is doing it by eye.
    let mut fleet = Fleet::new();
    // The first address answers slowest, so completion order is the reverse of address
    // order unless something puts it back.
    let mut slow = named("sw-first");
    slow = slow.slow(Duration::from_millis(200));
    fleet.insert(at("10.0.0.1"), slow);
    fleet.insert(at("10.0.0.2"), named("sw-second"));
    fleet.insert(at("10.0.0.3"), named("sw-third"));

    let findings = run(&fleet, &sweep_of("10.0.0.0/29"), 161).await;
    let addresses: Vec<SocketAddr> = findings.devices.iter().map(|s| s.address).collect();
    let mut sorted = addresses.clone();
    sorted.sort_unstable();
    assert_eq!(
        addresses, sorted,
        "findings must be reported in address order"
    );
}

/// A transport that answers instantly and records how many probes overlap.
///
/// The simulator cannot answer this question -- it has no notion of an outstanding
/// request -- so the concurrency cap needs a transport built to watch for it.
#[derive(Debug, Default)]
struct Counting {
    live: AtomicUsize,
    peak: AtomicUsize,
    total: AtomicUsize,
}

#[async_trait::async_trait]
impl Transport for Counting {
    async fn get_bulk(
        &self,
        _target: &Target,
        _after: &Oid,
        _max: Repetitions,
    ) -> Result<Vec<VarBind>, TransportError> {
        Err(TransportError::Timeout)
    }

    async fn get_scalars(
        &self,
        _target: &Target,
        _oids: &[Oid],
    ) -> Result<Vec<VarBind>, TransportError> {
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);
        self.total.fetch_add(1, Ordering::SeqCst);
        // Long enough that probes genuinely overlap. Under a paused clock this costs no
        // wall time, which is the point: the concurrency is real and the waiting is not.
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.live.fetch_sub(1, Ordering::SeqCst);
        Err(TransportError::Timeout)
    }
}

#[tokio::test(start_paused = true)]
async fn a_sweep_never_exceeds_its_concurrency_cap() {
    // The limit is not about this process. It is about the firewall between here and the
    // estate, which holds state for every outstanding UDP probe and has a table size.
    let transport = Arc::new(Counting::default());
    let findings = run(transport.as_ref(), &sweep_of("10.0.0.0/24"), 161).await;

    assert_eq!(findings.probed, 254);
    assert_eq!(transport.total.load(Ordering::SeqCst), 254);

    let peak = transport.peak.load(Ordering::SeqCst);
    assert!(
        peak <= IN_FLIGHT,
        "{peak} probes were in flight at once, and the cap is {IN_FLIGHT}"
    );
    assert!(
        peak > 1,
        "the sweep ran one probe at a time -- the pace lock is being held across the \
         sleep, which discards the concurrency cap entirely"
    );
}

#[tokio::test(start_paused = true)]
async fn a_sweep_never_exceeds_its_rate_cap() {
    // Measured as elapsed time for a known number of probes, which is the only thing a
    // customer's IDS measures either.
    let transport = Arc::new(Counting::default());
    let sweep = sweep_of("10.0.0.0/24");

    let started = tokio::time::Instant::now();
    let findings = run(transport.as_ref(), &sweep, 161).await;
    let elapsed = started.elapsed();

    // 254 probes at 200/s is 1.27 seconds. The floor is what matters: finishing sooner
    // than the rate allows means the cap is not being applied.
    let floor = Duration::from_secs(1) / PROBES_PER_SECOND * 253;
    assert!(
        elapsed >= floor,
        "254 probes took {elapsed:?}, which is faster than {PROBES_PER_SECOND}/s allows \
         ({floor:?}) -- the rate cap is not being applied"
    );
    assert_eq!(findings.probed, 254);
}

#[tokio::test(start_paused = true)]
async fn the_rate_is_global_rather_than_per_range() {
    // A job with forty ranges must not send forty times the traffic of a job with one.
    // The pace belongs to the run, and this is the test that says so.
    let transport = Arc::new(Counting::default());
    let ranges: Vec<Range> = (1..=4)
        .map(|n| {
            format!("10.0.{n}.0/24")
                .parse::<Range>()
                .expect("a test range parses")
        })
        .collect();
    let sweep = Sweep::new(&ranges).expect("four /24s are within the limits");

    let started = tokio::time::Instant::now();
    run(transport.as_ref(), &sweep, 161).await;
    let elapsed = started.elapsed();

    let floor = Duration::from_secs(1) / PROBES_PER_SECOND * (4 * 254 - 1);
    assert!(
        elapsed >= floor,
        "four ranges finished in {elapsed:?}, faster than one shared budget allows \
         ({floor:?}) -- the pace is per-range rather than per-run"
    );
}

#[tokio::test]
async fn an_empty_estate_is_a_complete_run_rather_than_a_failure() {
    // The common case for a range an operator guessed at, and it must not look broken.
    let findings = run(&Fleet::new(), &sweep_of("10.0.0.0/28"), 161).await;
    assert_eq!(findings.probed, 14);
    assert_eq!(findings.silent, 14);
    assert_eq!(findings.answered(), 0);
    assert!(
        findings.failed.is_empty(),
        "nothing failed; nothing was there"
    );
}

#[tokio::test(start_paused = true)]
async fn a_device_slower_than_the_probe_timeout_is_silent() {
    // Not a lost device: a probe is a single small GET, and something that cannot answer
    // one in PROBE_TIMEOUT on a management network is not something the *sweep* should
    // wait for. It will be found by the next run, or by a neighbour that can reach it.
    let mut fleet = Fleet::new();
    fleet.insert(
        at("10.0.0.1"),
        named("distant").slow(PROBE_TIMEOUT + Duration::from_secs(1)),
    );
    fleet.insert(at("10.0.0.2"), named("near"));

    let findings = run(&fleet, &sweep_of("10.0.0.0/29"), 161).await;
    assert_eq!(
        findings.devices.len(),
        1,
        "only the near device answered in time"
    );
    assert_eq!(findings.devices[0].sys_name.as_deref(), Some("near"));
}

#[test]
fn the_three_caps_agree_with_each_other() {
    // The arithmetic that was wrong once and would be wrong again silently.
    //
    // A sweep of empty addresses runs at `IN_FLIGHT / PROBE_TIMEOUT` probes per second no
    // matter what PROBES_PER_SECOND says. If that is the smaller number then it is the
    // real rate, the documented one is decoration, and a /16 takes 85 minutes while
    // appearing to be capped at 200/s. Which is exactly what the first version of these
    // constants did.
    let concurrency_allows = f64::from(u32::try_from(IN_FLIGHT).expect("a cap fits in a u32"))
        / PROBE_TIMEOUT.as_secs_f64();
    assert!(
        concurrency_allows >= f64::from(PROBES_PER_SECOND),
        "IN_FLIGHT ({IN_FLIGHT}) over PROBE_TIMEOUT ({PROBE_TIMEOUT:?}) allows only \
         {concurrency_allows}/s, so the rate cap of {PROBES_PER_SECOND}/s can never be \
         reached and is not the limit that binds"
    );

    // And the promise that number was made against: a /16 in about five and a half
    // minutes. MAX_ADDRESSES is documented as having been chosen for it.
    let seconds = f64::from(MAX_ADDRESSES) / f64::from(PROBES_PER_SECOND);
    assert!(
        (300.0..400.0).contains(&seconds),
        "a full sweep takes {seconds}s, and MAX_ADDRESSES is documented as chosen for \
         about 330"
    );
}

/// A transport that answers only to one community string, the way a real agent does.
///
/// The important half is the *silence*: `SNMPv2c` has no way to say "wrong community",
/// so an agent drops what it cannot authenticate. A sweep that treated silence as "no
/// device here" would find nothing on any estate whose first credential is not the right
/// one — which is most of them, since the list is ordered by guesswork.
#[derive(Debug)]
struct Picky {
    /// Which transport index the device will actually answer.
    accepts: usize,
    me: usize,
    /// Set when this transport was asked anything at all.
    asked: AtomicUsize,
}

#[async_trait::async_trait]
impl Transport for Picky {
    async fn get_bulk(
        &self,
        _target: &Target,
        _after: &Oid,
        _max: Repetitions,
    ) -> Result<Vec<VarBind>, TransportError> {
        Err(TransportError::Timeout)
    }

    async fn get_scalars(
        &self,
        _target: &Target,
        oids: &[Oid],
    ) -> Result<Vec<VarBind>, TransportError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        if self.me != self.accepts {
            // Silence, not a refusal. This is the whole point.
            return Err(TransportError::Timeout);
        }
        Ok(vec![VarBind {
            oid: oids[1].clone(),
            value: Value::Bytes(b"picky-sw".to_vec()),
        }])
    }
}

#[tokio::test(start_paused = true)]
async fn a_sweep_tries_every_credential_because_a_wrong_one_is_silent() {
    // The device answers only the third credential. A loop that gave up on silence would
    // report an empty network, which is the failure mode that makes an operator believe
    // discovery does not work.
    let transports: Vec<Picky> = (0..3)
        .map(|me| Picky {
            accepts: 2,
            me,
            asked: AtomicUsize::new(0),
        })
        .collect();
    let refs: Vec<&Picky> = transports.iter().collect();

    let findings = uops_discover::run_with(&refs, &sweep_of("10.0.0.0/30"), 161).await;

    assert_eq!(findings.devices.len(), 2, "both hosts in a /30 answered");
    assert_eq!(
        findings.devices[0].credential,
        Some(2),
        "and the sweep recorded which credential worked, which is what makes the device \
         pollable afterwards"
    );
    // The first two were tried against every address and answered nothing.
    assert_eq!(transports[0].asked.load(Ordering::SeqCst), 2);
    assert_eq!(transports[1].asked.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn a_credential_that_works_stops_the_loop() {
    // The cost of the loop is why MAX_CREDENTIALS exists, so not paying it when the first
    // credential answers is the one optimisation available.
    let transports: Vec<Picky> = (0..3)
        .map(|me| Picky {
            accepts: 0,
            me,
            asked: AtomicUsize::new(0),
        })
        .collect();
    let refs: Vec<&Picky> = transports.iter().collect();

    let findings = uops_discover::run_with(&refs, &sweep_of("10.0.0.0/30"), 161).await;

    assert_eq!(findings.devices.len(), 2);
    assert_eq!(findings.devices[0].credential, Some(0));
    assert_eq!(
        transports[2].asked.load(Ordering::SeqCst),
        0,
        "the third credential was never needed and never sent"
    );
}

#[tokio::test(start_paused = true)]
async fn the_rate_cap_counts_probes_rather_than_addresses() {
    // Four credentials over a /24 is a thousand packets, and the customer's network sees
    // a thousand packets. Pacing by address would quietly send four times what was agreed
    // with them — which is exactly the promise the rate cap exists to keep.
    let transport = Arc::new(Counting::default());
    let refs: Vec<&Counting> = vec![transport.as_ref(); 3];

    let started = tokio::time::Instant::now();
    uops_discover::run_with(&refs, &sweep_of("10.0.0.0/26"), 161).await;
    let elapsed = started.elapsed();

    // 62 hosts × 3 credentials, none of which answer.
    assert_eq!(transport.total.load(Ordering::SeqCst), 186);
    let floor = Duration::from_secs(1) / PROBES_PER_SECOND * 185;
    assert!(
        elapsed >= floor,
        "186 probes took {elapsed:?}, faster than {PROBES_PER_SECOND}/s allows ({floor:?})"
    );
}
