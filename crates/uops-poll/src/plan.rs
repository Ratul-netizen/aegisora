//! Turning a device and its profile into scheduled work.
//!
//! This is the join between the four things that already exist — a resource row, a
//! profile, the wheel and the executor — and it is where one decision determines whether
//! the scale in SPEC §M2 is reachable.
//!
//! # A job is a device and an interval, not a device and a metric
//!
//! SPEC states the problem as arithmetic:
//!
//! > 10 000 devices × 20 metrics × 60s means 200 k tasks/minute; spawning per-poll will
//! > thrash.
//!
//! The time wheel fixes the *spawning*. It does not fix the arithmetic: 200 000 wheel
//! entries a minute is still 200 000 SNMP requests a minute, to devices whose agents
//! have queues measured in single digits.
//!
//! Twenty metrics on one device at one interval are twenty OIDs, and SNMP has had a way
//! to ask for several OIDs in one request since v1. So the unit of work is `(device,
//! interval)`, carrying every OID due at that moment. The same fleet becomes 10 000
//! requests a minute — a twentieth of the traffic, and a twentieth of the load on each
//! agent, for no loss of data.
//!
//! The grouping is by interval and not simply per device, because a profile that polls
//! CPU every 60 s and a memory total every 5 minutes means it: collapsing those would
//! either poll the slow one twenty times too often or the fast one twenty times too
//! rarely.
//!
//! # Interface-scoped metrics are a separate job
//!
//! A device-scoped OID is a scalar and is fetched. An interface-scoped one is a column
//! and must be walked, because the set of interfaces is not known until it is. Same
//! device, same interval, different request shape — so a different job, and one that
//! depends on discovery having run.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use uops_core::{CredentialRef, ResourceId, SiteId, TenantId};
use uops_profile::{Fact, Oid, Profile, Scope};

/// A device the poller can reach.
///
/// Assembled from a `resource` row plus its `mgmt_ip` identifier and `credential_ref`.
/// A resource with no address is not a device, it is a row: there is no default address
/// and the caller that builds these has to skip it rather than invent one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub tenant: TenantId,
    pub resource: ResourceId,
    pub site: SiteId,
    pub address: SocketAddr,
    /// Absent means v1/v2c with no stored credential, which the poller refuses — see
    /// `uops_snmp::credential`. Kept as an `Option` because the column is nullable and
    /// pretending otherwise would move the problem rather than solve it.
    pub credential: Option<CredentialRef>,
}

/// What a job asks the device for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Work {
    /// Scalar OIDs, fetched together in one request.
    Scalars {
        /// `(metric name, OID, unit)`, in profile order.
        metrics: Vec<MetricRequest>,
    },
    /// One column per metric, walked across every interface.
    InterfaceColumns { metrics: Vec<MetricRequest> },
    /// The discovery walk that interface-scoped work depends on.
    Discovery { table: Oid },
    /// Read what the device *is* — make, model, serial, software — rather than how it is
    /// doing. See `uops_profile::Identity`.
    Identity { facts: Vec<(Fact, Oid)> },
    /// An availability check.
    Availability { index: usize },
}

/// One metric to read, and enough to record it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetricRequest {
    /// The semconv name it is stored under.
    pub name: String,
    pub oid: Oid,
    pub unit: String,
    pub counter: bool,
}

/// One scheduled unit of work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub device: ResourceId,
    pub interval: Duration,
    pub work: Work,
}

impl Job {
    /// A stable key for the wheel, and the seed its jitter is derived from.
    ///
    /// Stable across restarts: the same device's same job lands in the same place after
    /// a deploy, rather than the whole fleet reshuffling into fresh collisions. See
    /// [`crate::wheel`].
    #[must_use]
    pub fn seed(&self) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.device.into_uuid().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        for byte in self.discriminant().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        hash ^= self.interval.as_secs();
        hash
    }

    /// What distinguishes two jobs on the same device at the same interval.
    fn discriminant(&self) -> String {
        match &self.work {
            Work::Scalars { .. } => "scalars".to_owned(),
            Work::InterfaceColumns { .. } => "interfaces".to_owned(),
            Work::Discovery { .. } => "discovery".to_owned(),
            Work::Identity { .. } => "identity".to_owned(),
            Work::Availability { index } => format!("availability:{index}"),
        }
    }
}

/// How often a device's interface table is re-walked.
///
/// Not from the profile: discovery is not a metric and profiles do not give it an
/// interval. Fifteen minutes is a compromise — an interface added to a switch appears
/// within a quarter of an hour, and a 48-port table is not walked every minute for the
/// benefit of a change that happens twice a year.
pub const DISCOVERY_INTERVAL: Duration = Duration::from_mins(15);

/// Expand a device and its profile into everything that must be scheduled.
///
/// Jobs come back in a deterministic order — scalars, interfaces, discovery,
/// availability, each by ascending interval — so two runs of the planner produce the
/// same list and a diff of the schedule is readable.
#[must_use]
pub fn plan(device: &Device, profile: &Profile) -> Vec<Job> {
    let mut jobs = Vec::new();

    // Group by interval. The whole point — see the module docs.
    let mut scalars: BTreeMap<u64, Vec<MetricRequest>> = BTreeMap::new();
    let mut columns: BTreeMap<u64, Vec<MetricRequest>> = BTreeMap::new();

    for metric in &profile.metrics {
        let request = MetricRequest {
            name: metric.name.clone(),
            oid: metric.oid.clone(),
            unit: metric.unit.clone(),
            counter: metric.kind == uops_profile::MetricKind::Counter,
        };
        let secs = metric.interval.duration().as_secs();
        match metric.scope {
            Scope::Device => scalars.entry(secs).or_default().push(request),
            Scope::Interface => columns.entry(secs).or_default().push(request),
        }
    }

    for (secs, metrics) in scalars {
        jobs.push(Job {
            device: device.resource,
            interval: Duration::from_secs(secs),
            work: Work::Scalars { metrics },
        });
    }

    let has_interface_work = !columns.is_empty();
    for (secs, metrics) in columns {
        jobs.push(Job {
            device: device.resource,
            interval: Duration::from_secs(secs),
            work: Work::InterfaceColumns { metrics },
        });
    }

    // A declared discovery rule is scheduled whether or not any metric is scoped to
    // what it finds: the interfaces become child resources either way, and SPEC §M2
    // asks for those explicitly. `has_interface_work` is not a condition — it is a
    // consistency check, because Profile::validate refuses the other combination.
    debug_assert!(
        !has_interface_work || !profile.discovery.is_empty(),
        "validate() should have refused interface metrics with no discovery"
    );
    // On the discovery interval, not a metric one. A device's model number changes when
    // somebody swaps the hardware, which is the same cadence `sysObjectID` is cached at
    // and about as often as an interface is added.
    if let Some(identity) = profile.identity.as_ref().filter(|i| !i.is_empty()) {
        jobs.push(Job {
            device: device.resource,
            interval: DISCOVERY_INTERVAL,
            work: Work::Identity {
                facts: identity.facts(),
            },
        });
    }

    if let Some(rule) = profile.discovery.first() {
        jobs.push(Job {
            device: device.resource,
            interval: DISCOVERY_INTERVAL,
            work: Work::Discovery {
                table: rule.walk.clone(),
            },
        });
    }

    for (index, check) in profile.availability.iter().enumerate() {
        jobs.push(Job {
            device: device.resource,
            interval: check.interval.duration(),
            work: Work::Availability { index },
        });
    }

    jobs
}

/// How many requests a minute a fleet of `devices` will make under `profile`.
///
/// For a capacity conversation before the fleet exists, and for the test that pins the
/// grouping: without it the answer is twenty times larger, which is the difference
/// between a poller and an outage.
#[must_use]
pub fn requests_per_minute(jobs: &[Job], devices: usize) -> f64 {
    let per_device: f64 = jobs.iter().map(|j| 60.0 / j.interval.as_secs_f64()).sum();
    // A fleet size beyond 2^53 is not a thing, and the alternative is a fallible
    // conversion on a number that is being printed in a capacity conversation.
    #[allow(clippy::cast_precision_loss)]
    let fleet = devices as f64;
    per_device * fleet
}

#[cfg(test)]
mod tests {
    use super::*;
    use uops_profile::builtin;

    fn device() -> Device {
        Device {
            tenant: TenantId::new(),
            resource: ResourceId::new(),
            site: SiteId::new(),
            address: "10.0.0.1:161".parse().unwrap(),
            credential: None,
        }
    }

    fn profile(key: &str) -> Profile {
        builtin::all()
            .unwrap()
            .into_iter()
            .find(|p| p.id == key)
            .expect("built-in profile")
    }

    #[test]
    fn metrics_at_one_interval_become_one_job() {
        // The decision this module exists for. generic-snmp has four interface metrics
        // and one scalar, all at 60s — five OIDs, two jobs, not five.
        let jobs = plan(&device(), &profile("generic-snmp"));

        let scalars: Vec<&Job> = jobs
            .iter()
            .filter(|j| matches!(j.work, Work::Scalars { .. }))
            .collect();
        assert_eq!(
            scalars.len(),
            1,
            "one scalar job at one interval: {jobs:#?}"
        );

        let columns: Vec<&Job> = jobs
            .iter()
            .filter(|j| matches!(j.work, Work::InterfaceColumns { .. }))
            .collect();
        assert_eq!(columns.len(), 1, "one interface job at one interval");

        let Work::InterfaceColumns { metrics } = &columns[0].work else {
            unreachable!()
        };
        assert_eq!(
            metrics.len(),
            4,
            "all four interface metrics in one request"
        );
    }

    #[test]
    fn different_intervals_stay_different_jobs() {
        // linux-snmp polls memory total every 5 minutes and everything else every
        // minute. Collapsing them would poll one twenty times too often or the other
        // twenty times too rarely.
        let jobs = plan(&device(), &profile("linux-snmp"));
        let scalar_intervals: Vec<u64> = jobs
            .iter()
            .filter(|j| matches!(j.work, Work::Scalars { .. }))
            .map(|j| j.interval.as_secs())
            .collect();
        assert_eq!(scalar_intervals, vec![60, 300], "{jobs:#?}");
    }

    #[test]
    fn the_grouping_is_what_makes_the_spec_arithmetic_work() {
        // SPEC: "10 000 devices × 20 metrics × 60s means 200 k tasks/minute". The claim
        // is that grouping by interval turns metric requests into one per interval.
        //
        // Compared against metric requests only. An earlier version of this test put
        // the grouped total — which includes availability and discovery — against
        // ungrouped *metrics*, which is not a comparison: it charged one side for work
        // the other was never doing, and understated the saving as 1.7x.
        let p = profile("linux-snmp");
        let jobs = plan(&device(), &p);

        let metric_jobs: Vec<Job> = jobs
            .iter()
            .filter(|j| matches!(j.work, Work::Scalars { .. } | Work::InterfaceColumns { .. }))
            .cloned()
            .collect();

        let grouped = requests_per_minute(&metric_jobs, 10_000);
        let ungrouped: f64 = p
            .metrics
            .iter()
            .map(|m| 60.0 / m.interval.as_secs_f64())
            .sum::<f64>()
            * 10_000.0;

        // linux-snmp: eight metrics across two intervals and two scopes, so 72 000
        // requests a minute becomes 22 000. SPEC's example — twenty metrics at one
        // interval — is a twentyfold saving; this profile is more varied and saves
        // threefold, which is the same mechanism at a different shape.
        assert!(
            grouped * 3.0 <= ungrouped,
            "grouping saved too little to matter: {grouped} vs {ungrouped}"
        );

        // And the whole schedule, availability and discovery included, still fits.
        let everything = requests_per_minute(&jobs, 10_000);
        assert!(
            everything < 60_000.0,
            "{everything} requests/minute for 10 000 devices is too many"
        );
    }

    #[test]
    fn availability_checks_are_their_own_jobs() {
        // A 30s ICMP check alongside 60s metrics. Folding it into a metric job would
        // halve its rate or double theirs.
        let jobs = plan(&device(), &profile("generic-snmp"));
        let checks: Vec<&Job> = jobs
            .iter()
            .filter(|j| matches!(j.work, Work::Availability { .. }))
            .collect();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].interval, Duration::from_secs(30));
    }

    #[test]
    fn discovery_is_scheduled_and_is_not_a_metric_interval() {
        // Profiles give no interval for discovery because it is not a metric. Walking a
        // 48-port table every minute for a change that happens twice a year is waste;
        // never walking it means an interface added today appears never.
        let jobs = plan(&device(), &profile("cisco-ios"));
        let discovery: Vec<&Job> = jobs
            .iter()
            .filter(|j| matches!(j.work, Work::Discovery { .. }))
            .collect();
        assert_eq!(discovery.len(), 1);
        assert_eq!(discovery[0].interval, DISCOVERY_INTERVAL);
    }

    #[test]
    fn every_built_in_profile_plans_to_something_schedulable() {
        // Each job's interval must fit the wheel the poller uses — an hour — or it
        // cannot be scheduled at all, and the failure would be at startup on a
        // customer's site rather than here.
        for p in builtin::all().unwrap() {
            let jobs = plan(&device(), &p);
            assert!(!jobs.is_empty(), "{} planned nothing", p.id);
            for job in &jobs {
                assert!(
                    job.interval >= Duration::from_secs(1)
                        && job.interval <= Duration::from_secs(3600),
                    "{} has a job at {:?}, outside the wheel",
                    p.id,
                    job.interval
                );
            }
        }
    }

    #[test]
    fn a_counter_is_carried_through_as_one() {
        // The poller needs to know, because a counter is stored raw and a gauge is not
        // — and getting it backwards makes every rate wrong in a way no test of the
        // storage layer would catch.
        let jobs = plan(&device(), &profile("generic-snmp"));
        let Work::InterfaceColumns { metrics } = &jobs
            .iter()
            .find(|j| matches!(j.work, Work::InterfaceColumns { .. }))
            .unwrap()
            .work
        else {
            unreachable!()
        };
        assert!(
            metrics.iter().all(|m| m.counter),
            "every interface metric in generic-snmp is a counter"
        );

        let Work::Scalars { metrics } = &jobs
            .iter()
            .find(|j| matches!(j.work, Work::Scalars { .. }))
            .unwrap()
            .work
        else {
            unreachable!()
        };
        assert!(
            metrics.iter().all(|m| !m.counter),
            "sysUpTime is a gauge in this profile"
        );
    }

    #[test]
    fn a_jobs_seed_is_stable_and_distinct() {
        // Stable, or every deploy reshuffles the fleet into fresh collisions. Distinct,
        // or two jobs on one device land in the same slot forever.
        let d = device();
        let jobs = plan(&d, &profile("generic-snmp"));
        let again = plan(&d, &profile("generic-snmp"));

        let seeds: Vec<u64> = jobs.iter().map(Job::seed).collect();
        let seeds_again: Vec<u64> = again.iter().map(Job::seed).collect();
        assert_eq!(seeds, seeds_again, "a seed must survive a restart");

        let mut unique = seeds.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            seeds.len(),
            "two jobs share a seed: {seeds:?}"
        );

        // And a different device is a different schedule.
        let other = plan(&device(), &profile("generic-snmp"));
        assert_ne!(
            other.iter().map(Job::seed).collect::<Vec<_>>(),
            seeds,
            "two devices must not poll in lockstep"
        );
    }

    #[test]
    fn a_profile_with_no_metrics_still_plans_its_checks() {
        // A device that is only pinged. Planning nothing would silently stop monitoring
        // it, which is the failure mode that matters most here.
        let yaml = r"
id: ping-only
version: 1
name: Ping only
availability:
  - kind: icmp
    interval: 30s
    timeout: 2s
    retries: 3
";
        let p = Profile::from_yaml(yaml).unwrap();
        let jobs = plan(&device(), &p);
        assert_eq!(jobs.len(), 1);
        assert!(matches!(jobs[0].work, Work::Availability { .. }));
    }
}
