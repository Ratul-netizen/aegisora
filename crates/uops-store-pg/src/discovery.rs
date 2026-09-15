//! What a discovery walk becomes in the database.
//!
//! SPEC §M2: *"Interface discovery creates child resources **and** `member_of`
//! relationships."* The **and** is the interesting word, and it is why this is one
//! transaction: a child with no edge is a resource nothing can reach from the device it
//! belongs to, and an edge with no child is a dangling reference. Both are states the
//! topology UI (M6) and the correlation engine (M9) would read as fact.
//!
//! # Why this does not go through identity resolution
//!
//! An interface is not a resource that "turned up sending telemetry" — the case
//! `IdentityStore::create_provisional` exists for. It is a row in a table on a device
//! this poller already knows the identity of, reached through a credential that already
//! belongs to that device's tenant. Its parent is not in question, so there is nothing
//! to resolve: running it through the resolver would mean manufacturing a confidence
//! score for a fact, and putting an interface in the review queue because two switches
//! both have a `GigabitEthernet0/1`.
//!
//! Identifiers *are* attached, because the resolver needs them later — an interface's
//! MAC is how a flow record or an LLDP neighbour finds its way to this row.
//!
//! # What happens to an interface that disappears
//!
//! Nothing, deliberately. A port that stops appearing in `ifTable` has been removed, or
//! the walk was cut short, or the agent restarted mid-poll — and only the first of those
//! means the interface is gone. Telemetry already written references the child by id, so
//! deleting it would orphan history, and SPEC makes decommissioning a soft delete for
//! exactly that reason. `last_seen` stops advancing, which is the signal; acting on that
//! signal is a product decision and is in STATUS rather than invented here.

use uops_core::{Identifier, ResourceId, ResourceKind, Result, TenantScope};

use crate::error::map;
use crate::store::PgStore;

/// One row of a discovery walk, ready to be written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredChild {
    /// What the device calls it — `ifName`, for an interface. The key this is matched
    /// on across runs; see migration 0008 on why not `ifIndex`.
    pub name: String,
    pub kind: ResourceKind,
    /// The row's index in the table it came from, e.g. `ifIndex`. Stored as an
    /// attribute rather than used as a key, because it is what joins a sample's
    /// `network.interface.index` label back to this row and it is *not* stable enough
    /// to identify one.
    pub index: String,
    pub identifiers: Vec<Identifier>,
}

/// What one discovery pass wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiscoveryReport {
    /// Children that did not exist before.
    pub created: usize,
    /// Children that were already there and had their `last_seen` advanced.
    pub seen: usize,
    /// `member_of` edges written or refreshed.
    pub edges: usize,
    /// Identifiers attached. Fewer than offered means some were already held by another
    /// resource — see `attach_identifiers`, which leaves an identifier with whoever owns
    /// it. Counted rather than asserted, because a device that gives every port the same
    /// chassis MAC is a real device and not an error.
    pub identifiers: usize,
}

/// The `discovered_by` value these edges carry.
///
/// Free text in the schema on purpose — a new discovery source should not need a
/// migration — and this is the one this module writes. M5's LLDP discovery will write
/// `lldp` beside it, and the two disagreeing about a topology is information rather than
/// a conflict.
pub const SOURCE: &str = "snmp-iftable";

/// The attribute key an interface's table index is stored under.
///
/// The same string `uops_poll::sample::interface_columns` labels a sample with, so a row
/// of telemetry and the resource it belongs to can be joined on it.
pub const INDEX_KEY: &str = "network.interface.index";

impl PgStore {
    /// Record what a discovery walk found under `parent`.
    ///
    /// One transaction: see the module docs on why a child without its edge is worse
    /// than no child.
    ///
    /// # Errors
    ///
    /// Storage failures, or a parent that is not in this tenant.
    pub async fn record_discovery(
        &self,
        scope: &TenantScope,
        parent: ResourceId,
        children: &[DiscoveredChild],
    ) -> Result<DiscoveryReport> {
        let mut report = DiscoveryReport::default();
        if children.is_empty() {
            return Ok(report);
        }
        let tenant = scope.tenant_id();

        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| map("resource", parent.to_string(), e))?;

        // The child inherits the parent's site. Not nullable-by-default: an interface is
        // in the same rack as the device it is part of, and a child with no site would
        // disappear from every site-scoped view its parent appears in.
        //
        // Also the tenant check: a parent in another tenant returns no row, and the
        // insert below would then violate the composite foreign key. Failing here says
        // which resource, rather than reporting a constraint name.
        let site: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT site_id FROM resource WHERE tenant_id = $1 AND id = $2")
                .bind(tenant.into_uuid())
                .bind(parent.into_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| map("resource", parent.to_string(), e))?
                .ok_or_else(|| uops_core::Error::NotFound {
                    kind: "resource",
                    id: parent.to_string(),
                })?;

        for child in children {
            // ON CONFLICT against the partial index from migration 0008, so a rediscovery
            // is an update rather than a new interface every fifteen minutes. `xmax = 0`
            // is PostgreSQL's way of saying the row was inserted rather than updated —
            // there is no other way to tell from an upsert, and the alternative is a
            // second query per interface.
            let (id, inserted): (uuid::Uuid, bool) = sqlx::query_as(
                "INSERT INTO resource
                     (id, tenant_id, site_id, parent_id, kind, name, attributes)
                 VALUES (gen_random_uuid(), $1, $2, $3, $4, $5,
                         jsonb_build_object($6::text, $7::text))
                 ON CONFLICT (tenant_id, parent_id, kind, name) WHERE parent_id IS NOT NULL
                 DO UPDATE SET
                     last_seen  = now(),
                     updated_at = now(),
                     -- Merged, not replaced: the map also holds semconv keys written by
                     -- identity resolution and by collectors, and an interface whose
                     -- index changed after a reboot should have the new one.
                     attributes = resource.attributes
                                  || jsonb_build_object($6::text, $7::text)
                 RETURNING id, (xmax = 0) AS inserted",
            )
            .bind(tenant.into_uuid())
            .bind(site)
            .bind(parent.into_uuid())
            .bind(child.kind)
            .bind(&child.name)
            .bind(INDEX_KEY)
            .bind(&child.index)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| map("resource", child.name.clone(), e))?;

            if inserted {
                report.created += 1;
            } else {
                report.seen += 1;
            }

            // The edge, from child to parent. Direction matters and is not arbitrary:
            // `member_of` reads source-to-target, and it is the interface that is a
            // member of the device rather than the other way round.
            sqlx::query(
                "INSERT INTO resource_relationship
                     (id, tenant_id, source_id, target_id, kind, confidence, discovered_by)
                 VALUES (gen_random_uuid(), $1, $2, $3, 'member_of', 1.0, $4)
                 ON CONFLICT (tenant_id, source_id, target_id, kind)
                 DO UPDATE SET last_seen = now()",
            )
            .bind(tenant.into_uuid())
            .bind(id)
            .bind(parent.into_uuid())
            .bind(SOURCE)
            .execute(&mut *tx)
            .await
            .map_err(|e| map("relationship", child.name.clone(), e))?;
            report.edges += 1;

            // Confidence 1.0 above, unlike a discovered edge in M5: this is not an
            // inference from traffic or a neighbour's claim about us. The device itself
            // listed the interface as one of its own, over an authenticated session.

            for identifier in &child.identifiers {
                let affected = sqlx::query(
                    "INSERT INTO resource_identifier
                         (id, tenant_id, resource_id, kind, value, confidence, source)
                     VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, $6)
                     ON CONFLICT (tenant_id, kind, value) DO UPDATE SET last_seen = now()",
                )
                .bind(tenant.into_uuid())
                .bind(id)
                .bind(identifier.kind)
                .bind(&identifier.value)
                .bind(identifier.kind.base_confidence())
                .bind(SOURCE)
                .execute(&mut *tx)
                .await
                .map_err(|e| map("identifier", child.name.clone(), e))?
                .rows_affected();
                report.identifiers += usize::try_from(affected).unwrap_or(0);
            }
        }

        tx.commit()
            .await
            .map_err(|e| map("resource", parent.to_string(), e))?;

        Ok(report)
    }

    /// The children of a resource, for a test or a topology view.
    ///
    /// # Errors
    ///
    /// Storage failures.
    pub async fn children_of(
        &self,
        scope: &TenantScope,
        parent: ResourceId,
    ) -> Result<Vec<(ResourceId, String)>> {
        let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(
            "SELECT id, name FROM resource
              WHERE tenant_id = $1 AND parent_id = $2
              ORDER BY name",
        )
        .bind(scope.tenant_id().into_uuid())
        .bind(parent.into_uuid())
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("resource", parent.to_string(), e))?;

        Ok(rows
            .into_iter()
            .map(|(id, name)| (ResourceId::from_uuid(id), name))
            .collect())
    }

    /// The `member_of` edges pointing at a resource.
    ///
    /// # Errors
    ///
    /// Storage failures.
    pub async fn members_of(
        &self,
        scope: &TenantScope,
        parent: ResourceId,
    ) -> Result<Vec<ResourceId>> {
        let rows: Vec<(uuid::Uuid,)> = sqlx::query_as(
            "SELECT source_id FROM resource_relationship
              WHERE tenant_id = $1 AND target_id = $2 AND kind = 'member_of'
              ORDER BY source_id",
        )
        .bind(scope.tenant_id().into_uuid())
        .bind(parent.into_uuid())
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("relationship", parent.to_string(), e))?;

        Ok(rows
            .into_iter()
            .map(|(id,)| ResourceId::from_uuid(id))
            .collect())
    }
}
