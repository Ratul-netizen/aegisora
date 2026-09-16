//! `Enricher` over PostgreSQL — the `resource_id → (site, vendor)` half of attribution.
//!
//! A separate type rather than an impl on [`PgStore`], for the same reason [`PgCatalog`]
//! is: the pipeline knows nothing about PostgreSQL, and the store is what reaches across.
//!
//! [`PgCatalog`]: crate::PgCatalog

use uops_core::{ResourceId, SiteId, TenantId};
use uops_pipeline::{Enriched, Enricher};

use crate::store::PgStore;

/// Adapts the store to the trait the pipeline enriches through.
#[derive(Clone, Debug)]
pub struct PgEnricher {
    store: PgStore,
}

impl PgEnricher {
    #[must_use]
    pub const fn new(store: PgStore) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Enricher for PgEnricher {
    /// One row, two columns.
    ///
    /// Deliberately not `PgStore::resource`, which returns the whole record and would
    /// carry `attributes`, `tags` and a dozen other fields across a boundary that wants
    /// two of them. At the rate this is called — once per resource per process, behind
    /// the pipeline's cache — the difference is small, but the *shape* matters: a
    /// narrow query is one whose cost does not grow when somebody adds a column.
    ///
    /// A resource that is not there is `Ok(None)`, not an error. The resolver may have
    /// created it moments ago in another process, and treating that as a failure would
    /// cost the telemetry.
    async fn enrich(
        &self,
        tenant: TenantId,
        resource: ResourceId,
    ) -> Result<Option<Enriched>, String> {
        let row = sqlx::query!(
            r#"
            SELECT site_id AS "site_id: SiteId", vendor
              FROM resource
             WHERE tenant_id = $1 AND id = $2
            "#,
            tenant as TenantId,
            resource as ResourceId,
        )
        .fetch_optional(self.store.pool())
        .await
        .map_err(|e| e.to_string())?;

        Ok(row.map(|r| Enriched {
            // The nil uuid is what "no site" means on the telemetry side; the column
            // there is not nullable and one answer to "no site" beats two.
            site_id: r.site_id.unwrap_or_else(SiteId::nil),
            vendor: r.vendor.unwrap_or_default(),
        }))
    }
}
