//! The resource model — SPEC §M0.1.
//!
//! Everything telemetry attaches to is a resource: devices, interfaces, hosts, VMs,
//! containers, services, applications, databases. One model, one identity, so that
//! metrics, logs, traces, flows, events and config all hang off the same object.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::attr::AttrMap;
use crate::ids::{CredentialRef, ResourceGroupId, ResourceId, SiteId, TenantId};
use crate::tags::Tags;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
// Maps onto the PostgreSQL enum of the same name. If a variant is added here without
// a migration adding it there, the repository fails to compile against the schema —
// which is the intended outcome.
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(
    feature = "sqlx",
    sqlx(type_name = "resource_kind", rename_all = "snake_case")
)]
pub enum ResourceKind {
    Device,
    Interface,
    Host,
    Vm,
    Container,
    Service,
    Application,
    Database,
    CloudResource,
    Site,
}

impl ResourceKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Device => "device",
            Self::Interface => "interface",
            Self::Host => "host",
            Self::Vm => "vm",
            Self::Container => "container",
            Self::Service => "service",
            Self::Application => "application",
            Self::Database => "database",
            Self::CloudResource => "cloud_resource",
            Self::Site => "site",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(
    feature = "sqlx",
    sqlx(type_name = "resource_status", rename_all = "snake_case")
)]
pub enum ResourceStatus {
    Up,
    Down,
    Degraded,
    #[default]
    Unknown,
    /// Suppresses alerting without losing history.
    Maintenance,
    /// Retired. Kept so historical telemetry still resolves to something.
    Decommissioned,
}

impl ResourceStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Degraded => "degraded",
            Self::Unknown => "unknown",
            Self::Maintenance => "maintenance",
            Self::Decommissioned => "decommissioned",
        }
    }

    /// Whether alerts should be raised for a resource in this state.
    #[must_use]
    pub const fn alertable(self) -> bool {
        matches!(self, Self::Up | Self::Down | Self::Degraded | Self::Unknown)
    }
}

/// A monitored thing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Resource {
    pub id: ResourceId,
    pub tenant_id: TenantId,
    pub site_id: Option<SiteId>,
    /// Interface → device, container → host.
    pub parent_id: Option<ResourceId>,
    pub kind: ResourceKind,
    /// Canonical, system-chosen. Discovery may overwrite this.
    pub name: String,
    /// User override. Never written automatically — if a human named it, discovery
    /// must not silently rename it underneath them.
    pub display_name: Option<String>,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub status: ResourceStatus,
    pub profile_id: Option<uuid::Uuid>,
    /// Reference to sealed credential material — never the material itself.
    pub credential_ref: Option<CredentialRef>,
    /// OpenTelemetry semantic-convention keys. **Written by collectors**, on every walk.
    ///
    /// A human editing these would have their edit overwritten by the next discovery
    /// run, silently. What a human decides goes in [`tags`](Self::tags).
    pub attributes: AttrMap,
    /// Operator-managed labels: `environment=production`, `criticality=critical`.
    ///
    /// **Never written automatically** — the same rule as
    /// [`display_name`](Self::display_name), and for the same reason. A collector that
    /// wrote here would overwrite the judgement an alert routing rule depends on, and
    /// the resulting bug would be unreproducible because the evidence would be gone too.
    pub tags: Tags,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

impl Resource {
    /// What the UI should show: the human's name if they set one, else the system's.
    #[must_use]
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
}

/// An operator-defined set of resources.
///
/// The fourth way to talk about a group of things, and the only one nothing can infer:
///
/// | | what it means | who decides |
/// |---|---|---|
/// | site | where a thing physically is | discovery, or a human placing it |
/// | `parent_id` | what it is part of — interface → device | discovery |
/// | [`Relationship`] | how it is connected | discovery, LLDP, CDP |
/// | **group** | **which resources matter together** | **a human, and only a human** |
///
/// "Critical Servers" is not a place, a containment or a link. It is a sentence somebody
/// wrote down, and every M4 feature needs to be able to name one: an alert rule's scope,
/// a dashboard's filter, a notification routing rule, a maintenance window's target.
///
/// Membership is an explicit list rather than a stored predicate. A rule-based group —
/// "everything tagged `criticality=critical`" — is a later feature and materialises into
/// the same table, because an alert scoped to a rule that silently starts matching 400
/// more devices is a genuinely bad surprise, and an explicit list is what an operator can
/// audit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceGroup {
    pub id: ResourceGroupId,
    pub tenant_id: TenantId,
    /// Unique within the tenant, not globally: two customers of one MSP both have core
    /// routers.
    pub name: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// How two resources relate. Populated by discovery from M2 onward; the topology UI
/// arrives at M6, but the edges must exist before the correlation engine can use them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipKind {
    /// L2/L3 adjacency.
    ConnectedTo,
    /// Service → database.
    DependsOn,
    /// Hypervisor → VM, host → container.
    Hosts,
    /// Host → service.
    Runs,
    /// L3 next hop.
    RoutesTo,
    /// Interface → device, node → cluster.
    MemberOf,
}

impl RelationshipKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConnectedTo => "connected_to",
            Self::DependsOn => "depends_on",
            Self::Hosts => "hosts",
            Self::Runs => "runs",
            Self::RoutesTo => "routes_to",
            Self::MemberOf => "member_of",
        }
    }

    /// Whether a failure in the target propagates to the source.
    ///
    /// Used for blast-radius traversal. `ConnectedTo` is excluded: L2 adjacency is
    /// symmetric and does not imply dependence, so treating it as such would make
    /// every impact analysis spread across the entire network.
    #[must_use]
    pub const fn propagates_impact(self) -> bool {
        matches!(
            self,
            Self::DependsOn | Self::Hosts | Self::Runs | Self::MemberOf
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Relationship {
    pub tenant_id: TenantId,
    pub source_id: ResourceId,
    pub target_id: ResourceId,
    pub kind: RelationshipKind,
    pub confidence: f32,
    /// `lldp` | `snmp-iftable` | `otel` | `manual`.
    pub discovered_by: String,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_prefers_the_human_name() {
        let mut r = Resource {
            id: ResourceId::new(),
            tenant_id: TenantId::new(),
            site_id: None,
            parent_id: None,
            kind: ResourceKind::Device,
            name: "10.0.0.1".into(),
            display_name: None,
            vendor: None,
            model: None,
            os: None,
            os_version: None,
            status: ResourceStatus::Unknown,
            profile_id: None,
            credential_ref: None,
            attributes: AttrMap::new(),
            tags: Tags::new(),
            first_seen: Utc::now(),
            last_seen: Utc::now(),
        };
        assert_eq!(r.label(), "10.0.0.1");
        r.display_name = Some("Core Router 1".into());
        assert_eq!(r.label(), "Core Router 1");
    }

    #[test]
    fn maintenance_suppresses_alerting() {
        assert!(!ResourceStatus::Maintenance.alertable());
        assert!(!ResourceStatus::Decommissioned.alertable());
        assert!(ResourceStatus::Down.alertable());
        // Unknown must alert: a resource we cannot reach is the case that matters most.
        assert!(ResourceStatus::Unknown.alertable());
    }

    #[test]
    fn l2_adjacency_does_not_propagate_impact() {
        // If ConnectedTo propagated, blast radius would flood the whole switched
        // network from any single failure.
        assert!(!RelationshipKind::ConnectedTo.propagates_impact());
        assert!(RelationshipKind::DependsOn.propagates_impact());
        assert!(RelationshipKind::MemberOf.propagates_impact());
    }
}
