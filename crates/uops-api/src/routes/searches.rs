//! Saved searches — SPEC §M3.
//!
//! Five verbs over one object: a stored [`Query`] AST with a name. The AST goes out on
//! the wire exactly as `POST /api/v1/query` takes it in, so opening a saved search is
//! posting back what this returned, with nothing in between that could reinterpret it.
//!
//! # Roles
//!
//! Reading is `Viewer`; saving, editing and deleting are `Operator`. A saved search is
//! shared by everyone in the tenant — it is the team's question, not a personal
//! bookmark — so a viewer who could overwrite one could change what a colleague sees
//! during an incident without leaving anything on screen to say so. They can still run
//! any query they like; what they cannot do is put a name on it for everybody else.
//!
//! # No personal searches, yet
//!
//! Every search in a tenant is visible to that tenant. A private-to-me search is a real
//! feature and an easy column to add, and it is deliberately not here: it would need a
//! sharing model, and a half-built sharing model is how a search somebody believes is
//! private ends up listed for the whole team.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uops_core::{Role, SavedSearchId};
use uops_query::Query;
use uops_store_pg::NewSearch;

use crate::csrf::CsrfChecked;
use crate::error::ApiResult;
use crate::extract::Caller;
use crate::state::AppState;

/// One saved search, as the UI shows it.
#[derive(Debug, Serialize)]
pub struct SearchView {
    pub id: SavedSearchId,
    pub name: String,
    pub description: String,
    /// The signal, lifted out of the AST so a list can be grouped without parsing one
    /// query per row — the same reason the column exists in migration 0013.
    pub signal: String,
    /// The AST itself.
    ///
    /// Including the window it was saved over, which the client replaces with whatever
    /// range the operator is looking at. A saved search that reopened showing the same
    /// forty rows forever would be a screenshot, not a question.
    pub query: Query,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn view(s: uops_store_pg::SavedSearch) -> SearchView {
    SearchView {
        id: s.id,
        name: s.name,
        description: s.description,
        signal: s.query.signal.as_str().to_owned(),
        query: s.query,
        created_at: s.created_at,
        updated_at: s.updated_at,
    }
}

/// `GET /api/v1/searches`
///
/// The tenant's searches, most recently changed first.
pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<Vec<SearchView>>> {
    caller.require(Role::Viewer)?;

    let searches = state.store.saved_searches(caller.scope()).await?;
    caller.audit().read(
        "searches.list",
        Some(i64::try_from(searches.len()).unwrap_or(i64::MAX)),
    );

    Ok(Json(searches.into_iter().map(view).collect()))
}

/// `GET /api/v1/searches/{id}`
pub async fn get(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<SavedSearchId>,
) -> ApiResult<Json<SearchView>> {
    caller.require(Role::Viewer)?;

    let search = state.store.saved_search(caller.scope(), id).await?;
    caller.audit().read("searches.get", None);

    Ok(Json(view(search)))
}

/// What a client sends to save or replace one.
#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub query: Query,
}

impl From<SearchRequest> for NewSearch {
    fn from(body: SearchRequest) -> Self {
        Self {
            name: body.name,
            description: body.description,
            query: body.query,
        }
    }
}

/// `POST /api/v1/searches`
///
/// The query is compiled before the row is written, so a search that cannot be answered
/// is refused here with the compiler's own wording rather than stored and found to be
/// broken by whoever opens it next.
pub async fn save(
    State(state): State<AppState>,
    caller: Caller,
    _csrf: CsrfChecked,
    Json(body): Json<SearchRequest>,
) -> ApiResult<(StatusCode, Json<SearchView>)> {
    caller.require(Role::Operator)?;

    let search = state
        .store
        .save_search(caller.scope(), Some(caller.user_id()), &body.into())
        .await?;

    // The name and the signal, never the filter. A saved search's terms are the
    // customer's hostnames and error strings, and an audit row carrying them would put a
    // second copy of the data in the audit table — the same reason `read_query` records
    // a fingerprint instead of the query.
    caller.audit().wrote(
        "searches.save",
        format!("search:{}", search.id),
        None,
        Some(serde_json::json!({
            "name": search.name,
            "signal": search.query.signal.as_str(),
        })),
    );

    Ok((StatusCode::CREATED, Json(view(search))))
}

/// `PUT /api/v1/searches/{id}`
///
/// A whole replacement, not a patch: the three things a search is are the three things
/// this takes. A partial update of an AST is a merge nobody can review.
pub async fn update(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<SavedSearchId>,
    _csrf: CsrfChecked,
    Json(body): Json<SearchRequest>,
) -> ApiResult<Json<SearchView>> {
    caller.require(Role::Operator)?;

    let before = state.store.saved_search(caller.scope(), id).await?;
    let search = state
        .store
        .update_search(caller.scope(), id, &body.into())
        .await?;

    caller.audit().wrote(
        "searches.update",
        format!("search:{id}"),
        Some(serde_json::json!({
            "name": before.name,
            "signal": before.query.signal.as_str(),
        })),
        Some(serde_json::json!({
            "name": search.name,
            "signal": search.query.signal.as_str(),
        })),
    );

    Ok(Json(view(search)))
}

/// `DELETE /api/v1/searches/{id}`
pub async fn delete(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<SavedSearchId>,
    _csrf: CsrfChecked,
) -> ApiResult<StatusCode> {
    caller.require(Role::Operator)?;

    // Read first, so the audit row says what was deleted rather than only that something
    // was. A row naming an id nobody can look up any more is not an audit trail.
    let before = state.store.saved_search(caller.scope(), id).await?;
    state.store.delete_search(caller.scope(), id).await?;

    caller.audit().wrote(
        "searches.delete",
        format!("search:{id}"),
        Some(serde_json::json!({
            "name": before.name,
            "signal": before.query.signal.as_str(),
        })),
        None,
    );

    Ok(StatusCode::NO_CONTENT)
}
