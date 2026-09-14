//! `POST /api/v1/query` — SPEC §M1.
//!
//! The endpoint the whole architecture was arranged around, and the shortest handler in
//! the crate. Everything it does was decided somewhere with better tests:
//!
//! 1. [`Caller`] proved the session, the tenant and the role — and registered the
//!    request for auditing in the same step.
//! 2. `uops_query::resolve` expands the selector against `PostgreSQL`, collapsing
//!    aliases so telemetry written before a merge still resolves.
//! 3. `uops_query::compile` writes the SQL **and the tenant predicate**, from the scope.
//!    Nothing above or below it can produce a statement without one.
//! 4. `ChStore` executes it and hands back the rows, the warnings, and how much the
//!    server actually read.
//!
//! # Why a read is a POST
//!
//! The `Query` AST does not fit in a URL, and encoding it into one would produce
//! something no human can read in a log and no proxy will keep intact. It is still a
//! read: it changes nothing, and it requires only `Viewer`.
//!
//! # And why it still requires a CSRF token
//!
//! A read expressed as a POST is still a POST. Exempting it would create the one
//! endpoint whose protection is a special case somebody has to remember — and the
//! exemption list is exactly where that kind of mistake lives. The app sends the header
//! on every mutating request already; sending it here costs nothing.

use axum::Json;
use axum::extract::State;
use uops_core::Role;
use uops_query::{Query, resolve};
use uops_store_ch::{ResultSet, TelemetryStore, fingerprint};
use uops_store_pg::PgCatalog;

use crate::csrf::CsrfChecked;
use crate::error::{ApiError, ApiResult};
use crate::extract::Caller;
use crate::state::AppState;

/// `POST /api/v1/query`
pub async fn run(
    State(state): State<AppState>,
    caller: Caller,
    _csrf: CsrfChecked,
    Json(query): Json<Query>,
) -> ApiResult<Json<ResultSet>> {
    caller.require(Role::Viewer)?;

    // Alias expansion happens here, against the control plane, before any telemetry SQL
    // exists. A resource merged away last week still resolves, which is what makes a
    // merge O(1) instead of a re-ingest.
    let resources = resolve(
        &query.resources,
        caller.scope(),
        &PgCatalog::new(state.store.clone()),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    let result = state
        .telemetry
        .query(&query, caller.scope(), &resources)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    // The shape of the query and how much came back — never the parameters. An auditor
    // needs to recognise "this account ran tenant-wide log searches all night", not to
    // be handed a transcript of what was searched for.
    caller.audit().read_query(
        fingerprint(&query, result.table),
        Some(result.len().try_into().unwrap_or(i64::MAX)),
    );

    Ok(Json(result))
}
