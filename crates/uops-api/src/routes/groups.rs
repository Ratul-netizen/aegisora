//! Resource groups, and the tags that are not attributes.
//!
//! Both are things only a person writes, which is why they are here and not reachable
//! from any collector. See [`uops_core::tags`] for why `tags` and `attributes` are two
//! columns and must stay that way.
//!
//! # Roles
//!
//! Reading is `Viewer`. Everything that writes is `Operator`, because a group is what an
//! alert rule will be scoped to and a tag is what a notification will be routed by — so
//! editing either changes who gets woken up, and that is not a viewer's call.
//!
//! # 404-never-403
//!
//! Every route answers `NotFound` for another tenant's group, which is the same answer
//! as for one that never existed. The isolation harness found this exact bug twice in the
//! credential routes — a `204` on revoke and a `200 []` on list — so it is asserted here
//! rather than assumed.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use uops_core::{ResourceGroupId, ResourceId, Role, Tags};
use uops_store_pg::NewGroup;

use crate::csrf::CsrfChecked;
use crate::error::ApiResult;
use crate::extract::Caller;
use crate::state::AppState;

/// One group, as a list or a detail view shows it.
#[derive(Debug, Serialize)]
pub struct GroupView {
    pub id: ResourceGroupId,
    pub name: String,
    pub description: String,
    /// How many resources are in it. The list view wants this and never the members: a
    /// page of forty groups must not read forty membership lists to render forty numbers.
    pub members: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<uops_store_pg::GroupSummary> for GroupView {
    fn from(s: uops_store_pg::GroupSummary) -> Self {
        Self {
            id: s.group.id,
            name: s.group.name,
            description: s.group.description,
            members: s.members,
            created_at: s.group.created_at,
            updated_at: s.group.updated_at,
        }
    }
}

/// `GET /api/v1/groups`
///
/// Not paginated. A tenant has groups in the tens — they are written by hand, one per
/// operational concept — and a cursor would be a second round trip for a sidebar.
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<Vec<GroupView>>> {
    caller.require(Role::Viewer)?;

    let groups = state.store.groups(caller.scope()).await?;
    caller.audit().read(
        "group.list",
        Some(i64::try_from(groups.len()).unwrap_or(i64::MAX)),
    );

    Ok(Json(groups.into_iter().map(GroupView::from).collect()))
}

#[derive(Debug, Deserialize)]
pub struct GroupBody {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// `POST /api/v1/groups`
pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    _csrf: CsrfChecked,
    Json(body): Json<GroupBody>,
) -> ApiResult<(StatusCode, Json<GroupView>)> {
    caller.require(Role::Operator)?;

    let group = state
        .store
        .create_group(
            caller.scope(),
            &NewGroup {
                name: body.name,
                description: body.description,
            },
        )
        .await?;

    caller.audit().wrote(
        "group.create",
        format!("group:{}", group.id),
        None,
        Some(serde_json::json!({ "name": group.name })),
    );

    Ok((
        StatusCode::CREATED,
        Json(GroupView {
            id: group.id,
            name: group.name,
            description: group.description,
            // Nothing is in a group the moment it is made.
            members: 0,
            created_at: group.created_at,
            updated_at: group.updated_at,
        }),
    ))
}

/// `PUT /api/v1/groups/{id}`
pub async fn rename(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<ResourceGroupId>,
    _csrf: CsrfChecked,
    Json(body): Json<GroupBody>,
) -> ApiResult<Json<GroupView>> {
    caller.require(Role::Operator)?;

    let before = state.store.group(caller.scope(), id).await?;
    let group = state
        .store
        .rename_group(
            caller.scope(),
            id,
            &NewGroup {
                name: body.name,
                description: body.description,
            },
        )
        .await?;

    caller.audit().wrote(
        "group.rename",
        format!("group:{id}"),
        Some(serde_json::json!({ "name": before.name })),
        Some(serde_json::json!({ "name": group.name })),
    );

    // Re-read for the count rather than carrying one through the update. A rename does
    // not change membership, and a response that guessed zero would be wrong in the one
    // place a client would believe it.
    let members = state
        .store
        .groups(caller.scope())
        .await?
        .into_iter()
        .find(|g| g.group.id == id)
        .map_or(0, |g| g.members);

    Ok(Json(GroupView {
        id: group.id,
        name: group.name,
        description: group.description,
        members,
        created_at: group.created_at,
        updated_at: group.updated_at,
    }))
}

/// `DELETE /api/v1/groups/{id}`
///
/// Removes the group and its membership rows. The resources are untouched — deleting
/// "Critical Servers" must not delete the critical servers.
pub async fn delete(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<ResourceGroupId>,
    _csrf: CsrfChecked,
) -> ApiResult<StatusCode> {
    caller.require(Role::Operator)?;

    let before = state.store.group(caller.scope(), id).await?;
    state.store.delete_group(caller.scope(), id).await?;

    caller.audit().wrote(
        "group.delete",
        format!("group:{id}"),
        Some(serde_json::json!({ "name": before.name })),
        None,
    );

    Ok(StatusCode::NO_CONTENT)
}

/// What a client sends to change membership.
#[derive(Debug, Deserialize)]
pub struct Membership {
    pub resources: Vec<ResourceId>,
}

/// How many rows actually changed.
///
/// Reported rather than swallowed because both operations are idempotent: adding a
/// resource that is already a member is not an error, and a client that selected forty
/// rows and changed two probably wants to know.
#[derive(Debug, Serialize)]
pub struct Changed {
    pub changed: u64,
}

/// `POST /api/v1/groups/{id}/members`
pub async fn add_members(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<ResourceGroupId>,
    _csrf: CsrfChecked,
    Json(body): Json<Membership>,
) -> ApiResult<Json<Changed>> {
    caller.require(Role::Operator)?;

    // Establishes that the group is this tenant's before the composite foreign key would
    // — so another tenant's group id gets a 404 rather than a constraint violation
    // rendered as a 500.
    state.store.group(caller.scope(), id).await?;

    let changed = state
        .store
        .add_to_group(caller.scope(), id, &body.resources)
        .await?;

    caller.audit().wrote(
        "group.members.add",
        format!("group:{id}"),
        None,
        Some(serde_json::json!({ "requested": body.resources.len(), "added": changed })),
    );

    Ok(Json(Changed { changed }))
}

/// `DELETE /api/v1/groups/{id}/members`
///
/// A body on a DELETE, deliberately. The alternative is one request per resource, and
/// taking forty devices out of a group should not be forty audit rows and forty round
/// trips.
pub async fn remove_members(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<ResourceGroupId>,
    _csrf: CsrfChecked,
    Json(body): Json<Membership>,
) -> ApiResult<Json<Changed>> {
    caller.require(Role::Operator)?;

    state.store.group(caller.scope(), id).await?;

    let changed = state
        .store
        .remove_from_group(caller.scope(), id, &body.resources)
        .await?;

    caller.audit().wrote(
        "group.members.remove",
        format!("group:{id}"),
        None,
        Some(serde_json::json!({ "requested": body.resources.len(), "removed": changed })),
    );

    Ok(Json(Changed { changed }))
}

/// `GET /api/v1/resources/{id}/groups`
///
/// The reverse direction, for the resource detail page.
pub async fn of_resource(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<ResourceId>,
) -> ApiResult<Json<Vec<GroupView>>> {
    caller.require(Role::Viewer)?;

    // Same reason as above: another tenant's resource is a 404 here, not an empty list.
    state.store.resource(caller.scope(), id).await?;

    let groups = state.store.groups_of(caller.scope(), id).await?;
    caller.audit().read("group.of_resource", None);

    Ok(Json(
        groups
            .into_iter()
            .map(|g| GroupView {
                id: g.id,
                name: g.name,
                description: g.description,
                // Not counted here. This answers "which groups is this in", and a size
                // per group would be a query per row of a list that is usually short and
                // never sorted by it.
                members: 0,
                created_at: g.created_at,
                updated_at: g.updated_at,
            })
            .collect(),
    ))
}

/// `PUT /api/v1/resources/{id}/tags`
///
/// Replaces the whole map. That is how a tag is removed — a merge-only API would need a
/// second endpoint to delete one, and *"I removed `criticality=critical` and it came
/// back"* is a bug report nobody should have to file.
///
/// Writes `tags` and never `attributes`. Discovery owns that column, and a route that
/// could reach it would make every operator edit a race with the next SNMP walk.
pub async fn set_tags(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<ResourceId>,
    _csrf: CsrfChecked,
    Json(tags): Json<Tags>,
) -> ApiResult<Json<Tags>> {
    caller.require(Role::Operator)?;

    let before = state.store.resource(caller.scope(), id).await?;
    let updated = state.store.set_tags(caller.scope(), id, &tags).await?;

    caller.audit().wrote(
        "resource.tags",
        format!("resource:{id}"),
        Some(serde_json::to_value(&before.tags).unwrap_or(serde_json::Value::Null)),
        Some(serde_json::to_value(&updated.tags).unwrap_or(serde_json::Value::Null)),
    );

    Ok(Json(updated.tags))
}
