//! Which resources the poller can actually reach.
//!
//! A `resource` row is not a device. It becomes one when it has somewhere to send a
//! packet and something to authenticate with, and the join that establishes both is the
//! whole of this module.
//!
//! # Why the address is an identifier and not a column
//!
//! `resource` has no `address` column, deliberately. An address is an *identifier* —
//! SPEC §M0.2's tier 3, confidence 0.80 — and putting it in a column would make it two
//! things at once: the thing identity resolution matches on, and the thing the poller
//! dials. They drift the moment DHCP moves a device, and then one of them is wrong with
//! nothing to say which.
//!
//! So the poller reads `resource_identifier` where `kind = 'mgmt_ip'`, which is the same
//! row identity resolution wrote. A device that is re-addressed is re-identified and
//! re-dialled by the same fact changing once.
//!
//! # Why `sysObjectID` is an attribute
//!
//! Profile resolution matches on it, and getting it requires polling the device — so a
//! poller that read it fresh every cycle would spend a round trip per device per cycle
//! rediscovering something that changes when the hardware is replaced. It is cached in
//! `resource.attributes` under `snmp.sysobjectid` by the poll that discovers it, and
//! read from there afterwards.

use uops_core::{CredentialRef, ResourceId, Result, SiteId, TenantId, TenantScope};

use crate::error::map;
use crate::store::PgStore;

/// A resource the poller can reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollableDevice {
    pub tenant_id: TenantId,
    pub resource_id: ResourceId,
    /// `None` for a device not assigned to a site. Telemetry still needs *a* site id —
    /// see `uops_store_ch::MetricRow` — and the caller substitutes the nil uuid, which
    /// is what "no site" means there.
    pub site_id: Option<SiteId>,
    /// From the `mgmt_ip` identifier. Text, because that is what the column holds and
    /// because an unparseable one is a data problem the caller should report rather
    /// than a row this query should silently drop.
    pub address: String,
    pub credential: Option<CredentialRef>,
    /// An explicit profile pin, which beats `sysObjectID` matching.
    pub profile_id: Option<uuid::Uuid>,
    /// The pinned profile's key, resolved in the same query.
    ///
    /// `resource.profile_id` is a `monitoring_profile` row and profile *resolution*
    /// matches on the key, so without this the poller would have to fetch every pinned
    /// row separately to turn an id it already has into a name it can use. `None` when
    /// nothing is pinned — or when the pinned row is disabled, which is how an operator
    /// turns a profile off for a device that still points at it.
    pub profile_key: Option<String>,
    /// Cached from a previous poll. `None` means this device has not been asked yet and
    /// will fall back to `generic-snmp` until it has.
    pub sysobjectid: Option<String>,
}

/// The attribute key the discovered `sysObjectID` is cached under.
pub const SYSOBJECTID_KEY: &str = "snmp.sysobjectid";

impl PgStore {
    /// Every device in the scope's tenant that has an address.
    ///
    /// Decommissioned resources are excluded: SPEC §M1 makes decommissioning a soft
    /// delete so history still resolves, and continuing to poll something an operator
    /// retired would produce telemetry nobody asked for and alerts nobody wants.
    ///
    /// # Errors
    ///
    /// Storage failures.
    pub async fn pollable_devices(
        &self,
        scope: &TenantScope,
        limit: i64,
    ) -> Result<Vec<PollableDevice>> {
        let rows = sqlx::query!(
            r#"
            SELECT r.id            AS "resource_id: ResourceId",
                   r.tenant_id     AS "tenant_id: TenantId",
                   r.site_id       AS "site_id: SiteId",
                   i.value         AS address,
                   r.credential_ref AS "credential: CredentialRef",
                   r.profile_id,
                   -- `?`: the LEFT JOIN makes it nullable and sqlx reads nullability
                   -- from the column, which is NOT NULL in its own table.
                   p.profile_key AS "profile_key?",
                   r.attributes ->> $3 AS sysobjectid
              FROM resource r
              JOIN resource_identifier i
                ON i.resource_id = r.id
               AND i.tenant_id = r.tenant_id
               AND i.kind = 'mgmt_ip'
              -- LEFT, and filtered on `enabled`: a pin at a disabled profile leaves the
              -- device pollable under whatever sysObjectID matching chooses, rather than
              -- dropping it out of the fleet entirely.
              LEFT JOIN monitoring_profile p
                ON p.id = r.profile_id
               AND p.enabled
             WHERE r.tenant_id = $1
               AND r.status <> 'decommissioned'
             ORDER BY r.id
             LIMIT $2
            "#,
            scope.tenant_id() as TenantId,
            limit,
            SYSOBJECTID_KEY,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("resource", String::new(), e))?;

        Ok(rows
            .into_iter()
            .map(|r| PollableDevice {
                tenant_id: r.tenant_id,
                resource_id: r.resource_id,
                site_id: r.site_id,
                address: r.address,
                credential: r.credential,
                profile_id: r.profile_id,
                profile_key: r.profile_key,
                sysobjectid: r.sysobjectid,
            })
            .collect())
    }

    /// Every tenant, for a poller that serves all of them.
    ///
    /// `TenantScope` has no "all tenants" constructor, deliberately — a cross-tenant
    /// *query* should never be expressible. What a single-process poller needs instead
    /// is the list, so that it can build a scope per tenant and do the crossing in a
    /// visible loop rather than in a `WHERE` clause. This returns identifiers and
    /// nothing else: no tenant's data crosses here, only the fact that it exists.
    ///
    /// # Errors
    ///
    /// Storage failures.
    pub async fn all_tenant_ids(&self) -> Result<Vec<TenantId>> {
        // tenant-exempt: the list of tenants cannot itself be filtered by tenant. The
        // marker is explicit rather than implied by the `id: TenantId` cast below,
        // which happens to contain the string the scanner looks for and would have let
        // this pass for the wrong reason.
        let rows = sqlx::query_scalar!(
            r#"SELECT id AS "id: TenantId" FROM tenant ORDER BY created_at, id"#
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("tenant", String::new(), e))?;

        Ok(rows)
    }

    /// Record the `sysObjectID` a poll discovered.
    ///
    /// Merged into `attributes` rather than replacing them: the map also holds semconv
    /// keys written by identity resolution and by collectors, and a poller that replaced
    /// the object would delete them.
    ///
    /// # Errors
    ///
    /// Storage failures.
    pub async fn record_sysobjectid(
        &self,
        scope: &TenantScope,
        resource: ResourceId,
        sysobjectid: &str,
    ) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE resource
               SET attributes = attributes || jsonb_build_object($3::text, $4::text),
                   updated_at = now()
             WHERE tenant_id = $1 AND id = $2
            "#,
            scope.tenant_id() as TenantId,
            resource as ResourceId,
            SYSOBJECTID_KEY,
            sysobjectid,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("resource", resource.to_string(), e))?;
        Ok(())
    }
}
