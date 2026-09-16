//! Resource groups, and the tags that are not attributes.
//!
//! Two features from the architecture review of 2026-09-16, in one module because they
//! answer the same question — *which resources does an operator mean?* — and both are
//! things only a human writes.
//!
//! # Groups
//!
//! Membership is an explicit list. Every write goes through [`PgStore`], and every one of
//! them is tenant-scoped by the composite foreign keys in migration 0011 rather than by a
//! predicate anyone has to remember.
//!
//! # Tags
//!
//! `attributes` is written by collectors on every walk; `tags` is written by humans and
//! by nothing else. That separation is the whole feature — see [`uops_core::tags`] — and
//! the reason there is a [`set_tags`](PgStore::set_tags) here and deliberately no way for
//! discovery to reach the column.

use uops_core::{
    Error as CoreError, Resource, ResourceGroup, ResourceGroupId, ResourceId, Result, Tags,
    TenantScope,
};

use crate::error::map;
use crate::store::PgStore;

/// What a caller supplies to create or rename a group.
#[derive(Clone, Debug)]
pub struct NewGroup {
    pub name: String,
    pub description: String,
}

impl NewGroup {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
        }
    }

    #[must_use]
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }
}

/// A group with how many resources are in it.
///
/// The list view wants the count and never the members: a page showing forty groups must
/// not read forty membership lists to render forty numbers.
#[derive(Clone, Debug)]
pub struct GroupSummary {
    pub group: ResourceGroup,
    pub members: i64,
}

impl PgStore {
    /// Create a group in the scope's tenant.
    ///
    /// # Errors
    ///
    /// `Conflict` when the tenant already has a group of that name — scoped uniqueness,
    /// because two customers of one MSP both have core routers.
    pub async fn create_group(&self, scope: &TenantScope, new: &NewGroup) -> Result<ResourceGroup> {
        let id = ResourceGroupId::new();
        // tenant-exempt: the tenant is the second bound parameter, from the scope.
        let row = sqlx::query_as!(
            ResourceGroup,
            r#"
            INSERT INTO resource_group (id, tenant_id, name, description)
            VALUES ($1, $2, $3, $4)
            RETURNING
                id          AS "id: ResourceGroupId",
                tenant_id   AS "tenant_id: uops_core::TenantId",
                name,
                description,
                created_at,
                updated_at
            "#,
            id as ResourceGroupId,
            scope.tenant_id() as uops_core::TenantId,
            new.name.trim(),
            new.description,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("resource_group", new.name.clone(), e))?;

        Ok(row)
    }

    /// One group.
    ///
    /// # Errors
    ///
    /// `NotFound` for a group in another tenant, which is the same answer as for one that
    /// does not exist. 404-never-403: a distinguishable response is a way to enumerate
    /// another customer's groups by id.
    pub async fn group(&self, scope: &TenantScope, id: ResourceGroupId) -> Result<ResourceGroup> {
        sqlx::query_as!(
            ResourceGroup,
            r#"
            SELECT
                id          AS "id: ResourceGroupId",
                tenant_id   AS "tenant_id: uops_core::TenantId",
                name,
                description,
                created_at,
                updated_at
              FROM resource_group
             WHERE tenant_id = $1 AND id = $2
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id as ResourceGroupId,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("resource_group", id.to_string(), e))
    }

    /// Every group in the tenant, with its size, alphabetically.
    ///
    /// A `LEFT JOIN` rather than a correlated subquery so that an empty group appears
    /// with a zero: a group somebody created and has not filled yet is exactly the one
    /// they are looking for.
    pub async fn groups(&self, scope: &TenantScope) -> Result<Vec<GroupSummary>> {
        let rows = sqlx::query!(
            r#"
            SELECT
                g.id          AS "id: ResourceGroupId",
                g.tenant_id   AS "tenant_id: uops_core::TenantId",
                g.name,
                g.description,
                g.created_at,
                g.updated_at,
                count(m.resource_id) AS "members!: i64"
              FROM resource_group g
              LEFT JOIN resource_group_member m
                     ON m.group_id = g.id AND m.tenant_id = g.tenant_id
             WHERE g.tenant_id = $1
             GROUP BY g.id, g.tenant_id, g.name, g.description, g.created_at, g.updated_at
             ORDER BY g.name
            "#,
            scope.tenant_id() as uops_core::TenantId,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("resource_group", String::new(), e))?;

        Ok(rows
            .into_iter()
            .map(|r| GroupSummary {
                group: ResourceGroup {
                    id: r.id,
                    tenant_id: r.tenant_id,
                    name: r.name,
                    description: r.description,
                    created_at: r.created_at,
                    updated_at: r.updated_at,
                },
                members: r.members,
            })
            .collect())
    }

    /// Rename or re-describe a group.
    ///
    /// # Errors
    ///
    /// `NotFound` when the group is not this tenant's.
    pub async fn rename_group(
        &self,
        scope: &TenantScope,
        id: ResourceGroupId,
        new: &NewGroup,
    ) -> Result<ResourceGroup> {
        sqlx::query_as!(
            ResourceGroup,
            r#"
            UPDATE resource_group
               SET name = $3, description = $4
             WHERE tenant_id = $1 AND id = $2
            RETURNING
                id          AS "id: ResourceGroupId",
                tenant_id   AS "tenant_id: uops_core::TenantId",
                name,
                description,
                created_at,
                updated_at
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id as ResourceGroupId,
            new.name.trim(),
            new.description,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("resource_group", id.to_string(), e))
    }

    /// Delete a group. Its membership rows go with it; the resources do not.
    ///
    /// # Errors
    ///
    /// `NotFound` when the group is not this tenant's — rather than a silent 204, which
    /// would tell a caller nothing and would have hidden the two isolation bugs the
    /// credential routes had.
    pub async fn delete_group(&self, scope: &TenantScope, id: ResourceGroupId) -> Result<()> {
        let done = sqlx::query!(
            "DELETE FROM resource_group WHERE tenant_id = $1 AND id = $2",
            scope.tenant_id() as uops_core::TenantId,
            id as ResourceGroupId,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("resource_group", id.to_string(), e))?;

        if done.rows_affected() == 0 {
            return Err(CoreError::NotFound {
                kind: "resource_group",
                id: id.to_string(),
            });
        }
        Ok(())
    }

    /// Put resources in a group. Already-present ones are left alone.
    ///
    /// Idempotent by `ON CONFLICT DO NOTHING`: adding a resource that is already a member
    /// is what a UI does when somebody re-selects a row, and it is not an error.
    ///
    /// # Errors
    ///
    /// `NotFound` when either the group or any resource is not this tenant's. The
    /// composite foreign keys refuse the write, so this is the database's answer rather
    /// than a check that could drift out of step with it.
    pub async fn add_to_group(
        &self,
        scope: &TenantScope,
        group: ResourceGroupId,
        resources: &[ResourceId],
    ) -> Result<u64> {
        if resources.is_empty() {
            return Ok(0);
        }
        let ids: Vec<uuid::Uuid> = resources.iter().map(|r| r.into_uuid()).collect();

        let done = sqlx::query!(
            r#"
            INSERT INTO resource_group_member (tenant_id, group_id, resource_id)
            SELECT $1, $2, given
              FROM unnest($3::uuid[]) AS given
            ON CONFLICT DO NOTHING
            "#,
            scope.tenant_id() as uops_core::TenantId,
            group as ResourceGroupId,
            &ids,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("resource_group", group.to_string(), e))?;

        Ok(done.rows_affected())
    }

    /// Take resources out of a group.
    ///
    /// Removing something that was never in it is not an error, for the same reason
    /// adding a duplicate is not: the caller's intent — *this resource is not in this
    /// group* — is satisfied either way.
    pub async fn remove_from_group(
        &self,
        scope: &TenantScope,
        group: ResourceGroupId,
        resources: &[ResourceId],
    ) -> Result<u64> {
        if resources.is_empty() {
            return Ok(0);
        }
        let ids: Vec<uuid::Uuid> = resources.iter().map(|r| r.into_uuid()).collect();

        let done = sqlx::query!(
            r#"
            DELETE FROM resource_group_member
             WHERE tenant_id = $1 AND group_id = $2 AND resource_id = ANY($3::uuid[])
            "#,
            scope.tenant_id() as uops_core::TenantId,
            group as ResourceGroupId,
            &ids,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("resource_group", group.to_string(), e))?;

        Ok(done.rows_affected())
    }

    /// Which groups a resource belongs to.
    ///
    /// Served by `resource_group_member_resource_idx`, which exists for this query and
    /// for the foreign key underneath it.
    pub async fn groups_of(
        &self,
        scope: &TenantScope,
        resource: ResourceId,
    ) -> Result<Vec<ResourceGroup>> {
        sqlx::query_as!(
            ResourceGroup,
            r#"
            SELECT
                g.id        AS "id: ResourceGroupId",
                g.tenant_id AS "tenant_id: uops_core::TenantId",
                g.name,
                g.description,
                g.created_at,
                g.updated_at
              FROM resource_group g
              JOIN resource_group_member m
                    ON m.group_id = g.id AND m.tenant_id = g.tenant_id
             WHERE m.tenant_id = $1 AND m.resource_id = $2
             ORDER BY g.name
            "#,
            scope.tenant_id() as uops_core::TenantId,
            resource as ResourceId,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("resource_group", resource.to_string(), e))
    }

    /// Replace a resource's operator tags.
    ///
    /// Replace rather than merge, and deliberately: a `PUT` of the whole map is how a
    /// human removes a tag. A merge-only API would have no way to delete one except a
    /// second endpoint, and *"I removed `criticality=critical` and it came back"* is a
    /// bug report nobody should have to file.
    ///
    /// **Nothing automatic calls this.** Discovery writes `attributes`; this column is
    /// only ever written by a person. See [`uops_core::tags`].
    ///
    /// # Errors
    ///
    /// `Invalid` when the tags fail [`Tags::validate`] — checked here rather than only in
    /// the database, so the caller gets *which* tag is wrong instead of a constraint
    /// name. `NotFound` when the resource is not this tenant's.
    pub async fn set_tags(
        &self,
        scope: &TenantScope,
        resource: ResourceId,
        tags: &Tags,
    ) -> Result<Resource> {
        tags.validate()
            .map_err(|e| CoreError::Invalid(e.to_string()))?;

        let encoded = sqlx::types::Json(tags.clone());
        let row = sqlx::query!(
            r#"
            UPDATE resource
               SET tags = $3
             WHERE tenant_id = $1 AND id = $2
            RETURNING id
            "#,
            scope.tenant_id() as uops_core::TenantId,
            resource as ResourceId,
            encoded as sqlx::types::Json<Tags>,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("resource", resource.to_string(), e))?;

        if row.is_none() {
            return Err(CoreError::NotFound {
                kind: "resource",
                id: resource.to_string(),
            });
        }
        self.resource(scope, resource).await
    }
}
