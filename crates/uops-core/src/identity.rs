//! Resource identity resolution.
//!
//! SPEC §M0.2. This is the highest-leverage code in the system: when `router-01` appears
//! in SNMP, syslog, `NetFlow`, LLDP, a config backup and an alert, all six must resolve to
//! one [`ResourceId`](crate::ResourceId). Correlation, blast radius and the Investigation
//! Workspace are all downstream of getting this right.
//!
//! Two rules do the work, and both are deliberately conservative, because a wrong
//! auto-merge silently corrupts every downstream correlation while a review-queue entry
//! costs a human ten seconds:
//!
//! 1. **Noisy-OR combination** — independent matches compound as `1 − Π(1 − cᵢ)`.
//! 2. **Tier-1 contradiction** — two disagreeing globally-unique identifiers force a
//!    *new* resource, because the hardware was replaced.

use serde::{Deserialize, Serialize};

use crate::ids::{ResourceId, SiteId};

/// A kind of observed identifier, ordered by how much it can be trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
// Maps onto the `identifier_kind` PostgreSQL enum. The variant names and the enum
// labels are one list in two places; a mismatch fails at the first insert, on the
// ingestion path.
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(
    feature = "sqlx",
    sqlx(type_name = "identifier_kind", rename_all = "snake_case")
)]
pub enum IdentifierKind {
    // --- Tier 1: globally unique by specification ---
    /// `entPhysicalSerialNum`.
    Serial,
    /// LLDP chassis ID.
    ChassisId,
    /// `SNMPv3` engine ID — unique per agent.
    SnmpEngineId,
    /// OpenTelemetry `host.id` (machine ID).
    OtelHostId,

    // --- Tier 2 ---
    /// Unique, but NICs move between chassis.
    Mac,

    // --- Tier 3: addresses, which DHCP and re-IP break ---
    MgmtIp,
    /// NetFlow/IPFIX exporter address.
    FlowExporter,

    // --- Tier 4: names, which are truncated, duplicated and lied about ---
    /// `sysName`, syslog HOSTNAME, or `OTel` `host.name`.
    Hostname,
    /// `OTel` `service.name` — often maps to many resources, not one host.
    ServiceName,
}

impl IdentifierKind {
    /// Base confidence that a match on this identifier alone identifies the resource.
    ///
    /// Values are from the table in SPEC §M0.2 and are the tuning surface for the whole
    /// resolver — change them here, nowhere else.
    #[must_use]
    pub const fn base_confidence(self) -> f32 {
        match self {
            Self::Serial | Self::ChassisId | Self::SnmpEngineId | Self::OtelHostId => 1.00,
            Self::Mac => 0.90,
            Self::MgmtIp | Self::FlowExporter => 0.80,
            Self::Hostname => 0.65,
            Self::ServiceName => 0.60,
        }
    }

    /// Tier 1 identifiers are globally unique by specification. Two of them disagreeing
    /// is not low confidence — it is proof of a *different* resource.
    #[must_use]
    pub const fn is_tier_one(self) -> bool {
        matches!(
            self,
            Self::Serial | Self::ChassisId | Self::SnmpEngineId | Self::OtelHostId
        )
    }

    /// Stable string for the `identifier_kind` PostgreSQL enum.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Serial => "serial",
            Self::ChassisId => "chassis_id",
            Self::SnmpEngineId => "snmp_engine_id",
            Self::OtelHostId => "otel_host_id",
            Self::Mac => "mac",
            Self::MgmtIp => "mgmt_ip",
            Self::FlowExporter => "flow_exporter",
            Self::Hostname => "hostname",
            Self::ServiceName => "service_name",
        }
    }
}

/// Confidence at or above which a match is merged automatically.
pub const AUTO_MERGE_THRESHOLD: f32 = 0.95;
/// Confidence below which a new resource is created without review.
pub const REVIEW_FLOOR: f32 = 0.60;

/// One `(kind, value)` pair as observed by a collector.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identifier {
    pub kind: IdentifierKind,
    pub value: String,
}

impl Identifier {
    pub fn new(kind: IdentifierKind, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}

/// Everything a collector knows about who produced a signal, before resolution.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObservedIdentity {
    pub identifiers: Vec<Identifier>,
    /// Which collector observed this — recorded on the decision for auditability.
    pub source: String,
    /// Narrows candidates when the site is already known.
    pub site_hint: Option<SiteId>,
}

impl ObservedIdentity {
    #[must_use]
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            identifiers: Vec::new(),
            source: source.into(),
            site_hint: None,
        }
    }

    #[must_use]
    pub fn with(mut self, kind: IdentifierKind, value: impl Into<String>) -> Self {
        self.identifiers.push(Identifier::new(kind, value));
        self
    }
}

/// One identifier that matched an existing resource.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Match {
    pub kind: IdentifierKind,
    pub value: String,
    pub confidence: f32,
}

/// What the resolver decided, and why.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Resolution {
    /// Confident enough to attach telemetry to an existing resource.
    Matched {
        resource_id: ResourceId,
        confidence: f32,
        matched_by: Vec<Match>,
    },
    /// Plausible but not certain. A provisional resource is created so ingestion is
    /// never blocked, and a human decides later.
    Review {
        provisional_id: ResourceId,
        candidates: Vec<Candidate>,
    },
    /// Nothing matched, or a tier-1 contradiction proved this is different hardware.
    Created { resource_id: ResourceId },
}

/// A possible match presented to a reviewer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub resource_id: ResourceId,
    pub confidence: f32,
    pub matched_by: Vec<Match>,
    /// Set when tier-1 identifiers disagreed. Explains why this was not auto-merged
    /// despite otherwise strong evidence.
    pub contradiction: Option<Contradiction>,
}

/// Two tier-1 identifiers of the same kind with different values.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Contradiction {
    pub kind: IdentifierKind,
    pub observed: String,
    pub existing: String,
}

/// Combine independent match confidences as noisy-OR: `1 − Π(1 − cᵢ)`.
///
/// Each additional independent signal reduces the remaining doubt proportionally, so
/// weak evidence accumulates without any single weak identifier being able to trigger
/// an auto-merge on its own.
///
/// ```
/// # use uops_core::identity::combine_confidence;
/// // mgmt_ip (0.80) alone is not enough
/// assert!(combine_confidence(&[0.80]) < 0.95);
/// // plus hostname (0.65) is still not enough — 0.93
/// assert!(combine_confidence(&[0.80, 0.65]) < 0.95);
/// // plus mac (0.90) clears the bar — 0.993
/// assert!(combine_confidence(&[0.80, 0.65, 0.90]) > 0.95);
/// ```
#[must_use]
pub fn combine_confidence(confidences: &[f32]) -> f32 {
    let doubt = confidences
        .iter()
        .fold(1.0_f32, |acc, &c| acc * (1.0 - c.clamp(0.0, 1.0)));
    (1.0 - doubt).clamp(0.0, 1.0)
}

/// Decide the outcome for a candidate given its matches and any contradiction.
///
/// Split out from storage so the rules are unit-testable without a database — these are
/// the rules that decide whether two telemetry streams describe one machine.
#[must_use]
pub fn classify(matches: &[Match], contradiction: Option<&Contradiction>) -> Outcome {
    // A tier-1 contradiction is decisive regardless of other evidence: the same
    // management IP with a different serial means the box was physically replaced.
    // Inheriting the predecessor's history would corrupt every trend and incident
    // attached to it.
    if let Some(c) = contradiction
        && c.kind.is_tier_one()
    {
        return Outcome::CreateNew {
            reason: OutcomeReason::TierOneContradiction,
        };
    }

    if matches.is_empty() {
        return Outcome::CreateNew {
            reason: OutcomeReason::NoMatch,
        };
    }

    let confidences: Vec<f32> = matches.iter().map(|m| m.confidence).collect();
    let combined = combine_confidence(&confidences);

    if combined >= AUTO_MERGE_THRESHOLD {
        Outcome::AutoMerge {
            confidence: combined,
        }
    } else if combined >= REVIEW_FLOOR {
        Outcome::Review {
            confidence: combined,
        }
    } else {
        Outcome::CreateNew {
            reason: OutcomeReason::BelowFloor,
        }
    }
}

/// The rule engine's verdict, before it is turned into a [`Resolution`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    AutoMerge { confidence: f32 },
    Review { confidence: f32 },
    CreateNew { reason: OutcomeReason },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutcomeReason {
    /// Two globally-unique identifiers disagreed: different hardware.
    TierOneContradiction,
    /// No identifier matched anything known.
    NoMatch,
    /// Matched, but too weakly to be worth a human's attention.
    BelowFloor,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(kind: IdentifierKind) -> Match {
        Match {
            kind,
            value: "x".into(),
            confidence: kind.base_confidence(),
        }
    }

    #[test]
    fn tier_one_alone_auto_merges() {
        let out = classify(&[m(IdentifierKind::Serial)], None);
        assert!(matches!(out, Outcome::AutoMerge { .. }), "{out:?}");
    }

    #[test]
    fn worked_example_from_the_spec() {
        // mgmt_ip + hostname = 0.93 → review, NOT auto-merge.
        let c = combine_confidence(&[0.80, 0.65]);
        assert!((c - 0.93).abs() < 0.005, "expected ~0.93, got {c}");
        assert!(matches!(
            classify(
                &[m(IdentifierKind::MgmtIp), m(IdentifierKind::Hostname)],
                None
            ),
            Outcome::Review { .. }
        ));

        // adding mac = 0.993 → auto-merge.
        let c = combine_confidence(&[0.80, 0.65, 0.90]);
        assert!((c - 0.993).abs() < 0.005, "expected ~0.993, got {c}");
        assert!(matches!(
            classify(
                &[
                    m(IdentifierKind::MgmtIp),
                    m(IdentifierKind::Hostname),
                    m(IdentifierKind::Mac)
                ],
                None
            ),
            Outcome::AutoMerge { .. }
        ));
    }

    #[test]
    fn hostname_alone_goes_to_review_never_to_merge() {
        // Hostnames are duplicated and lied about constantly. On its own, a hostname
        // must never be enough to merge two telemetry streams.
        assert!(matches!(
            classify(&[m(IdentifierKind::Hostname)], None),
            Outcome::Review { .. }
        ));
    }

    #[test]
    fn service_name_alone_is_below_the_floor_for_merging() {
        let c = combine_confidence(&[IdentifierKind::ServiceName.base_confidence()]);
        assert!(c < AUTO_MERGE_THRESHOLD);
    }

    #[test]
    fn tier_one_contradiction_beats_any_amount_of_other_evidence() {
        // Same IP, same MAC, same hostname — but a different serial. The chassis was
        // swapped. Creating a new resource is correct; merging would attach the new
        // hardware's telemetry to the old one's history.
        let strong = vec![
            m(IdentifierKind::MgmtIp),
            m(IdentifierKind::Mac),
            m(IdentifierKind::Hostname),
        ];
        assert!(
            combine_confidence(&strong.iter().map(|x| x.confidence).collect::<Vec<_>>())
                > AUTO_MERGE_THRESHOLD,
            "precondition: this evidence would otherwise auto-merge"
        );

        let out = classify(
            &strong,
            Some(&Contradiction {
                kind: IdentifierKind::Serial,
                observed: "FTX-NEW".into(),
                existing: "FTX-OLD".into(),
            }),
        );
        assert_eq!(
            out,
            Outcome::CreateNew {
                reason: OutcomeReason::TierOneContradiction
            }
        );
    }

    #[test]
    fn non_tier_one_disagreement_does_not_force_a_new_resource() {
        // A changed management IP is routine (DHCP, re-IP). It must not be treated
        // the way a changed serial is.
        let out = classify(
            &[m(IdentifierKind::Serial)],
            Some(&Contradiction {
                kind: IdentifierKind::MgmtIp,
                observed: "10.0.0.9".into(),
                existing: "10.0.0.8".into(),
            }),
        );
        assert!(matches!(out, Outcome::AutoMerge { .. }), "{out:?}");
    }

    #[test]
    fn no_matches_creates() {
        assert_eq!(
            classify(&[], None),
            Outcome::CreateNew {
                reason: OutcomeReason::NoMatch
            }
        );
    }

    #[test]
    fn combination_is_order_independent_and_monotonic() {
        let a = combine_confidence(&[0.8, 0.65, 0.9]);
        let b = combine_confidence(&[0.9, 0.8, 0.65]);
        assert!((a - b).abs() < f32::EPSILON, "noisy-OR must be commutative");

        // More evidence can never lower confidence.
        assert!(combine_confidence(&[0.8, 0.65]) >= combine_confidence(&[0.8]));
    }

    #[test]
    fn confidence_stays_in_range() {
        assert!((combine_confidence(&[]) - 0.0).abs() < f32::EPSILON);
        assert!(combine_confidence(&[1.0, 1.0, 1.0]) <= 1.0);
        // Out-of-range inputs are clamped rather than producing a nonsense result.
        assert!(combine_confidence(&[2.0, -1.0]) <= 1.0);
    }

    #[test]
    fn tier_one_membership_matches_base_confidence() {
        // Any identifier with base confidence 1.0 must be tier one, and vice versa.
        // These two definitions are used independently and must not drift apart.
        for k in [
            IdentifierKind::Serial,
            IdentifierKind::ChassisId,
            IdentifierKind::SnmpEngineId,
            IdentifierKind::OtelHostId,
            IdentifierKind::Mac,
            IdentifierKind::MgmtIp,
            IdentifierKind::FlowExporter,
            IdentifierKind::Hostname,
            IdentifierKind::ServiceName,
        ] {
            assert_eq!(
                k.is_tier_one(),
                (k.base_confidence() - 1.0).abs() < f32::EPSILON,
                "{k:?} disagrees between is_tier_one() and base_confidence()"
            );
        }
    }
}
