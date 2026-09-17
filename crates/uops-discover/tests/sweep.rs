//! What a sweep will and will not probe.
//!
//! These are the acceptance criteria from `docs/M5-discovery.md` §4 that do not need a
//! network: the bound is enforced, the refusal says what to do instead, and the address
//! list is the one an operator would draw on a whiteboard.

use std::net::Ipv4Addr;

use uops_discover::{MAX_ADDRESSES, Range, Sweep, SweepError};

fn range(s: &str) -> Range {
    s.parse().expect("a test range must parse")
}

#[test]
fn a_24_is_254_hosts() {
    let sweep = Sweep::new(&[range("192.168.1.0/24")]).expect("a /24 is within the limits");
    assert_eq!(
        sweep.len(),
        254,
        "a /24 is 256 addresses less network and broadcast"
    );
    assert_eq!(
        sweep.addresses().first(),
        Some(&Ipv4Addr::new(192, 168, 1, 1))
    );
    assert_eq!(
        sweep.addresses().last(),
        Some(&Ipv4Addr::new(192, 168, 1, 254))
    );
}

#[test]
fn the_broadcast_address_is_never_probed() {
    // Not about the two addresses saved. The broadcast address makes every host on the
    // segment answer at once, which looks like a discovery tool that has found a great
    // many devices and is one being shouted at by the same device several hundred times.
    let sweep = Sweep::new(&[range("10.4.0.0/22")]).expect("a /22 is within the limits");
    assert!(!sweep.addresses().contains(&Ipv4Addr::new(10, 4, 0, 0)));
    assert!(!sweep.addresses().contains(&Ipv4Addr::new(10, 4, 3, 255)));
    assert_eq!(sweep.len(), 1022);
}

#[test]
fn a_31_is_a_point_to_point_link_and_both_addresses_are_real() {
    // RFC 3021. Skipping "the first and the last" here would skip the whole range, and a
    // /31 is what every router-to-router link in a modern estate is numbered with.
    let sweep = Sweep::new(&[range("10.0.0.0/31")]).expect("a /31 is within the limits");
    assert_eq!(sweep.len(), 2);
    let sweep = Sweep::new(&[range("10.0.0.7/32")]).expect("a /32 is within the limits");
    assert_eq!(sweep.len(), 1);
}

#[test]
fn host_bits_are_cleared_rather_than_refused() {
    // What an operator types when they mean "the network that host is on", and what every
    // router CLI does with it.
    assert_eq!(range("192.168.1.40/24"), range("192.168.1.0/24"));
}

#[test]
fn a_range_wider_than_a_16_is_refused_with_advice() {
    let error = Sweep::new(&[range("10.0.0.0/8")]).expect_err("a /8 must be refused");
    let sentence = error.to_string();
    assert!(
        matches!(error, SweepError::RangeTooWide { .. }),
        "a /8 is refused for being wide, not for being long: {error}"
    );
    // The acceptance criterion is not that it is refused. It is that the operator is told
    // what to do instead -- a message reading "invalid range" teaches nothing.
    assert!(
        sentence.contains("Split"),
        "the refusal must say what to do instead: {sentence}"
    );
}

#[test]
fn many_legal_ranges_are_still_capped_in_total() {
    // Ten /16s are ten legal ranges and 655 360 addresses. The cap is on the job, not on
    // the prettiest range in it.
    let ranges: Vec<Range> = (1..=10).map(|n| range(&format!("10.{n}.0.0/16"))).collect();
    let error = Sweep::new(&ranges).expect_err("ten /16s must be refused");
    assert!(
        matches!(error, SweepError::TooManyAddresses { .. }),
        "{error}"
    );
}

#[test]
fn a_16_is_exactly_the_limit_and_is_allowed() {
    // The boundary in the direction that matters: a /16 is MAX_ADDRESSES, and an
    // off-by-one here would refuse the largest thing the spec promises to accept.
    let sweep = Sweep::new(&[range("172.16.0.0/16")]).expect("a /16 is the documented limit");
    assert_eq!(
        u32::try_from(sweep.len()).expect("a sweep fits in a u32"),
        MAX_ADDRESSES - 2
    );
}

#[test]
fn a_job_with_no_ranges_scans_nothing_and_says_so() {
    assert_eq!(
        Sweep::new(&[]).expect_err("no ranges is refused"),
        SweepError::NoRanges
    );
}

#[test]
fn overlapping_ranges_probe_each_address_once() {
    // An operator who writes a supernet and one of its subnets has said something
    // redundant, not something wrong. Probing the overlap twice would double-count it in
    // `discovery_run.probed`, which is the number they use to judge their credentials.
    let sweep = Sweep::new(&[range("10.1.0.0/24"), range("10.1.0.0/25")])
        .expect("overlapping ranges are legal");
    assert_eq!(
        sweep.len(),
        254,
        "the /25 is inside the /24 and adds nothing"
    );
}

#[test]
fn ranges_are_probed_in_address_order() {
    // Not cosmetic. A sweep that jumps about the address space looks like a scan to every
    // IDS on the path, and one that walks it in order looks like an inventory tool --
    // which is what it is.
    let sweep = Sweep::new(&[range("10.9.0.0/24"), range("10.8.0.0/24")])
        .expect("two /24s are within the limits");
    assert!(
        sweep.addresses().windows(2).all(|w| w[0] < w[1]),
        "addresses must be probed in order"
    );
}

#[test]
fn a_prefix_longer_than_32_is_not_a_prefix() {
    assert!(matches!(
        "10.0.0.0/33".parse::<Range>(),
        Err(SweepError::PrefixTooLong { prefix: 33 })
    ));
}

#[test]
fn something_that_is_not_a_range_says_what_one_looks_like() {
    let error = "the office network"
        .parse::<Range>()
        .expect_err("not a range");
    assert!(
        error.to_string().contains("192.168.1.0/24"),
        "the message must show the shape: {error}"
    );
}
