//! `ResourceCatalog` over PostgreSQL — what makes `uops-query`'s resolution real.
//!
//! `uops-query` has compiled `ResourceSelector` into a resolved ID list since M0, but
//! against a fake catalog in tests and nothing at all in production. This is the other
//! half: the four control-plane lookups that turn "every device at the Dhaka site" into
//! the `resource_id IN (…)` predicate that reaches `ClickHouse`.
//!
//! It is also where alias expansion actually happens. A merge writes `resource_alias`
//! and never rewrites the telemetry already stored under the old `resource_id`, so a
//! query for the surviving resource has to ask for both IDs — and because
//! `0004_identity.sql` collapses alias chains on write, that stays a single join
//! instead of a recursive walk on the hot path.

use async_trait::async_trait;
use uops_core::{ResourceId, ResourceKind, SiteId, TenantId};
use uops_query::{ResourceCatalog, Result as QueryResult};

use crate::store::PgStore;

/// Adapts the store to the trait `uops-query` resolves through.
///
/// A separate type rather than an impl on `PgStore` so that the dependency direction is
/// visible: the query layer knows nothing about PostgreSQL, and the store is what
/// reaches across.
#[derive(Clone, Debug)]
pub struct PgCatalog {
    store: PgStore,
}

impl PgCatalog {
    #[must_use]
    pub const fn new(store: PgStore) -> Self {
        Self { store }
    }
}

/// Failures here are storage failures, not query-compilation failures, but the trait
/// speaks `uops_query::Error`. `Transport` is the honest variant: the planner did
/// nothing wrong.
fn storage(e: &sqlx::Error) -> uops_query::Error {
    uops_query::Error::Invalid(format!("resolving resources: {e}"))
}

#[async_trait]
impl ResourceCatalog for PgCatalog {
    /// Collapse aliases and drop anything that is not this tenant's.
    ///
    /// The `JOIN resource` at the end is the tenant check *and* the existence check in
    /// one: an ID belonging to another tenant simply does not join, so it disappears
    /// rather than producing a distinguishable error. SPEC §M0.2 and
    /// `uops_core::Error::TenantMismatch` take the same position — confirming that an
    /// ID exists elsewhere is an inventory leak between customers.
    async fn canonical(
        &self,
        tenant: TenantId,
        ids: &[ResourceId],
    ) -> QueryResult<Vec<ResourceId>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let raw: Vec<uuid::Uuid> = ids.iter().map(|id| id.into_uuid()).collect();

        let rows = sqlx::query_scalar!(
            r#"
            SELECT r.id AS "id!: ResourceId"
              FROM unnest($2::uuid[]) AS given(id)
              LEFT JOIN resource_alias a
                     ON a.tenant_id = $1 AND a.historical_id = given.id
              JOIN resource r
                     ON r.tenant_id = $1
                    AND r.id = COALESCE(a.current_id, given.id)
            "#,
            tenant as TenantId,
            &raw,
        )
        .fetch_all(self.store.pool())
        .await
        .map_err(|e| storage(&e))?;

        Ok(rows)
    }

    async fn of_kind(&self, tenant: TenantId, kind: ResourceKind) -> QueryResult<Vec<ResourceId>> {
        sqlx::query_scalar!(
            r#"
            SELECT id AS "id!: ResourceId"
              FROM resource
             WHERE tenant_id = $1 AND kind = $2
            "#,
            tenant as TenantId,
            kind as ResourceKind,
        )
        .fetch_all(self.store.pool())
        .await
        .map_err(|e| storage(&e))
    }

    async fn at_site(&self, tenant: TenantId, site: SiteId) -> QueryResult<Vec<ResourceId>> {
        sqlx::query_scalar!(
            r#"
            SELECT id AS "id!: ResourceId"
              FROM resource
             WHERE tenant_id = $1 AND site_id = $2
            "#,
            tenant as TenantId,
            site as SiteId,
        )
        .fetch_all(self.store.pool())
        .await
        .map_err(|e| storage(&e))
    }

    /// Walks `resource_dependents()`, which carries the cycle guard and the depth bound.
    ///
    /// The tenant is the function's first argument, not a predicate bolted on afterwards
    /// — see the note in `migrations/0003_relationships.sql`. `DISTINCT` because a node
    /// reachable by two paths appears once per path, and a caller wants the set.
    async fn descendants(
        &self,
        tenant: TenantId,
        root: ResourceId,
        max_depth: u8,
    ) -> QueryResult<Vec<ResourceId>> {
        // tenant-exempt: the tenant IS $1 — resource_dependents() takes it as its
        // first argument and filters every step of the walk on it, which is exactly
        // why that function's signature differs from the SPEC sketch.
        sqlx::query_scalar!(
            r#"
            SELECT DISTINCT resource_id AS "id!: ResourceId"
              FROM resource_dependents($1, $2, $3)
            "#,
            tenant as TenantId,
            root as ResourceId,
            i32::from(max_depth),
        )
        .fetch_all(self.store.pool())
        .await
        .map_err(|e| storage(&e))
    }
}
