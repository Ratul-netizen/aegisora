//! Maintenance windows — telling the product about planned work.
//!
//! Four verbs and no update. A window is scheduled, listed, inspected and cancelled;
//! changing one means cancelling it and scheduling another, which is deliberate: an edit
//! to a window that is currently *open* has no obvious meaning — does the suppression
//! that already happened stay suppressed? — and answering that badly is worse than making
//! somebody do it in two steps. The audit log also reads better as "cancelled, then
//! scheduled" than as a diff nobody can reconstruct.
//!
//! # Roles
//!
//! Reading is `Viewer`. Scheduling and cancelling are `Operator`, because both change who
//! gets woken up — scheduling silences alerts and cancelling un-silences them, and the
//! second is as consequential as the first during work that is still going on.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uops_core::{Recurrence, Role, Schedule, Suppression, Target};
use uops_store_pg::NewWindow;

use crate::csrf::CsrfChecked;
use crate::error::ApiResult;
use crate::extract::Caller;
use crate::state::AppState;

/// One window, as the UI shows it.
#[derive(Debug, Serialize)]
pub struct WindowView {
    pub id: uuid::Uuid,
    pub reason: String,
    #[serde(flatten)]
    pub target: Target,
    pub starts_at: DateTime<Utc>,
    pub duration_minutes: i64,
    pub timezone: String,
    pub recurrence: Recurrence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<DateTime<Utc>>,
    pub suppression: Suppression,
    /// Whether it is open at the instant this response was built.
    ///
    /// Computed rather than stored, and returned because it is the single thing an
    /// operator looking at this list wants to know. A client working it out for itself
    /// would have to reimplement the DST rules in JavaScript, and it would get the
    /// fall-back case wrong.
    pub open_now: bool,
    pub created_at: DateTime<Utc>,
}

fn view(w: uops_store_pg::MaintenanceWindow, now: DateTime<Utc>) -> WindowView {
    WindowView {
        id: w.id,
        reason: w.reason,
        target: w.target,
        starts_at: w.schedule.starts_at,
        duration_minutes: w.schedule.duration_minutes,
        timezone: w.schedule.timezone.clone(),
        recurrence: w.schedule.recurrence,
        until: w.schedule.until,
        suppression: w.suppression,
        open_now: w.schedule.is_open_at(now),
        created_at: w.created_at,
    }
}

/// `GET /api/v1/maintenance`
///
/// Every window that could still open, soonest first. Windows past their `until` are not
/// listed: they can never open again, and an estate accumulates finished one-off windows
/// forever.
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<Vec<WindowView>>> {
    caller.require(Role::Viewer)?;

    let now = Utc::now();
    let windows = state.store.live_windows(caller.scope(), now).await?;
    caller.audit().read(
        "maintenance.list",
        Some(i64::try_from(windows.len()).unwrap_or(i64::MAX)),
    );

    Ok(Json(windows.into_iter().map(|w| view(w, now)).collect()))
}

/// `GET /api/v1/maintenance/{id}`
pub async fn get(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<Json<WindowView>> {
    caller.require(Role::Viewer)?;

    let window = state.store.maintenance_window(caller.scope(), id).await?;
    caller.audit().read("maintenance.get", None);

    Ok(Json(view(window, Utc::now())))
}

/// What a client sends to schedule one.
#[derive(Debug, Deserialize)]
pub struct ScheduleRequest {
    pub reason: String,
    #[serde(flatten)]
    pub target: Target,
    pub starts_at: DateTime<Utc>,
    pub duration_minutes: i64,
    pub timezone: String,
    pub recurrence: Recurrence,
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    /// Both flags default to suppressing. An operator who sends no preference is asking
    /// for quiet, which is the whole reason they are scheduling a window.
    #[serde(default)]
    pub suppression: Suppression,
}

/// `POST /api/v1/maintenance`
pub async fn schedule(
    State(state): State<AppState>,
    caller: Caller,
    _csrf: CsrfChecked,
    Json(body): Json<ScheduleRequest>,
) -> ApiResult<(StatusCode, Json<WindowView>)> {
    caller.require(Role::Operator)?;

    let window = state
        .store
        .schedule_maintenance(
            caller.scope(),
            Some(caller.user_id()),
            &NewWindow {
                reason: body.reason,
                target: body.target,
                schedule: Schedule {
                    starts_at: body.starts_at,
                    duration_minutes: body.duration_minutes,
                    timezone: body.timezone,
                    recurrence: body.recurrence,
                    until: body.until,
                },
                suppression: body.suppression,
            },
        )
        .await?;

    caller.audit().wrote(
        "maintenance.schedule",
        format!("maintenance:{}", window.id),
        None,
        Some(serde_json::json!({
            "reason": window.reason,
            "target": window.target.kind(),
            "starts_at": window.schedule.starts_at,
            "duration_minutes": window.schedule.duration_minutes,
            "timezone": window.schedule.timezone,
            "recurrence": window.schedule.recurrence.as_str(),
            "suppress_alerts": window.suppression.alerts,
        })),
    );

    Ok((StatusCode::CREATED, Json(view(window, Utc::now()))))
}

/// `DELETE /api/v1/maintenance/{id}`
///
/// Cancelling an open window un-silences whatever it covered, immediately. That is as
/// consequential as scheduling it, which is why this is `Operator` and why the audit row
/// records whether it was open at the time.
pub async fn cancel(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
    _csrf: CsrfChecked,
) -> ApiResult<StatusCode> {
    caller.require(Role::Operator)?;

    let now = Utc::now();
    let before = state.store.maintenance_window(caller.scope(), id).await?;
    let was_open = before.schedule.is_open_at(now);
    state.store.cancel_maintenance(caller.scope(), id).await?;

    caller.audit().wrote(
        "maintenance.cancel",
        format!("maintenance:{id}"),
        Some(serde_json::json!({
            "reason": before.reason,
            "target": before.target.kind(),
            // The number somebody will want six months later: cancelling a window that
            // was open is what turned the alerts back on, and it should not take a
            // reconstruction of the schedule to find out that it happened.
            "was_open": was_open,
        })),
        None,
    );

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/resources/{id}/maintenance`
///
/// What is being suppressed for one resource right now, or `null`.
///
/// The question the alert engine asks internally, exposed because an operator looking at
/// a device that is not alerting needs to be able to find out why — and *"a window
/// covering a group it is in"* is not something they can work out from the device page
/// otherwise.
pub async fn for_resource(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uops_core::ResourceId>,
) -> ApiResult<Json<Option<Suppression>>> {
    caller.require(Role::Viewer)?;

    // Another tenant's resource is a 404 here, not a null — a null would confirm the id
    // is real.
    state.store.resource(caller.scope(), id).await?;

    let suppression = state
        .store
        .maintenance_for(caller.scope(), id, Utc::now())
        .await?;
    caller.audit().read("maintenance.for_resource", None);

    Ok(Json(suppression))
}
