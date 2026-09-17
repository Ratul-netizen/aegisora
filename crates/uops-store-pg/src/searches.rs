//! Saved searches — SPEC §M3.
//!
//! A saved search is a stored [`Query`] AST and nothing else. That is the whole design,
//! and it is what makes M4's acceptance criterion — *"a saved search from the Log
//! Explorer converts to an alert rule with no edits"* — true by construction rather than
//! by a conversion function somebody has to keep in step with the AST.
//!
//! # Two things this module refuses to store
//!
//! **A query the compiler cannot answer.** [`PgStore::save_search`] compiles the AST
//! before it writes the row. A search over `trace`, or a `severity` filter on metrics,
//! or a time bucket wider than a day, is refused here — at the moment somebody clicks
//! Save, with the compiler's own wording — rather than stored and discovered to be
//! broken by whoever opens it during an incident. The check is on the query's *shape*:
//! it validates fields against the signal and the limits against their ceilings, not
//! whether the resources it names still exist, which is a question with a different
//! answer every week and no business failing a save.
//!
//! **A signal that disagrees with the AST.** The `signal` column is denormalised so that
//! listing a tenant's searches does not parse a jsonb document per row, and migration
//! 0013 has a CHECK that keeps it honest. This module never sets it from a caller: it
//! reads it off the query it just validated.

use uops_core::{Error as CoreError, Result, SavedSearchId, TenantScope};
use uops_query::{Query, ResolvedResources, compile};

use crate::error::map;
use crate::store::PgStore;

/// A stored search.
#[derive(Clone, Debug)]
pub struct SavedSearch {
    pub id: SavedSearchId,
    pub tenant_id: uops_core::TenantId,
    pub name: String,
    pub description: String,
    /// The AST, exactly as `POST /api/v1/query` would receive it.
    ///
    /// Including the time window it was saved over, which is *provenance* and not the
    /// window it runs with — the Explorer substitutes the operator's current range and
    /// M4's evaluator will substitute its interval. See migration 0013.
    pub query: Query,
    pub created_by: Option<uops_core::ActorId>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// What a caller supplies to save or replace one.
#[derive(Clone, Debug)]
pub struct NewSearch {
    pub name: String,
    pub description: String,
    pub query: Query,
}

/// The row as `PostgreSQL` holds it, before the AST is parsed back out.
struct Row {
    id: SavedSearchId,
    tenant_id: uops_core::TenantId,
    name: String,
    description: String,
    query: serde_json::Value,
    created_by: Option<uops_core::ActorId>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl Row {
    /// # Errors
    ///
    /// `Serialization` when the stored document is not a `Query` any more — which
    /// happens if the AST changes shape incompatibly. Surfaced rather than swallowed: a
    /// saved search that silently disappears from a list because it failed to parse is
    /// a support case nobody can reproduce.
    fn parse(self) -> Result<SavedSearch> {
        Ok(SavedSearch {
            id: self.id,
            tenant_id: self.tenant_id,
            name: self.name,
            description: self.description,
            query: serde_json::from_value(self.query)?,
            created_by: self.created_by,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Reject a query the compiler cannot answer, in the caller's own terms.
///
/// Compiled against the whole tenant rather than against the resources the selector
/// names: expanding a selector is a round trip through `resource_alias`, and a saved
/// search is not made invalid by a resource being decommissioned afterwards. What this
/// catches is the permanent kind of wrong — a field that does not exist on that signal,
/// a bucket outside the compiler's bounds, a signal with no table behind it.
fn must_compile(query: &Query, scope: &TenantScope) -> Result<()> {
    compile(query, scope, &ResolvedResources::whole_tenant(scope))
        .map(|_| ())
        .map_err(|e| CoreError::Invalid(e.to_string()))
}

impl PgStore {
    /// Save a search under a name.
    ///
    /// # Errors
    ///
    /// `Invalid` when the query does not compile, and `Invalid` again when the tenant
    /// already has a search by that name — scoped uniqueness, because two customers of
    /// one MSP both have a search called "BGP".
    pub async fn save_search(
        &self,
        scope: &TenantScope,
        by: Option<uops_core::ActorId>,
        new: &NewSearch,
    ) -> Result<SavedSearch> {
        must_compile(&new.query, scope)?;

        let id = SavedSearchId::new();
        let document = serde_json::to_value(&new.query)?;
        // tenant-exempt: the tenant is the second bound parameter, from the scope.
        let row = sqlx::query_as!(
            Row,
            r#"
            INSERT INTO saved_search
                (id, tenant_id, name, description, signal, query, created_by)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING
                id          AS "id: SavedSearchId",
                tenant_id   AS "tenant_id: uops_core::TenantId",
                name,
                description,
                query,
                created_by  AS "created_by: uops_core::ActorId",
                created_at,
                updated_at
            "#,
            id as SavedSearchId,
            scope.tenant_id() as uops_core::TenantId,
            new.name.trim(),
            new.description,
            // Never from the caller. The CHECK in migration 0013 would refuse a
            // disagreement anyway; taking it from the AST means there is nothing to
            // disagree with.
            new.query.signal.as_str(),
            document,
            // Nullable, because a search created by a system process has no app_user
            // row to point at.
            by as Option<uops_core::ActorId>,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("saved_search", new.name.clone(), e))?;

        row.parse()
    }

    /// A tenant's searches, most recently changed first.
    ///
    /// The ordering an operator wants during an incident: the search they were editing
    /// five minutes ago is the one they want back. `saved_search_by_tenant_idx` serves it
    /// without a sort.
    pub async fn saved_searches(&self, scope: &TenantScope) -> Result<Vec<SavedSearch>> {
        // tenant-exempt: the tenant is the only bound parameter, from the scope.
        let rows = sqlx::query_as!(
            Row,
            r#"
            SELECT
                id          AS "id: SavedSearchId",
                tenant_id   AS "tenant_id: uops_core::TenantId",
                name,
                description,
                query,
                created_by  AS "created_by: uops_core::ActorId",
                created_at,
                updated_at
              FROM saved_search
             WHERE tenant_id = $1
             ORDER BY updated_at DESC
            "#,
            scope.tenant_id() as uops_core::TenantId,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("saved_search", "list".to_owned(), e))?;

        rows.into_iter().map(Row::parse).collect()
    }

    /// One search.
    ///
    /// # Errors
    ///
    /// `NotFound` for a search in another tenant, which is the same answer as for one
    /// that does not exist. 404-never-403: a distinguishable response is a way to
    /// enumerate another customer's searches by id.
    pub async fn saved_search(
        &self,
        scope: &TenantScope,
        id: SavedSearchId,
    ) -> Result<SavedSearch> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query_as!(
            Row,
            r#"
            SELECT
                id          AS "id: SavedSearchId",
                tenant_id   AS "tenant_id: uops_core::TenantId",
                name,
                description,
                query,
                created_by  AS "created_by: uops_core::ActorId",
                created_at,
                updated_at
              FROM saved_search
             WHERE tenant_id = $1 AND id = $2
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id as SavedSearchId,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("saved_search", id.to_string(), e))?
        .ok_or(CoreError::NotFound {
            kind: "saved_search",
            id: id.to_string(),
        })?;

        row.parse()
    }

    /// Replace a search's name, description and query.
    ///
    /// A whole replacement rather than a patch: the three things a search is are the
    /// three things this takes, and a partial update of an AST — "change the filter, keep
    /// the signal" — is a request nothing in the product makes and a merge nobody can
    /// review.
    ///
    /// # Errors
    ///
    /// `Invalid` when the new query does not compile or the new name is already taken
    /// in this tenant, `NotFound` for another tenant's search.
    pub async fn update_search(
        &self,
        scope: &TenantScope,
        id: SavedSearchId,
        new: &NewSearch,
    ) -> Result<SavedSearch> {
        must_compile(&new.query, scope)?;

        let document = serde_json::to_value(&new.query)?;
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query_as!(
            Row,
            r#"
            UPDATE saved_search
               SET name = $3, description = $4, signal = $5, query = $6
             WHERE tenant_id = $1 AND id = $2
            RETURNING
                id          AS "id: SavedSearchId",
                tenant_id   AS "tenant_id: uops_core::TenantId",
                name,
                description,
                query,
                created_by  AS "created_by: uops_core::ActorId",
                created_at,
                updated_at
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id as SavedSearchId,
            new.name.trim(),
            new.description,
            new.query.signal.as_str(),
            document,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("saved_search", new.name.clone(), e))?
        .ok_or(CoreError::NotFound {
            kind: "saved_search",
            id: id.to_string(),
        })?;

        row.parse()
    }

    /// Delete a search.
    ///
    /// Gone, not archived. A search is a question somebody wrote down; there is nothing
    /// downstream holding a reference to it, and a "deleted searches" list is a feature
    /// nobody asked for. When M4 builds an alert rule from one it will copy the AST, not
    /// point at the row — so deleting the search must not silently disable an alert.
    ///
    /// # Errors
    ///
    /// `NotFound` for another tenant's search, so deleting is not a way to discover that
    /// an id exists.
    pub async fn delete_search(&self, scope: &TenantScope, id: SavedSearchId) -> Result<()> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let done = sqlx::query!(
            "DELETE FROM saved_search WHERE tenant_id = $1 AND id = $2",
            scope.tenant_id() as uops_core::TenantId,
            id as SavedSearchId,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("saved_search", id.to_string(), e))?;

        if done.rows_affected() == 0 {
            return Err(CoreError::NotFound {
                kind: "saved_search",
                id: id.to_string(),
            });
        }
        Ok(())
    }
}
