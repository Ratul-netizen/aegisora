//! Resource inventory — SPEC §M1.
//!
//! Thin, and that is the achievement rather than a shortcut. Pagination, filtering, the
//! tenant predicate and the soft delete are all decided in `uops-store-pg` and tested
//! against a real database; the role checks are decided in [`crate::Caller`]. What is
//! left here is deciding *which* role each verb needs and saying what happened for the
//! audit log.
//!
//! # The shape every handler follows
//!
//! 1. Take a `Caller` — which has already proved the session, the tenant and the role.
//! 2. `require` the role this verb needs.
//! 3. Call the repository with `caller.scope()`.
//! 4. Tell [`crate::Audit`] what was read or changed.
//!
//! Step 4 is not optional in practice: the `Caller` that made steps 1–3 possible is the
//! same object that carries the audit handle, so a handler holding one has already been
//! attributed.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use uops_core::{Resource, ResourceId, ResourceKind, ResourceStatus, Role, SiteId};
use uops_store_pg::{Cursor, NewResource, ResourceFilter};

use crate::csrf::CsrfChecked;
use crate::error::{ApiError, ApiResult};
use crate::extract::Caller;
use crate::state::AppState;

/// `GET /api/v1/resources?kind=&site=&status=&q=&cursor=&limit=`
#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    pub kind: Option<ResourceKind>,
    pub site: Option<SiteId>,
    pub status: Option<ResourceStatus>,
    /// Substring of the name or display name.
    pub q: Option<String>,
    pub cursor: Option<ResourceId>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct PageResponse<T> {
    pub items: Vec<T>,
    /// Absent on the last page. Opaque by convention — it is a `ResourceId` today and
    /// that is not a promise.
    pub next: Option<String>,
}

/// What a client may send when creating a resource.
///
/// Not `Resource`: `id`, `tenant_id` and the timestamps belong to the server, and
/// accepting them from a caller is how a resource ends up in the wrong tenant.
#[derive(Debug, Deserialize)]
pub struct CreateResource {
    pub kind: ResourceKind,
    pub name: String,
    pub display_name: Option<String>,
    pub site_id: Option<SiteId>,
    pub parent_id: Option<ResourceId>,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub os: Option<String>,
    pub os_version: Option<String>,
}

/// `GET /api/v1/resources`
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
    Query(params): Query<ListParams>,
) -> ApiResult<Json<PageResponse<Resource>>> {
    caller.require(Role::Viewer)?;

    let page = state
        .store
        .resources(
            caller.scope(),
            &ResourceFilter {
                kind: params.kind,
                site_id: params.site,
                status: params.status,
                name_contains: params.q,
                cursor: params.cursor.map(Cursor),
                limit: params.limit,
            },
        )
        .await?;

    // The row count matters: "listed the inventory" and "paged through forty thousand
    // devices" are different events, and only the second is what an investigation looks
    // for.
    caller
        .audit()
        .read("resources", Some(page.len().try_into().unwrap_or(i64::MAX)));

    Ok(Json(PageResponse {
        items: page.items,
        next: page.next.map(|c| c.to_string()),
    }))
}

/// `GET /api/v1/resources/{id}`
pub async fn get(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<ResourceId>,
) -> ApiResult<Json<Resource>> {
    caller.require(Role::Viewer)?;

    // A resource in another tenant does not match the predicate, so this is NotFound
    // rather than Forbidden — the repository decides that, not this handler.
    let resource = state.store.resource(caller.scope(), id).await?;

    caller.audit().read(format!("resource:{id}"), Some(1));
    Ok(Json(resource))
}

/// `POST /api/v1/resources`
pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    _csrf: CsrfChecked,
    Json(body): Json<CreateResource>,
) -> ApiResult<(StatusCode, Json<Resource>)> {
    caller.require(Role::Operator)?;

    if body.name.trim().is_empty() {
        return Err(ApiError::BadRequest("name cannot be empty".into()));
    }

    let created = state
        .store
        .create_resource(
            caller.scope(),
            &NewResource {
                kind: body.kind,
                name: body.name,
                display_name: body.display_name,
                site_id: body.site_id,
                parent_id: body.parent_id,
                vendor: body.vendor,
                model: body.model,
                os: body.os,
                os_version: body.os_version,
                attributes: uops_core::AttrMap::new(),
            },
        )
        .await?;

    caller.audit().wrote(
        "resource.create",
        format!("resource:{}", created.id),
        None,
        Some(summarise(&created)),
    );

    Ok((StatusCode::CREATED, Json(created)))
}

#[derive(Debug, Deserialize)]
pub struct StatusChange {
    pub status: ResourceStatus,
}

/// `PATCH /api/v1/resources/{id}/status`
pub async fn set_status(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<ResourceId>,
    _csrf: CsrfChecked,
    Json(body): Json<StatusChange>,
) -> ApiResult<Json<Resource>> {
    caller.require(Role::Operator)?;

    // Read first, so the audit entry can carry a real before. Without it, "who set this
    // to maintenance and what was it before" needs the whole history replayed.
    let before = state.store.resource(caller.scope(), id).await?;
    let after = state
        .store
        .set_resource_status(caller.scope(), id, body.status)
        .await?;

    caller.audit().wrote(
        "resource.status",
        format!("resource:{id}"),
        Some(summarise(&before)),
        Some(summarise(&after)),
    );

    Ok(Json(after))
}

/// `DELETE /api/v1/resources/{id}`
///
/// A soft delete — SPEC §M1 is explicit. A hard delete would orphan every piece of
/// telemetry already written under this `resource_id`, and history that resolves to
/// nothing is worse than a row marked retired.
pub async fn decommission(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<ResourceId>,
    _csrf: CsrfChecked,
) -> ApiResult<Json<Resource>> {
    caller.require(Role::Operator)?;

    let before = state.store.resource(caller.scope(), id).await?;
    let after = state
        .store
        .set_resource_status(caller.scope(), id, ResourceStatus::Decommissioned)
        .await?;

    caller.audit().wrote(
        "resource.decommission",
        format!("resource:{id}"),
        Some(summarise(&before)),
        Some(summarise(&after)),
    );

    Ok(Json(after))
}

/// What goes in an audit row's before/after.
///
/// A summary rather than the whole resource: the audit log is read by people asking
/// what changed, and a wall of unchanged fields buries the one that did. Attributes are
/// deliberately excluded — they can be large, and they are not what a status change or
/// a rename is about.
fn summarise(r: &Resource) -> serde_json::Value {
    serde_json::json!({
        "name": r.name,
        "display_name": r.display_name,
        "kind": r.kind.as_str(),
        "status": r.status.as_str(),
        "site_id": r.site_id.map(|s| s.to_string()),
        "parent_id": r.parent_id.map(|p| p.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uops_core::TenantId;

    fn resource() -> Resource {
        Resource {
            id: ResourceId::new(),
            tenant_id: TenantId::new(),
            site_id: None,
            parent_id: None,
            kind: ResourceKind::Device,
            name: "rtr-01".into(),
            display_name: Some("Core Router".into()),
            vendor: Some("cisco".into()),
            model: None,
            os: None,
            os_version: None,
            status: ResourceStatus::Up,
            profile_id: None,
            credential_ref: None,
            attributes: uops_core::AttrMap::new(),
            tags: uops_core::Tags::new(),
            first_seen: chrono::Utc::now(),
            last_seen: chrono::Utc::now(),
        }
    }

    #[test]
    fn an_audit_summary_carries_what_changes_and_not_what_does_not() {
        let summary = summarise(&resource());

        assert_eq!(summary["name"], "rtr-01");
        assert_eq!(summary["status"], "up");
        assert_eq!(summary["display_name"], "Core Router");

        // Attributes can be large and are not what a rename or a status change is
        // about. A wall of unchanged fields buries the one that did change.
        assert!(summary.get("attributes").is_none(), "{summary}");
        assert!(summary.get("first_seen").is_none(), "{summary}");
    }

    #[test]
    fn a_summary_survives_a_resource_with_nothing_optional_set() {
        let mut bare = resource();
        bare.display_name = None;
        bare.vendor = None;

        let summary = summarise(&bare);
        assert!(summary["display_name"].is_null());
        assert!(summary["site_id"].is_null());
    }
}
