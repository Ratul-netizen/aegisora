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
                   r.attributes ->> $3 AS sysobjectid
              FROM resource r
              JOIN resource_identifier i
                ON i.resource_id = r.id
               AND i.tenant_id = r.tenant_id
               AND i.kind = 'mgmt_ip'
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
                sysobjectid: r.sysobjectid,
            })
            .collect())
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
