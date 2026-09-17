//! Notification channels, and the record of what they were sent — SPEC §M4.
//!
//! # Roles
//!
//! Reading is `Viewer`; writing is `Operator`, the same as an alert rule. A channel is
//! where a page goes, so changing one changes who gets woken up — which is the same
//! consequence a rule has, and is why it is the same role rather than a stricter one an
//! on-call engineer would have to escalate to at 3am.
//!
//! # Why the delivery log is a route at all
//!
//! *"Why did nobody get paged?"* is asked at the worst possible moment, and the answer is
//! usually one of three things: no channel was named, the channel refused, or the rate
//! limit did. All three are rows in `notification_sent`, and none of them is visible from
//! the alert list — so the list exists, refusals included.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uops_core::Role;
use uops_notify::{Smtp, Webhook};
use uops_store_pg::NewChannel;

use crate::csrf::CsrfChecked;
use crate::error::{ApiError, ApiResult};
use crate::extract::Caller;
use crate::state::AppState;

/// A channel, as the UI shows it.
#[derive(Debug, Serialize)]
pub struct ChannelView {
    pub id: uuid::Uuid,
    pub name: String,
    pub kind: String,
    /// Never secret — see migration 0015. A webhook that needs a token carries it in a
    /// header here, which is why this is readable by anyone who can read the channel and
    /// why anything stronger belongs in the vault.
    pub config: serde_json::Value,
    pub enabled: bool,
    pub max_per_minute: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn view(c: uops_store_pg::Channel) -> ChannelView {
    ChannelView {
        id: c.id,
        name: c.name,
        kind: c.kind,
        config: c.config,
        enabled: c.enabled,
        max_per_minute: c.max_per_minute,
        created_at: c.created_at,
        updated_at: c.updated_at,
    }
}

/// One attempt, delivered or refused.
#[derive(Debug, Serialize)]
pub struct SentView {
    pub id: uuid::Uuid,
    pub channel_id: uuid::Uuid,
    pub rule_id: uuid::Uuid,
    pub dedup_key: String,
    pub phase: String,
    /// `sent` | `failed` | `rate_limited` | `over_budget`.
    pub outcome: String,
    pub detail: String,
    pub sent_at: DateTime<Utc>,
}

/// What a client sends to create or replace a channel.
#[derive(Debug, Deserialize)]
pub struct ChannelRequest {
    pub name: String,
    pub kind: String,
    pub config: serde_json::Value,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default = "default_rate")]
    pub max_per_minute: i32,
}

const fn enabled_by_default() -> bool {
    true
}

const fn default_rate() -> i32 {
    12
}

/// Refuse a channel that could never deliver.
///
/// The transport's own parser, run while somebody is still looking at the form. A webhook
/// with no url, or an `https://` one this server cannot reach without the TLS it
/// deliberately does not carry, is a channel that accepts alerts and delivers none — and
/// the symptom arrives at 4am rather than here.
fn validate(request: &ChannelRequest) -> Result<(), ApiError> {
    match request.kind.as_str() {
        "webhook" => Webhook::from_config(&request.config)
            .map(|_| ())
            .map_err(ApiError::BadRequest),
        // A smarthost on the deployment's own network. Authenticated submission is not
        // supported and will not be — `AUTH PLAIN` over an unencrypted connection sends a
        // password in clear, and this workspace carries no TLS by decision. See
        // `uops_notify::smtp`.
        "email" => Smtp::from_config(&request.config)
            .map(|_| ())
            .map_err(ApiError::BadRequest),
        other => Err(ApiError::BadRequest(format!(
            "unknown channel kind {other}: this build has webhook and email"
        ))),
    }
}

impl From<ChannelRequest> for NewChannel {
    fn from(body: ChannelRequest) -> Self {
        Self {
            name: body.name,
            kind: body.kind,
            config: body.config,
            enabled: body.enabled,
            max_per_minute: body.max_per_minute,
        }
    }
}

/// `GET /api/v1/channels`
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<Vec<ChannelView>>> {
    caller.require(Role::Viewer)?;

    let channels = state.store.channels(caller.scope()).await?;
    caller.audit().read(
        "channels.list",
        Some(i64::try_from(channels.len()).unwrap_or(i64::MAX)),
    );

    Ok(Json(channels.into_iter().map(view).collect()))
}

/// `GET /api/v1/channels/{id}`
pub async fn get(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<Json<ChannelView>> {
    caller.require(Role::Viewer)?;

    let channel = state.store.channel(caller.scope(), id).await?;
    caller.audit().read("channels.get", None);

    Ok(Json(view(channel)))
}

/// `POST /api/v1/channels`
pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    _csrf: CsrfChecked,
    Json(body): Json<ChannelRequest>,
) -> ApiResult<(StatusCode, Json<ChannelView>)> {
    caller.require(Role::Operator)?;
    validate(&body)?;

    let channel = state
        .store
        .create_channel(caller.scope(), Some(caller.user_id()), &body.into())
        .await?;

    caller.audit().wrote(
        "channels.create",
        format!("channel:{}", channel.id),
        None,
        // The name, the kind and the rate — never the config, which carries the endpoint
        // and whatever header somebody put a token in.
        Some(serde_json::json!({
            "name": channel.name,
            "kind": channel.kind,
            "max_per_minute": channel.max_per_minute,
            "enabled": channel.enabled,
        })),
    );

    Ok((StatusCode::CREATED, Json(view(channel))))
}

/// `PUT /api/v1/channels/{id}`
pub async fn update(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
    _csrf: CsrfChecked,
    Json(body): Json<ChannelRequest>,
) -> ApiResult<Json<ChannelView>> {
    caller.require(Role::Operator)?;
    validate(&body)?;

    let before = state.store.channel(caller.scope(), id).await?;
    let channel = state
        .store
        .update_channel(caller.scope(), id, &body.into())
        .await?;

    caller.audit().wrote(
        "channels.update",
        format!("channel:{id}"),
        Some(serde_json::json!({
            "name": before.name,
            "kind": before.kind,
            "max_per_minute": before.max_per_minute,
            "enabled": before.enabled,
        })),
        Some(serde_json::json!({
            "name": channel.name,
            "kind": channel.kind,
            "max_per_minute": channel.max_per_minute,
            "enabled": channel.enabled,
        })),
    );

    Ok(Json(view(channel)))
}

/// `DELETE /api/v1/channels/{id}`
///
/// Takes the record of what it sent with it. A rule still naming this channel will say so
/// the next time it fires — see `uops_notify::Notifier`, which reports a channel that has
/// gone rather than silently notifying nobody.
pub async fn delete(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
    _csrf: CsrfChecked,
) -> ApiResult<StatusCode> {
    caller.require(Role::Operator)?;

    let before = state.store.channel(caller.scope(), id).await?;
    state.store.delete_channel(caller.scope(), id).await?;

    caller.audit().wrote(
        "channels.delete",
        format!("channel:{id}"),
        Some(serde_json::json!({ "name": before.name, "kind": before.kind })),
        None,
    );

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/notifications`
///
/// What was attempted, newest first, refusals included.
pub async fn sent(State(state): State<AppState>, caller: Caller) -> ApiResult<Json<Vec<SentView>>> {
    caller.require(Role::Viewer)?;

    let records = state.store.notifications(caller.scope(), 200).await?;
    caller.audit().read(
        "notifications.list",
        Some(i64::try_from(records.len()).unwrap_or(i64::MAX)),
    );

    Ok(Json(
        records
            .into_iter()
            .map(|r| SentView {
                id: r.id,
                channel_id: r.channel_id,
                rule_id: r.rule_id,
                dedup_key: r.dedup_key,
                phase: r.phase,
                outcome: r.outcome.as_str().to_owned(),
                detail: r.detail,
                sent_at: r.sent_at,
            })
            .collect(),
    ))
}
