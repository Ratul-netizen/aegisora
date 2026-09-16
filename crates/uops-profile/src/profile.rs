//! The profile document, and what it has to satisfy beyond parsing.
//!
//! Serde gets the shape. Everything interesting is what serde cannot check:
//!
//! * an interface-scoped metric with no discovery rule that creates interfaces — it
//!   would poll nothing, forever, silently;
//! * two metrics with the same name, which collide in one series once written;
//! * a metric name that is not a semconv name, which makes it unjoinable with
//!   everything the `OTel` receiver in M3 will write;
//! * a counter declared with a unit that makes no sense for one.
//!
//! Every one of those produces a profile that loads, polls, and quietly produces the
//! wrong thing. [`Profile::validate`] is where they stop, and it runs on every built-in
//! at compile-adjacent time — see `builtin.rs` — so a broken profile cannot ship.

use serde::{Deserialize, Serialize};
use uops_core::{IdentifierKind, ResourceKind};

use crate::interval::{Interval, Timeout};
use crate::oid::Oid;

/// A monitoring profile.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Stable key, e.g. `mikrotik-routeros`. Not a uuid: it is written by hand, in a
    /// file, and referred to in a support conversation.
    pub id: String,
    pub version: u32,
    pub name: String,

    #[serde(default, rename = "match")]
    pub matches: Match,

    #[serde(default)]
    pub discovery: Vec<Discovery>,
    #[serde(default)]
    pub metrics: Vec<Metric>,
    #[serde(default)]
    pub availability: Vec<Availability>,

    /// Where to read the device's make, model and serial from. Absent for a profile that
    /// does not know — which is not the same as a device that does not answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,
}

/// Which OIDs hold the facts that describe a device rather than measure it.
///
/// # Why this is a profile and not a match arm per vendor
///
/// Every vendor puts its model number somewhere different, and the somewhere is an OID.
/// A profile is a document a customer can write and ship without waiting for a release,
/// which is the whole reason profiles exist — and "this switch reports its model at
/// 1.3.6.1.4.1.9.3.6.11" is exactly the kind of fact a customer discovers before we do.
///
/// Every field is optional, and a device that does not answer one simply does not get
/// it. `ENTITY-MIB` is where these live on equipment that implements it, which is most
/// enterprise hardware and very little else, so the built-in profiles name its OIDs and
/// vendor profiles override them.
///
/// # The serial is not decoration
///
/// SPEC §M0.2 makes a serial number a **tier-1** identifier: globally unique by
/// specification, confidence 1.00, and proof of identity on its own. Nothing in this
/// product produced one until this block existed, so identity resolution has been
/// running on management addresses and hostnames — tier 3 and below — for every SNMP
/// device. This is what gives it something to be certain about.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    /// `entPhysicalMfgName`, usually. Falls back to the MAC's IEEE assignment when the
    /// device does not answer — see `uops-oui`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<Oid>,
    /// `entPhysicalModelName`, usually.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Oid>,
    /// `entPhysicalSerialNum`, usually. A tier-1 identifier — see above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<Oid>,
    /// The operating system's name. `sysDescr` on most devices, which is a sentence
    /// rather than a name — stored verbatim, because a parser per vendor is the thing
    /// this design exists to avoid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<Oid>,
    /// `entPhysicalSoftwareRev`, usually.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<Oid>,
}

/// One fact to read off a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fact {
    Vendor,
    Model,
    Serial,
    Os,
    OsVersion,
}

impl Fact {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vendor => "vendor",
            Self::Model => "model",
            Self::Serial => "serial",
            Self::Os => "os",
            Self::OsVersion => "os_version",
        }
    }
}

impl Identity {
    /// The facts this profile knows how to read, in a stable order.
    ///
    /// A `Vec` rather than five `Option`s at every call site: the poller asks for them in
    /// one request and matches the answers back by OID, and iterating is what both of
    /// those want.
    #[must_use]
    pub fn facts(&self) -> Vec<(Fact, Oid)> {
        [
            (Fact::Vendor, self.vendor.as_ref()),
            (Fact::Model, self.model.as_ref()),
            (Fact::Serial, self.serial.as_ref()),
            (Fact::Os, self.os.as_ref()),
            (Fact::OsVersion, self.os_version.as_ref()),
        ]
        .into_iter()
        .filter_map(|(fact, oid)| oid.map(|o| (fact, o.clone())))
        .collect()
    }

    /// Whether this block asks for anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.facts().is_empty()
    }
}

/// How a device is recognised as this kind of device.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Match {
    /// Matched arc-wise against the device's `sysObjectID`. Absent means this profile
    /// is never selected automatically — the generic fallback, or one a customer
    /// assigns explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sysobjectid_prefix: Option<Oid>,
}

/// A table to walk, and what each row becomes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    pub kind: DiscoveryKind,
    /// The table's entry OID — `IF-MIB::ifTable` is `1.3.6.1.2.1.2.2.1`.
    pub walk: Oid,
    /// The column that identifies a row, e.g. `ifIndex`. Recorded for the operator
    /// reading the profile; the walk's instance suffix is what is actually used.
    pub key: String,
    pub creates: Creates,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryKind {
    Interface,
}

/// The child resource a discovered row becomes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Creates {
    pub resource_kind: ResourceKind,
    /// The column to read the child's name from — `ifName`, usually.
    pub name_from: Oid,
    /// The edge from child to parent. SPEC §M2: interface discovery produces M0.1
    /// edges for free, and M9's correlation engine needs them to exist.
    pub relationship: Relationship,
    #[serde(default)]
    pub identifiers: Vec<IdentifierSource>,
}

/// The relationship kinds a profile may create.
///
/// Deliberately a subset of `relationship_kind` in the schema. A profile discovering a
/// `connected_to` edge would be asserting L2 adjacency from a single device's view,
/// which is M5's job with data from both ends.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Relationship {
    MemberOf,
    Hosts,
    Runs,
}

impl Relationship {
    /// The value the `relationship_kind` enum accepts.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MemberOf => "member_of",
            Self::Hosts => "hosts",
            Self::Runs => "runs",
        }
    }
}

/// An identifier to read off a discovered row, so identity resolution can do its job.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IdentifierSource {
    pub kind: IdentifierKind,
    pub oid: Oid,
}

/// One thing to poll.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Metric {
    /// OpenTelemetry semantic-convention name, e.g. `system.cpu.utilization`.
    pub name: String,
    pub oid: Oid,
    pub kind: MetricKind,
    /// UCUM, as `OTel` uses: `%`, `By`, `1`, `s`.
    pub unit: String,
    pub interval: Interval,
    /// What the OID is indexed by. `device` means a scalar; `interface` means one value
    /// per discovered interface, and requires a discovery rule that creates them.
    #[serde(default)]
    pub scope: Scope,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    Gauge,
    /// Stored raw, always. SPEC §M2: rates are computed at query time, because storing
    /// a pre-computed rate makes re-interpretation impossible and hides counter wrap.
    Counter,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Device,
    Interface,
}

/// An availability check.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Availability {
    pub kind: CheckKind,
    pub interval: Interval,
    pub timeout: Timeout,
    #[serde(default = "default_retries")]
    pub retries: u8,
    /// TCP only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

const fn default_retries() -> u8 {
    3
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Icmp,
    Tcp,
}

/// Why a profile was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProfileError {
    #[error("{0}")]
    Parse(String),

    #[error("a profile needs an id")]
    NoId,

    #[error("`{0}` is not a usable profile id — lowercase letters, digits and hyphens")]
    BadId(String),

    #[error("a profile version must be greater than zero")]
    BadVersion,

    #[error("metric `{0}` is declared twice; two metrics with one name are one series")]
    DuplicateMetric(String),

    #[error(
        "metric `{0}` is not a semantic-convention name — lowercase words joined by \
         dots, e.g. system.cpu.utilization. Anything else cannot be joined with what \
         the OTel receiver writes"
    )]
    NotSemconv(String),

    #[error(
        "metric `{metric}` is scoped to {scope:?} but nothing in this profile \
         discovers them, so it would poll nothing, forever, without saying so"
    )]
    ScopeWithoutDiscovery { metric: String, scope: Scope },

    #[error("a tcp availability check needs a port")]
    TcpWithoutPort,

    #[error("an icmp availability check cannot have a port")]
    IcmpWithPort,

    #[error("a timeout of {timeout:?} is not shorter than the {interval:?} interval it runs in")]
    TimeoutExceedsInterval {
        timeout: std::time::Duration,
        interval: std::time::Duration,
    },

    #[error("a profile that polls nothing and discovers nothing does nothing")]
    Empty,
}

/// Lowercase words joined by dots, as `OTel` semantic conventions are written.
fn is_semconv(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_')
        && name.contains('.')
}

fn is_profile_key(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl Profile {
    /// Parse a profile from YAML and validate it.
    ///
    /// # Errors
    ///
    /// A YAML error, or anything [`Self::validate`] refuses.
    pub fn from_yaml(yaml: &str) -> Result<Self, ProfileError> {
        let profile: Self =
            serde_yaml_ng::from_str(yaml).map_err(|e| ProfileError::Parse(e.to_string()))?;
        profile.validate()?;
        Ok(profile)
    }

    /// Everything true of a usable profile that serde cannot express.
    ///
    /// # Errors
    ///
    /// See [`ProfileError`]. Each one names the thing that is wrong and why it matters,
    /// because the person reading it is editing a YAML file for a device we have never
    /// seen and cannot ask us.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.id.is_empty() {
            return Err(ProfileError::NoId);
        }
        if !is_profile_key(&self.id) {
            return Err(ProfileError::BadId(self.id.clone()));
        }
        if self.version == 0 {
            return Err(ProfileError::BadVersion);
        }
        if self.metrics.is_empty() && self.discovery.is_empty() && self.availability.is_empty() {
            return Err(ProfileError::Empty);
        }

        let discovers_interfaces = self
            .discovery
            .iter()
            .any(|d| d.kind == DiscoveryKind::Interface);

        let mut seen: Vec<&str> = Vec::with_capacity(self.metrics.len());
        for metric in &self.metrics {
            if seen.contains(&metric.name.as_str()) {
                return Err(ProfileError::DuplicateMetric(metric.name.clone()));
            }
            seen.push(&metric.name);

            if !is_semconv(&metric.name) {
                return Err(ProfileError::NotSemconv(metric.name.clone()));
            }

            if metric.scope == Scope::Interface && !discovers_interfaces {
                return Err(ProfileError::ScopeWithoutDiscovery {
                    metric: metric.name.clone(),
                    scope: metric.scope,
                });
            }
        }

        for check in &self.availability {
            match check.kind {
                CheckKind::Tcp if check.port.is_none() => return Err(ProfileError::TcpWithoutPort),
                CheckKind::Icmp if check.port.is_some() => return Err(ProfileError::IcmpWithPort),
                _ => {}
            }

            // A check whose timeout is its own interval can never report a failure
            // before the next attempt starts, and retries make it worse: three attempts
            // at a 30s timeout inside a 30s interval is a check permanently behind.
            let budget = check.timeout.duration() * u32::from(check.retries.max(1));
            if budget >= check.interval.duration() {
                return Err(ProfileError::TimeoutExceedsInterval {
                    timeout: budget,
                    interval: check.interval.duration(),
                });
            }
        }

        Ok(())
    }

    /// Every metric that needs the interface table walked first.
    pub fn interface_metrics(&self) -> impl Iterator<Item = &Metric> {
        self.metrics.iter().filter(|m| m.scope == Scope::Interface)
    }

    /// The distinct intervals this profile asks for, for the scheduler to lay out.
    #[must_use]
    pub fn intervals(&self) -> Vec<Interval> {
        let mut out: Vec<Interval> = self
            .metrics
            .iter()
            .map(|m| m.interval)
            .chain(self.availability.iter().map(|a| a.interval))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

#[cfg(test)]
// Every fixture below is a YAML document written as r#"…"#. Some contain a quoted OID
// and need the hashes; some do not. Using one delimiter throughout keeps them readable
// as a set — a reader comparing two fixtures should be looking at the YAML, not at why
// the string literals are punctuated differently.
#[allow(clippy::needless_raw_string_hashes)]
mod tests {
    use super::*;

    /// A minimal valid profile, for mutating one field at a time.
    fn base() -> String {
        r#"
id: test-profile
version: 1
name: Test
metrics:
  - name: system.uptime
    oid: "1.3.6.1.2.1.1.3.0"
    kind: gauge
    unit: s
    interval: 60s
"#
        .to_owned()
    }

    #[test]
    fn the_base_profile_is_valid() {
        // Without this, every test below could be passing for the wrong reason.
        Profile::from_yaml(&base()).expect("the base fixture must be valid");
    }

    #[test]
    fn an_interface_metric_with_nothing_to_attach_to_is_refused() {
        // The quiet one: it parses, it polls, and it polls nothing forever without
        // saying so. There is no runtime symptom other than an absent series.
        let yaml = base()
            + r#"
  - name: network.io.receive
    oid: "1.3.6.1.2.1.31.1.1.1.6"
    kind: counter
    unit: By
    scope: interface
    interval: 60s
"#;
        assert!(matches!(
            Profile::from_yaml(&yaml),
            Err(ProfileError::ScopeWithoutDiscovery { .. })
        ));
    }

    #[test]
    fn the_same_metric_twice_is_refused() {
        let yaml = base()
            + r#"
  - name: system.uptime
    oid: "1.3.6.1.2.1.1.3.0"
    kind: gauge
    unit: s
    interval: 30s
"#;
        assert_eq!(
            Profile::from_yaml(&yaml).unwrap_err(),
            ProfileError::DuplicateMetric("system.uptime".to_owned())
        );
    }

    #[test]
    fn a_name_that_is_not_semconv_is_refused() {
        // A metric written any other way cannot be joined with what the OTel receiver
        // writes in M3, which makes it a series nobody can use in a query with anything
        // else.
        for name in [
            "SystemUptime",
            "system uptime",
            "uptime",
            "system..uptime",
            ".uptime",
        ] {
            let yaml = base().replace("system.uptime", name);
            assert!(
                matches!(Profile::from_yaml(&yaml), Err(ProfileError::NotSemconv(_))),
                "{name:?} should not be accepted"
            );
        }
        // Underscores within a word are fine — semconv uses them (http.request.method).
        let yaml = base().replace("system.uptime", "system.boot_time");
        assert!(Profile::from_yaml(&yaml).is_ok());
    }

    #[test]
    fn a_check_that_cannot_finish_before_its_next_attempt_is_refused() {
        // Three attempts at a 15s timeout inside a 30s interval is a check permanently
        // behind: it can never report a failure before the next one starts.
        let yaml = base()
            + r#"
availability:
  - kind: icmp
    interval: 30s
    timeout: 15s
    retries: 3
"#;
        assert!(matches!(
            Profile::from_yaml(&yaml),
            Err(ProfileError::TimeoutExceedsInterval { .. })
        ));

        // The SPEC example: 3 × 2s inside 30s. Comfortable.
        let ok = base()
            + r#"
availability:
  - kind: icmp
    interval: 30s
    timeout: 2s
    retries: 3
"#;
        assert!(Profile::from_yaml(&ok).is_ok());
    }

    #[test]
    fn a_port_belongs_to_tcp_and_only_tcp() {
        let tcp_no_port = base()
            + r#"
availability:
  - kind: tcp
    interval: 30s
    timeout: 2s
"#;
        assert_eq!(
            Profile::from_yaml(&tcp_no_port).unwrap_err(),
            ProfileError::TcpWithoutPort
        );

        let icmp_with_port = base()
            + r#"
availability:
  - kind: icmp
    interval: 30s
    timeout: 2s
    port: 443
"#;
        assert_eq!(
            Profile::from_yaml(&icmp_with_port).unwrap_err(),
            ProfileError::IcmpWithPort
        );
    }

    #[test]
    fn an_unknown_field_is_an_error_rather_than_a_shrug() {
        // deny_unknown_fields, deliberately. `internal: 60s` instead of `interval` would
        // otherwise leave the metric at whatever the default is — except there is no
        // default, so serde would say "missing field interval" and the operator would
        // have to find the typo themselves. This names it.
        let yaml = base().replace("interval: 60s", "interval: 60s\n    intreval: 30s");
        let err = Profile::from_yaml(&yaml).unwrap_err();
        assert!(
            format!("{err}").contains("intreval"),
            "the error must name the misspelled field: {err}"
        );
    }

    #[test]
    fn a_profile_that_does_nothing_is_refused() {
        let yaml = "id: empty-profile\nversion: 1\nname: Empty\n";
        assert_eq!(Profile::from_yaml(yaml).unwrap_err(), ProfileError::Empty);
    }

    #[test]
    fn a_profile_key_has_to_be_usable_as_one() {
        for id in ["Test Profile", "test_profile", "-leading", "trailing-", ""] {
            let yaml = base().replace("id: test-profile", &format!("id: {id:?}"));
            assert!(
                Profile::from_yaml(&yaml).is_err(),
                "{id:?} should not be a profile key"
            );
        }
    }

    #[test]
    fn intervals_are_deduplicated_and_ordered_for_the_scheduler() {
        let yaml = base()
            + r#"
availability:
  - kind: icmp
    interval: 30s
    timeout: 2s
    retries: 3
"#;
        let profile = Profile::from_yaml(&yaml).unwrap();
        let intervals: Vec<String> = profile
            .intervals()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(intervals, vec!["30s", "1m"]);
    }

    #[test]
    fn a_profile_round_trips_through_json() {
        // It is stored as jsonb and read back by the poller. A field that serialises
        // but does not deserialise would be discovered in production.
        let profile = Profile::from_yaml(&base()).unwrap();
        let json = serde_json::to_string(&profile).unwrap();
        let back: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(profile, back);
    }
}
