//! Alert rules and the alerts they produce — SPEC §M4.
//!
//! A rule is a `Query` plus a [`Condition`]. Both arrive on the wire in the shapes they
//! are stored in, which is what makes *"a saved search from the Log Explorer converts to
//! an alert rule with no edits"* a copy: `GET /api/v1/searches/{id}` returns a `query`,
//! and that value is posted here verbatim.
//!
//! # Roles
//!
//! Reading is `Viewer`. Everything else is `Operator`, acknowledgement included — an ack
//! decides that nobody else needs to be woken up, which is an operational act rather than
//! an annotation.
//!
//! # Two verbs on one rule
//!
//! `PUT` replaces a rule; `PATCH …/enabled` flips one boolean. The second exists because
//! silencing a rule is what somebody does at 3am, and making them round-trip the whole
//! rule to do it is how a tired person overwrites a threshold by accident.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uops_core::alert::{AlertSeverity, Condition, Phase};
use uops_core::{ResourceId, Role};
use uops_query::Query;
use uops_store_pg::NewRule;

use crate::csrf::CsrfChecked;
use crate::error::ApiResult;
use crate::extract::Caller;
use crate::state::AppState;

/// A rule, as the UI shows it.
#[derive(Debug, Serialize)]
pub struct RuleView {
    pub id: uuid::Uuid,
    pub name: String,
    pub description: String,
    /// `threshold` | `absence`, lifted out of the condition so a list can be grouped
    /// without inspecting one document per row.
    pub kind: String,
    pub query: Query,
    pub condition: Condition,
    pub severity: AlertSeverity,
    pub enabled: bool,
    pub eval_interval_seconds: i64,
    pub notify: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn rule_view(r: uops_store_pg::AlertRule) -> RuleView {
    RuleView {
        id: r.id,
        name: r.name,
        description: r.description,
        kind: r.condition.as_str().to_owned(),
        query: r.query,
        condition: r.condition,
        severity: r.severity,
        enabled: r.enabled,
        eval_interval_seconds: r.eval_interval.num_seconds(),
        notify: r.notify,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}

/// One alert, as the UI shows it.
#[derive(Debug, Serialize)]
pub struct AlertView {
    pub id: uuid::Uuid,
    pub rule_id: uuid::Uuid,
    /// The rule's name and the resource's, joined server-side. A screen showing forty
    /// alerts would otherwise make forty requests to name forty devices.
    pub rule: String,
    pub severity: AlertSeverity,
    pub resource_id: ResourceId,
    pub resource: String,
    /// Rule, resource and labels. In the API because it is in the database and in the
    /// support ticket; there is nothing to gain by hiding it.
    pub dedup_key: String,
    pub state: Phase,
    pub since: DateTime<Utc>,
    pub last_eval: DateTime<Utc>,
    pub last_value: Option<f64>,
    /// Present when somebody has taken it. An acknowledged alert is still firing and
    /// still in this list — what it is not is a second page about the same thing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acked_at: Option<DateTime<Utc>>,
}

fn alert_view(a: uops_store_pg::ActiveAlert) -> AlertView {
    AlertView {
        id: a.alert.id,
        rule_id: a.alert.rule_id,
        rule: a.rule,
        severity: a.severity,
        resource_id: a.alert.resource_id,
        resource: a.resource,
        dedup_key: a.alert.dedup_key,
        state: a.alert.phase,
        since: a.alert.since,
        last_eval: a.alert.last_eval,
        last_value: a.alert.last_value,
        acked_at: a.alert.acked_at,
    }
}

/// The same view for one alert that was just acknowledged.
///
/// `acknowledge_alert` returns the row it wrote and not the join, so the two names come
/// from the caller — which already has the rule, because it is the one being acked.
fn acked_view(
    a: uops_store_pg::AlertStateRow,
    rule: String,
    severity: AlertSeverity,
    resource: String,
) -> AlertView {
    AlertView {
        id: a.id,
        rule_id: a.rule_id,
        rule,
        severity,
        resource_id: a.resource_id,
        resource,
        dedup_key: a.dedup_key,
        state: a.phase,
        since: a.since,
        last_eval: a.last_eval,
        last_value: a.last_value,
        acked_at: a.acked_at,
    }
}

/// What a client sends to create or replace a rule.
#[derive(Debug, Deserialize)]
pub struct RuleRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The AST — posted straight from a saved search, if that is where it came from.
    pub query: Query,
    pub condition: Condition,
    pub severity: AlertSeverity,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// How often the engine asks. Bounded by migration 0014 at ten seconds to a day; a
    /// value outside that comes back as a 400 rather than being clamped, because a rule
    /// silently evaluating sixty times less often than asked is worse than a refusal.
    #[serde(default = "default_interval_seconds")]
    pub eval_interval_seconds: u32,
    #[serde(default = "no_channels")]
    pub notify: serde_json::Value,
}

const fn enabled_by_default() -> bool {
    true
}

const fn default_interval_seconds() -> u32 {
    60
}

fn no_channels() -> serde_json::Value {
    serde_json::json!([])
}

impl From<RuleRequest> for NewRule {
    fn from(body: RuleRequest) -> Self {
        Self {
            name: body.name,
            description: body.description,
            query: body.query,
            condition: body.condition,
            severity: body.severity,
            enabled: body.enabled,
            eval_interval: chrono::Duration::seconds(i64::from(body.eval_interval_seconds)),
            notify: body.notify,
        }
    }
}

/// `GET /api/v1/alerts/rules`
pub async fn list_rules(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<Vec<RuleView>>> {
    caller.require(Role::Viewer)?;

    let rules = state.store.alert_rules(caller.scope()).await?;
    caller.audit().read(
        "alerts.rules",
        Some(i64::try_from(rules.len()).unwrap_or(i64::MAX)),
    );

    Ok(Json(rules.into_iter().map(rule_view).collect()))
}

/// `GET /api/v1/alerts/rules/{id}`
pub async fn get_rule(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<Json<RuleView>> {
    caller.require(Role::Viewer)?;

    let rule = state.store.alert_rule(caller.scope(), id).await?;
    caller.audit().read("alerts.rule", None);

    Ok(Json(rule_view(rule)))
}

/// `POST /api/v1/alerts/rules`
pub async fn create_rule(
    State(state): State<AppState>,
    caller: Caller,
    _csrf: CsrfChecked,
    Json(body): Json<RuleRequest>,
) -> ApiResult<(StatusCode, Json<RuleView>)> {
    caller.require(Role::Operator)?;

    let rule = state
        .store
        .create_rule(caller.scope(), Some(caller.user_id()), &body.into())
        .await?;

    // The rule's shape, never its query. A rule's filter carries the customer's hostnames
    // the same way a search's does, and the audit table is not where a second copy of
    // them belongs.
    caller.audit().wrote(
        "alerts.rule.create",
        format!("alert_rule:{}", rule.id),
        None,
        Some(serde_json::json!({
            "name": rule.name,
            "kind": rule.condition.as_str(),
            "severity": rule.severity.as_str(),
            "enabled": rule.enabled,
        })),
    );

    Ok((StatusCode::CREATED, Json(rule_view(rule))))
}

/// `PUT /api/v1/alerts/rules/{id}`
pub async fn update_rule(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
    _csrf: CsrfChecked,
    Json(body): Json<RuleRequest>,
) -> ApiResult<Json<RuleView>> {
    caller.require(Role::Operator)?;

    let before = state.store.alert_rule(caller.scope(), id).await?;
    let rule = state
        .store
        .update_rule(caller.scope(), id, &body.into())
        .await?;

    caller.audit().wrote(
        "alerts.rule.update",
        format!("alert_rule:{id}"),
        Some(serde_json::json!({
            "name": before.name,
            "kind": before.condition.as_str(),
            "severity": before.severity.as_str(),
            "enabled": before.enabled,
        })),
        Some(serde_json::json!({
            "name": rule.name,
            "kind": rule.condition.as_str(),
            "severity": rule.severity.as_str(),
            "enabled": rule.enabled,
        })),
    );

    Ok(Json(rule_view(rule)))
}

#[derive(Debug, Deserialize)]
pub struct EnabledRequest {
    pub enabled: bool,
}

/// `PATCH /api/v1/alerts/rules/{id}/enabled`
///
/// Silencing a rule and un-silencing it are both consequential, and both leave a row: six
/// weeks later *"who turned this off, and when"* is the only question anybody asks about
/// a rule that did not fire.
pub async fn set_enabled(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
    _csrf: CsrfChecked,
    Json(body): Json<EnabledRequest>,
) -> ApiResult<Json<RuleView>> {
    caller.require(Role::Operator)?;

    let before = state.store.alert_rule(caller.scope(), id).await?;
    let rule = state
        .store
        .set_rule_enabled(caller.scope(), id, body.enabled)
        .await?;

    caller.audit().wrote(
        if body.enabled {
            "alerts.rule.enable"
        } else {
            "alerts.rule.disable"
        },
        format!("alert_rule:{id}"),
        Some(serde_json::json!({ "enabled": before.enabled })),
        Some(serde_json::json!({ "enabled": rule.enabled })),
    );

    Ok(Json(rule_view(rule)))
}

/// `DELETE /api/v1/alerts/rules/{id}`
pub async fn delete_rule(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
    _csrf: CsrfChecked,
) -> ApiResult<StatusCode> {
    caller.require(Role::Operator)?;

    let before = state.store.alert_rule(caller.scope(), id).await?;
    state.store.delete_rule(caller.scope(), id).await?;

    caller.audit().wrote(
        "alerts.rule.delete",
        format!("alert_rule:{id}"),
        Some(serde_json::json!({
            "name": before.name,
            "kind": before.condition.as_str(),
            "severity": before.severity.as_str(),
        })),
        None,
    );

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/alerts`
///
/// What is wrong in this tenant right now. `pending` is included deliberately: nobody has
/// been notified about a pending alert, and an operator who has just been paged about one
/// device wants to see the four that are one evaluation away from paging too.
pub async fn list_alerts(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<Vec<AlertView>>> {
    caller.require(Role::Viewer)?;

    let alerts = state.store.active_alerts(caller.scope()).await?;
    caller.audit().read(
        "alerts.active",
        Some(i64::try_from(alerts.len()).unwrap_or(i64::MAX)),
    );

    Ok(Json(alerts.into_iter().map(alert_view).collect()))
}

/// `POST /api/v1/alerts/{id}/ack`
///
/// Takes responsibility for an alert without changing what is true about it. The alert
/// stays in the list, still firing, with a name against it — an acknowledgement that hid
/// it would mean the next person to look at the screen concludes the problem went away.
pub async fn acknowledge(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<uuid::Uuid>,
    _csrf: CsrfChecked,
) -> ApiResult<Json<AlertView>> {
    caller.require(Role::Operator)?;

    let alert = state
        .store
        .acknowledge_alert(caller.scope(), id, caller.user_id(), Utc::now())
        .await?;

    // Two lookups the caller already caused: the rule it belongs to and the device it is
    // about, so the response is the same shape the list is and a client can drop it
    // straight back into the row it came from.
    let rule = state
        .store
        .alert_rule(caller.scope(), alert.rule_id)
        .await?;
    let resource = state
        .store
        .resource(caller.scope(), alert.resource_id)
        .await
        .map_or_else(|_| alert.resource_id.to_string(), |r| r.name);

    caller.audit().wrote(
        "alerts.ack",
        format!("alert:{id}"),
        None,
        Some(serde_json::json!({
            "dedup_key": alert.dedup_key,
            "state": alert.phase.as_str(),
        })),
    );

    Ok(Json(acked_view(alert, rule.name, rule.severity, resource)))
}
