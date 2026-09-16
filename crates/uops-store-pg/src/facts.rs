//! What a device says it is.
//!
//! Make, model, serial and software version, read off a device by the poller and written
//! here. SPEC §M2 calls these part of the inventory; what makes them more than decoration
//! is the serial.
//!
//! # The serial is a tier-1 identifier
//!
//! SPEC §M0.2 ranks identifiers by how much a match on one proves. A serial number is
//! tier 1 — globally unique by specification, confidence 1.00, and on its own proof that
//! two observations are the same device. A management address is tier 3 at 0.80 and a
//! hostname is 0.65.
//!
//! Until this module existed nothing in the product produced a tier-1 identifier for an
//! SNMP device, so identity resolution had been running entirely on the weak tiers: two
//! collectors seeing the same switch resolved to one resource only if they agreed about
//! its address or its name, and a device that was re-addressed looked like a new one.
//!
//! It is also what makes a *contradiction* detectable. Two tier-1 identifiers disagreeing
//! is not low confidence — it is proof of a different resource, which is how the resolver
//! tells "this switch moved" from "somebody put a new switch at that address".
//!
//! # Why a vendor is never overwritten with nothing
//!
//! Every field is optional and a device that does not answer one is the normal case, not
//! an error. An update that wrote `NULL` for an unanswered field would erase what an
//! earlier poll learned, or what an operator typed, every fifteen minutes — so the
//! statement below only sets a column when there is something to set.

use uops_core::{Error, Identifier, IdentifierKind, ResourceId, Result, TenantScope};

use crate::error::map;
use crate::store::PgStore;

/// What a poll learned about a device.
///
/// Every field optional, because every OID behind it is. A device that implements no
/// `ENTITY-MIB` produces one of these with only `os` set, from `sysDescr`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceFacts {
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub os: Option<String>,
    pub os_version: Option<String>,
}

impl DeviceFacts {
    /// Whether there is anything here worth a round trip.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.vendor.is_none()
            && self.model.is_none()
            && self.serial.is_none()
            && self.os.is_none()
            && self.os_version.is_none()
    }
}

/// What recording them did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdentityReport {
    /// Whether the resource row changed.
    pub updated: bool,
    /// Whether a serial was attached as an identifier.
    pub serial_recorded: bool,
}

/// The source recorded on an identifier this poller writes.
pub const SOURCE: &str = "snmp-identity";

/// The source recorded on an identifier a person typed.
///
/// The column is what separates what an operator asserted from what a collector
/// observed, and the separation is load-bearing: editing the inventory must not be able
/// to delete the evidence identity resolution merged two resources on.
pub const MANUAL: &str = "manual";

impl PgStore {
    /// Record what a device says it is.
    ///
    /// # Errors
    ///
    /// Storage failures. A device that is not in this tenant simply matches no row and
    /// reports `updated: false`.
    pub async fn record_device_facts(
        &self,
        scope: &TenantScope,
        resource: ResourceId,
        facts: &DeviceFacts,
    ) -> Result<IdentityReport> {
        let mut report = IdentityReport::default();
        if facts.is_empty() {
            return Ok(report);
        }
        let tenant = scope.tenant_id();

        // COALESCE on the *parameter*, not on the column: a NULL argument leaves the
        // column as it was. See the module docs — a device that stops answering one OID
        // must not erase what it said last time.
        let affected = sqlx::query(
            "UPDATE resource
                SET vendor     = COALESCE($3, vendor),
                    model      = COALESCE($4, model),
                    os         = COALESCE($5, os),
                    os_version = COALESCE($6, os_version),
                    updated_at = now()
              WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant.into_uuid())
        .bind(resource.into_uuid())
        .bind(facts.vendor.as_deref())
        .bind(facts.model.as_deref())
        .bind(facts.os.as_deref())
        .bind(facts.os_version.as_deref())
        .execute(self.pool())
        .await
        .map_err(|e| map("resource", resource.to_string(), e))?
        .rows_affected();
        report.updated = affected > 0;

        if let Some(serial) = facts.serial.as_deref().filter(|s| !s.is_empty()) {
            // The same upsert identity resolution uses: an identifier stays with whoever
            // already holds it, and the row records that it was seen again. Taking a
            // serial from another resource is a tier-1 contradiction and a decision the
            // resolver makes — not something a poller does on its own.
            let written = sqlx::query(
                "INSERT INTO resource_identifier
                     (id, tenant_id, resource_id, kind, value, confidence, source)
                 VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, $6)
                 ON CONFLICT (tenant_id, kind, value) DO UPDATE SET last_seen = now()",
            )
            .bind(tenant.into_uuid())
            .bind(resource.into_uuid())
            .bind(IdentifierKind::Serial)
            .bind(serial)
            .bind(IdentifierKind::Serial.base_confidence())
            .bind(SOURCE)
            .execute(self.pool())
            .await
            .map_err(|e| map("identifier", resource.to_string(), e))?
            .rows_affected();
            report.serial_recorded = written > 0;
        }

        Ok(report)
    }

    /// Replace the identifiers an operator typed, leaving the ones collectors found.
    ///
    /// # Why replace rather than add
    ///
    /// One verb gives add, change and remove. A `POST` that only added would make a
    /// typo'd management address permanent — and a mistyped `mgmt_ip` is not a cosmetic
    /// error, it is a device polling somebody else's equipment.
    ///
    /// # Why only the manual ones
    ///
    /// A discovered identifier is a fact a collector observed: the MAC discovery read
    /// off an interface, the serial ENTITY-MIB reported. An operator editing the
    /// inventory has no business deleting those, and an operator who could would be
    /// able to make identity resolution forget why it merged two resources. The
    /// `source` column is what separates them, and this statement is the only place
    /// that distinction is enforced.
    ///
    /// # Errors
    ///
    /// Storage failures, or a value another resource in this tenant already claims —
    /// which is `IdentityConflict`, because two devices cannot share a management
    /// address and the caller needs to know which one already has it.
    pub async fn set_manual_identifiers(
        &self,
        scope: &TenantScope,
        resource: ResourceId,
        identifiers: &[Identifier],
    ) -> Result<()> {
        let tenant = scope.tenant_id();

        // One transaction: between the delete and the insert this resource has no
        // address, and a poller reloading in that window would drop it from the fleet.
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| map("identifier", resource.to_string(), e))?;

        // The resource must be this tenant's. Without this the delete below matches
        // nothing and the insert then attaches an identifier to another customer's
        // resource — which is the one mistake in this module that would matter.
        let exists: Option<(uuid::Uuid,)> =
            sqlx::query_as("SELECT id FROM resource WHERE tenant_id = $1 AND id = $2")
                .bind(tenant.into_uuid())
                .bind(resource.into_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| map("resource", resource.to_string(), e))?;
        if exists.is_none() {
            return Err(Error::NotFound {
                kind: "resource",
                id: resource.to_string(),
            });
        }

        sqlx::query(
            "DELETE FROM resource_identifier
              WHERE tenant_id = $1 AND resource_id = $2 AND source = $3",
        )
        .bind(tenant.into_uuid())
        .bind(resource.into_uuid())
        .bind(MANUAL)
        .execute(&mut *tx)
        .await
        .map_err(|e| map("identifier", resource.to_string(), e))?;

        for identifier in identifiers {
            let result = sqlx::query(
                "INSERT INTO resource_identifier
                     (id, tenant_id, resource_id, kind, value, confidence, source)
                 VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, $6)",
            )
            .bind(tenant.into_uuid())
            .bind(resource.into_uuid())
            .bind(identifier.kind)
            .bind(&identifier.value)
            .bind(identifier.kind.base_confidence())
            .bind(MANUAL)
            .execute(&mut *tx)
            .await;

            if let Err(e) = result {
                // Not an upsert, unlike the collectors' path. A collector re-observing
                // an identifier somebody else holds is ordinary and the row simply stays
                // where it is; an operator *typing* one is asserting something, and
                // silently ignoring it would leave them looking at a form that saved and
                // a device that did not change.
                if let sqlx::Error::Database(db) = &e
                    && db.is_unique_violation()
                {
                    return Err(Error::IdentityConflict {
                        kind: identifier.kind.as_str(),
                        value: identifier.value.clone(),
                    });
                }
                return Err(map("identifier", resource.to_string(), e));
            }
        }

        tx.commit()
            .await
            .map_err(|e| map("identifier", resource.to_string(), e))?;
        Ok(())
    }

    /// The identifiers recorded for a resource. For a test, and for a detail view.
    ///
    /// # Errors
    ///
    /// Storage failures.
    pub async fn identifiers_for(
        &self,
        scope: &TenantScope,
        resource: ResourceId,
    ) -> Result<Vec<Identifier>> {
        // The resource has to exist in this tenant. Without this, another tenant's id
        // returns an empty list and a 200 — which leaks nothing, since an empty list is
        // also what a resource with no identifiers returns, but which says "this exists
        // and has none" where every other resource route says "not found". An API whose
        // answer to the same question depends on which route you asked is one nobody can
        // reason about.
        let exists: Option<(uuid::Uuid,)> =
            sqlx::query_as("SELECT id FROM resource WHERE tenant_id = $1 AND id = $2")
                .bind(scope.tenant_id().into_uuid())
                .bind(resource.into_uuid())
                .fetch_optional(self.pool())
                .await
                .map_err(|e| map("resource", resource.to_string(), e))?;
        if exists.is_none() {
            return Err(Error::NotFound {
                kind: "resource",
                id: resource.to_string(),
            });
        }

        let rows: Vec<(IdentifierKind, String)> = sqlx::query_as(
            "SELECT kind, value FROM resource_identifier
              WHERE tenant_id = $1 AND resource_id = $2
              ORDER BY kind, value",
        )
        .bind(scope.tenant_id().into_uuid())
        .bind(resource.into_uuid())
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("identifier", resource.to_string(), e))?;

        Ok(rows
            .into_iter()
            .map(|(kind, value)| Identifier { kind, value })
            .collect())
    }
}
