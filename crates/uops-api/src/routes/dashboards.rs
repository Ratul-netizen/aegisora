//! Dashboards — SPEC §M4.
//!
//! Five verbs over a document: a name and an ordered list of panels, each of which is a
//! `Query` AST and a picture to draw it as.
//!
//! # No panel-level route
//!
//! Adding, moving and removing a panel are all `PUT` of the whole dashboard. A panel has
//! no life outside the dashboard it is on, and a `PATCH /dashboards/{id}/panels/{id}`
//! would be a second way to write the same row — with its own ordering semantics to get
//! wrong, for a document a few kilobytes long that one person edits at a time.
//!
//! # Roles
//!
//! Reading is `Viewer`; writing is `Operator`. A dashboard is the team's screen rather
//! than a personal one — the same reasoning as a saved search, and the same consequence:
//! what somebody builds, their colleagues see.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uops_core::Role;
use uops_store_pg::{NewDashboard, Panel};

use crate::csrf::CsrfChecked;
use crate::error::ApiResult;
use crate::extract::Caller;
use crate::state::AppState;

/// A dashboard, as the UI shows it.
#[derive(Debug, Serialize)]
pub struct DashboardView {
    pub id: uuid::Uuid,
    pub name: String,
    pub description: String,
    pub panels: Vec<Panel>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn view(d: uops_store_pg::Dashboard) -> DashboardView {
    DashboardView {
        id: d.id,
        name: d.name,
        description: d.description,
        panels: d.panels,
        created_at: d.created_at,
        updated_at: d.updated_at,
    }
}

/// What a client sends to create or replace one.
#[derive(Debug, Deserialize)]
pub struct DashboardRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub panels: Vec<Panel>,
}

impl From<DashboardRequest> for NewDashboard {
    fn from(body: DashboardRequest) -> Self {
        Self {
            name: body.name,
            description: body.description,
            panels: body.panels,
        }
    }
}

/// `GET /api/v1/dashboards`
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<Vec<DashboardView>>> {
    caller.require(Role::Viewer)?;

    let dashboards = state.store.dashboards(caller.scope()).await?;
    caller.audit().read(
        "dashboards.list",
        Some(i64::try_from(dashboards.len()).unwrap_or(i64::MAX)),
    );

    Ok(Json(dashboards.into_iter().map(view).collect()))
}

/// `GET /api/v1/dashboards/{id}`
pub async fn get(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<Json<DashboardView>> {
    caller.require(Role::Viewer)?;

    let dashboard = state.store.dashboard(caller.scope(), id).await?;
    caller.audit().read("dashboards.get", None);

    Ok(Json(view(dashboard)))
}

/// `POST /api/v1/dashboards`
///
/// Every panel's query is compiled before the row is written, so a panel that cannot be
/// answered is refused here rather than becoming an error box on a wall display.
pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    _csrf: CsrfChecked,
    Json(body): Json<DashboardRequest>,
) -> ApiResult<(StatusCode, Json<DashboardView>)> {
    caller.require(Role::Operator)?;

    let dashboard = state
        .store
        .create_dashboard(caller.scope(), Some(caller.user_id()), &body.into())
        .await?;

    // The name and the shape, never the panels' queries: a panel's filter carries the
    // customer's hostnames exactly as a saved search's does.
    caller.audit().wrote(
        "dashboards.create",
        format!("dashboard:{}", dashboard.id),
        None,
        Some(serde_json::json!({
            "name": dashboard.name,
            "panels": dashboard.panels.len(),
        })),
    );

    Ok((StatusCode::CREATED, Json(view(dashboard))))
}

/// `PUT /api/v1/dashboards/{id}`
pub async fn update(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
    _csrf: CsrfChecked,
    Json(body): Json<DashboardRequest>,
) -> ApiResult<Json<DashboardView>> {
    caller.require(Role::Operator)?;

    let before = state.store.dashboard(caller.scope(), id).await?;
    let dashboard = state
        .store
        .update_dashboard(caller.scope(), id, &body.into())
        .await?;

    caller.audit().wrote(
        "dashboards.update",
        format!("dashboard:{id}"),
        Some(serde_json::json!({
            "name": before.name,
            "panels": before.panels.len(),
        })),
        Some(serde_json::json!({
            "name": dashboard.name,
            "panels": dashboard.panels.len(),
        })),
    );

    Ok(Json(view(dashboard)))
}

/// `DELETE /api/v1/dashboards/{id}`
pub async fn delete(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
    _csrf: CsrfChecked,
) -> ApiResult<StatusCode> {
    caller.require(Role::Operator)?;

    let before = state.store.dashboard(caller.scope(), id).await?;
    state.store.delete_dashboard(caller.scope(), id).await?;

    caller.audit().wrote(
        "dashboards.delete",
        format!("dashboard:{id}"),
        Some(serde_json::json!({
            "name": before.name,
            "panels": before.panels.len(),
        })),
        None,
    );

    Ok(StatusCode::NO_CONTENT)
}
