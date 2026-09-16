//! The resource repository.
//!
//! Every statement in this file filters on `tenant_id`, and every public method takes a
//! `&TenantScope` to get it. That is not a convention here — `enforced.rs` reads this
//! source file and fails the build if a query appears without it.
//!
//! Note what the scope is *not* used for: it is never interpolated. `TenantScope` is not
//! `Display` precisely so that it cannot be, and the tenant reaches the statement as a
//! bound parameter like every other value.

use sqlx::types::Json;
use uops_core::{
    AttrMap, CredentialRef, Error, Resource, ResourceId, ResourceKind, ResourceStatus, Result,
    SiteId, Tags, TenantScope,
};

use crate::error::map;
use crate::page::{Cursor, Page, page_size};
use crate::store::PgStore;

/// What a caller supplies to create a resource. Deliberately not `Resource`: `id`,
/// `tenant_id` and the timestamps are the database's to choose, and accepting them from
/// a caller is how a resource ends up in the wrong tenant.
#[derive(Clone, Debug)]
pub struct NewResource {
    pub kind: ResourceKind,
    pub name: String,
    pub display_name: Option<String>,
    pub site_id: Option<SiteId>,
    pub parent_id: Option<ResourceId>,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub attributes: AttrMap,
}

impl NewResource {
    #[must_use]
    pub fn new(kind: ResourceKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
            display_name: None,
            site_id: None,
            parent_id: None,
            vendor: None,
            model: None,
            os: None,
            os_version: None,
            attributes: AttrMap::new(),
        }
    }
}

/// Which resources to list.
#[derive(Clone, Debug, Default)]
pub struct ResourceFilter {
    pub kind: Option<ResourceKind>,
    pub site_id: Option<SiteId>,
    pub status: Option<ResourceStatus>,
    /// Case-insensitive substring of the name or display name.
    pub name_contains: Option<String>,
    pub cursor: Option<Cursor>,
    pub limit: Option<i64>,
}

/// One row of `resource`, exactly as the columns come back.
struct Row {
    id: ResourceId,
    tenant_id: uops_core::TenantId,
    site_id: Option<SiteId>,
    parent_id: Option<ResourceId>,
    kind: ResourceKind,
    name: String,
    display_name: Option<String>,
    vendor: Option<String>,
    model: Option<String>,
    os: Option<String>,
    os_version: Option<String>,
    status: ResourceStatus,
    profile_id: Option<uuid::Uuid>,
    credential_ref: Option<CredentialRef>,
    attributes: Json<AttrMap>,
    tags: Json<Tags>,
    first_seen: chrono::DateTime<chrono::Utc>,
    last_seen: chrono::DateTime<chrono::Utc>,
}

impl From<Row> for Resource {
    fn from(r: Row) -> Self {
        Self {
            id: r.id,
            tenant_id: r.tenant_id,
            site_id: r.site_id,
            parent_id: r.parent_id,
            kind: r.kind,
            name: r.name,
            display_name: r.display_name,
            vendor: r.vendor,
            model: r.model,
            os: r.os,
            os_version: r.os_version,
            status: r.status,
            profile_id: r.profile_id,
            credential_ref: r.credential_ref,
            attributes: r.attributes.0,
            tags: r.tags.0,
            first_seen: r.first_seen,
            last_seen: r.last_seen,
        }
    }
}

impl PgStore {
    /// Create a resource in the scope's tenant.
    pub async fn create_resource(
        &self,
        scope: &TenantScope,
        new: &NewResource,
    ) -> Result<Resource> {
        let id = ResourceId::new();
        let attributes = Json(new.attributes.clone());

        let row = sqlx::query_as!(
            Row,
            r#"
            INSERT INTO resource
                (id, tenant_id, site_id, parent_id, kind, name, display_name,
                 vendor, model, os, os_version, attributes)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            RETURNING
                id            AS "id: ResourceId",
                tenant_id     AS "tenant_id: uops_core::TenantId",
                site_id       AS "site_id: SiteId",
                parent_id     AS "parent_id: ResourceId",
                kind          AS "kind: ResourceKind",
                name,
                display_name,
                vendor, model, os, os_version,
                status        AS "status: ResourceStatus",
                profile_id,
                credential_ref AS "credential_ref: CredentialRef",
                attributes    AS "attributes: Json<AttrMap>",
                tags          AS "tags: Json<Tags>",
                first_seen, last_seen
            "#,
            id as ResourceId,
            scope.tenant_id() as uops_core::TenantId,
            new.site_id as Option<SiteId>,
            new.parent_id as Option<ResourceId>,
            new.kind as ResourceKind,
            new.name,
            new.display_name,
            new.vendor,
            new.model,
            new.os,
            new.os_version,
            attributes as Json<AttrMap>,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("resource", id.to_string(), e))?;

        Ok(row.into())
    }

    /// Point a resource at a stored credential, or detach it.
    ///
    /// # Why the credential's tenant is checked here
    ///
    /// The poller trusts `resource.credential_ref`: it reads the column and opens
    /// whatever it names. Without this check a resource could be pointed at another
    /// customer's credential by id, and the poller would dutifully use it — which is a
    /// cross-tenant read performed by the most privileged component in the product.
    ///
    /// Migration 0005's foreign key is on `credential (id)` alone, not on
    /// `(id, tenant_id)`, so the database would accept it. The `EXISTS` below is what
    /// refuses it, and it is one statement so there is no window between the check and
    /// the write.
    ///
    /// # Errors
    ///
    /// Storage failures, a resource in another tenant, or a credential in another
    /// tenant — both `NotFound`, because confirming either exists would leak it.
    pub async fn assign_credential(
        &self,
        scope: &TenantScope,
        id: ResourceId,
        credential: Option<CredentialRef>,
    ) -> Result<()> {
        let affected = sqlx::query!(
            r#"
            UPDATE resource
               SET credential_ref = $3, updated_at = now()
             WHERE tenant_id = $1
               AND id = $2
               AND ($3::uuid IS NULL
                    OR EXISTS (SELECT 1 FROM credential c
                                WHERE c.id = $3 AND c.tenant_id = $1))
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id as ResourceId,
            credential as Option<CredentialRef>,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("resource", id.to_string(), e))?
        .rows_affected();

        if affected == 0 {
            // Deliberately does not distinguish "no such resource" from "no such
            // credential". Both are things the caller may not know exist.
            return Err(Error::NotFound {
                kind: "resource",
                id: id.to_string(),
            });
        }
        Ok(())
    }

    /// Fetch one resource.
    ///
    /// A resource in another tenant reports `NotFound`, not `Forbidden` — the tenant
    /// predicate simply does not match it. Confirming that an ID exists somewhere else
    /// would leak one customer's inventory to another; `uops_core::Error` makes the two
    /// indistinguishable for the same reason.
    pub async fn resource(&self, scope: &TenantScope, id: ResourceId) -> Result<Resource> {
        let row = sqlx::query_as!(
            Row,
            r#"
            SELECT
                id            AS "id: ResourceId",
                tenant_id     AS "tenant_id: uops_core::TenantId",
                site_id       AS "site_id: SiteId",
                parent_id     AS "parent_id: ResourceId",
                kind          AS "kind: ResourceKind",
                name,
                display_name,
                vendor, model, os, os_version,
                status        AS "status: ResourceStatus",
                profile_id,
                credential_ref AS "credential_ref: CredentialRef",
                attributes    AS "attributes: Json<AttrMap>",
                tags          AS "tags: Json<Tags>",
                first_seen, last_seen
            FROM resource
            WHERE id = $1 AND tenant_id = $2
            "#,
            id as ResourceId,
            scope.tenant_id() as uops_core::TenantId,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("resource", id.to_string(), e))?;

        Ok(row.into())
    }

    /// List resources, filtered and keyset-paginated.
    ///
    /// The filters are written as `($n IS NULL OR column = $n)` rather than assembled
    /// from string fragments. That keeps one statement, which keeps the compile-time
    /// check — a builder producing SQL at runtime would verify nothing.
    pub async fn resources(
        &self,
        scope: &TenantScope,
        filter: &ResourceFilter,
    ) -> Result<Page<Resource>> {
        let limit = page_size(filter.limit);
        let after = filter.cursor.map(|c| c.0);
        let name = filter
            .name_contains
            .as_ref()
            .map(|q| format!("%{}%", q.replace('%', "\\%").replace('_', "\\_")));

        let rows = sqlx::query_as!(
            Row,
            r#"
            SELECT
                id            AS "id: ResourceId",
                tenant_id     AS "tenant_id: uops_core::TenantId",
                site_id       AS "site_id: SiteId",
                parent_id     AS "parent_id: ResourceId",
                kind          AS "kind: ResourceKind",
                name,
                display_name,
                vendor, model, os, os_version,
                status        AS "status: ResourceStatus",
                profile_id,
                credential_ref AS "credential_ref: CredentialRef",
                attributes    AS "attributes: Json<AttrMap>",
                tags          AS "tags: Json<Tags>",
                first_seen, last_seen
            FROM resource
            WHERE tenant_id = $1
              AND ($2::uuid IS NULL OR id > $2)
              AND ($3::resource_kind IS NULL OR kind = $3)
              AND ($4::uuid IS NULL OR site_id = $4)
              AND ($5::resource_status IS NULL OR status = $5)
              AND ($6::text IS NULL
                   OR name ILIKE $6
                   OR display_name ILIKE $6)
            ORDER BY id
            LIMIT $7
            "#,
            scope.tenant_id() as uops_core::TenantId,
            after as Option<ResourceId>,
            filter.kind as Option<ResourceKind>,
            filter.site_id as Option<SiteId>,
            filter.status as Option<ResourceStatus>,
            name,
            // One more than asked for, to learn whether a next page exists without a
            // second COUNT over the filtered set.
            limit + 1,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("resource", String::new(), e))?;

        let resources: Vec<Resource> = rows.into_iter().map(Resource::from).collect();
        Ok(Page::from_overfetch(resources, limit, |r| Cursor(r.id)))
    }

    /// Change a resource's status.
    ///
    /// `DELETE /resources/{id}` is this, with `Decommissioned` — SPEC §M1. A hard delete
    /// would orphan every piece of telemetry already written under that `resource_id`,
    /// and historical data that resolves to nothing is worse than a retired row.
    pub async fn set_resource_status(
        &self,
        scope: &TenantScope,
        id: ResourceId,
        status: ResourceStatus,
    ) -> Result<Resource> {
        let affected = sqlx::query!(
            r#"
            UPDATE resource
               SET status = $3
             WHERE id = $1 AND tenant_id = $2
            "#,
            id as ResourceId,
            scope.tenant_id() as uops_core::TenantId,
            status as ResourceStatus,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("resource", id.to_string(), e))?
        .rows_affected();

        if affected == 0 {
            return Err(Error::NotFound {
                kind: "resource",
                id: id.to_string(),
            });
        }
        self.resource(scope, id).await
    }

    /// Record that a resource was seen. Called by the pipeline on every resolved
    /// envelope, so it is deliberately a single narrow write rather than a read-modify.
    pub async fn touch_resource(&self, scope: &TenantScope, id: ResourceId) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE resource
               SET last_seen = now()
             WHERE id = $1 AND tenant_id = $2
            "#,
            id as ResourceId,
            scope.tenant_id() as uops_core::TenantId,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("resource", id.to_string(), e))?;
        Ok(())
    }
}
