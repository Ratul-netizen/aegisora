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
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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

/// How much history the first poll of a tail shows.
///
/// A tail that opens empty looks broken on a quiet estate, and a tail that opens with an
/// hour of history is a search somebody did not ask for. A minute is enough for the view
/// to have something in it and short enough that nobody mistakes it for the search.
const FIRST_POLL: chrono::Duration = chrono::Duration::minutes(1);

/// What a client sends to follow a search.
#[derive(Debug, Deserialize)]
pub struct TailRequest {
    /// The search being followed — the same AST `POST /api/v1/query` takes, unchanged.
    /// Aggregation and paging on it are dropped rather than refused: following the
    /// histogram means following the rows its bars count.
    pub query: Query,
    /// The watermark from the previous poll, or absent on the first.
    ///
    /// Always the server's own `next_since`, never a client clock. A browser whose clock
    /// is three minutes fast would otherwise ask for a window that has not happened yet
    /// and see nothing until it caught up — and would have no way to find out why.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
}

/// One poll of a tail.
#[derive(Debug, Serialize)]
pub struct TailPage {
    /// The rows that arrived in this window, newest first.
    #[serde(flatten)]
    pub result: ResultSet,
    /// The watermark to send back on the next poll.
    ///
    /// The end of the window this poll covered, which is the *server's* now. Sent back
    /// verbatim, so the sequence of windows is contiguous by construction and no client
    /// ever has to reason about clock skew between itself and the server.
    pub next_since: DateTime<Utc>,
    /// Whether this window was delivered whole.
    ///
    /// False when the poll filled its row limit, which means ingest is arriving faster
    /// than the tail can show it and some of this window was left behind. A tail that
    /// silently drops rows while claiming to be live is worse than one that says it is
    /// behind, and this is the difference between the two.
    pub complete: bool,
    /// Whether the watermark had to be moved forward to reach this window.
    ///
    /// True when a client comes back after longer than the tail can look back — a laptop
    /// that was asleep, a tab left open overnight. The rows in between are still in
    /// storage and a search will find them; the tail cannot, and says so rather than
    /// resuming as though nothing were missing.
    pub skipped: bool,
}

/// `POST /api/v1/query/tail`
///
/// # Why this is a poll
///
/// SPEC sketches the tail as a stream. `MergeTree` has no change feed, so anything
/// calling itself one would be this request with a connection held open in front of it —
/// and a held-open connection is the thing that on-premise reverse proxies close at sixty
/// seconds, which turns "live tail" into an intermittent one that nobody can debug.
///
/// What makes the result a stream is the *window*, not the transport: it is half-open on
/// `ingested_at`, `[since, now)`, so consecutive polls partition the rows — every row
/// delivered once, none twice, including the ones whose ingest lagged their own
/// timestamp. See [`uops_query::follow`].
pub async fn tail(
    State(state): State<AppState>,
    caller: Caller,
    _csrf: CsrfChecked,
    Json(body): Json<TailRequest>,
) -> ApiResult<Json<TailPage>> {
    caller.require(Role::Viewer)?;

    let now = Utc::now();
    let first = body.since.is_none();
    let asked = body.since.unwrap_or(now - FIRST_POLL);
    // A watermark older than the lookback cannot be honoured: the bracket `follow` puts
    // on `observed_at` is what keeps a poll from scanning every partition in the
    // retention period, and widening it silently for a client that went to sleep would
    // make one poll cost what a month-long search costs.
    let floor = now - uops_query::TAIL_LOOKBACK;
    let since = asked.max(floor);
    let skipped = since > asked;

    let followed = uops_query::follow(&body.query, since, now);

    let resources = resolve(
        &followed.resources,
        caller.scope(),
        &PgCatalog::new(state.store.clone()),
    )
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    let result = state
        .telemetry
        .tail(&followed, caller.scope(), &resources)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    // One audit row when a tail is switched on, and none for the polls that follow. The
    // reasoning is the health endpoint's: a log recording something every two seconds is
    // one nobody reads, and what an auditor needs to see is that this account started
    // tailing the tenant's logs — not 1 800 rows saying it still was.
    if first {
        caller.audit().read_query(
            format!("tail:{}", fingerprint(&followed, result.table)),
            Some(result.len().try_into().unwrap_or(i64::MAX)),
        );
    }

    let ceiling = followed.limit.min(uops_query::TAIL_LIMIT) as usize;
    Ok(Json(TailPage {
        complete: result.len() < ceiling,
        next_since: now,
        skipped,
        result,
    }))
}
