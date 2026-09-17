//! The route table.
//!
//! Assembled in one place so the surface reads as a list rather than being discovered by
//! grepping for attributes. SPEC §M1 has the full intended surface; this is what exists.
//!
//! `POST /api/v1/query` takes the AST from SPEC §M0.5 directly — the same type the UI
//! builds, saved alerts are instances of, and the M6 text language will parse onto.
//! There is one path to telemetry, and this is it.

pub mod alerts;
pub mod auth;
pub mod channels;
pub mod credentials;
pub mod dashboards;
pub mod groups;
pub mod health;
pub mod maintenance;
pub mod query;
pub mod resources;
pub mod searches;
pub mod sites;

use axum::routing::{any, delete, get, patch, post, put};
use axum::{Router, middleware};

use crate::audit;
use crate::error::ApiError;
use crate::state::AppState;

/// Everything under `/api/v1`.
///
/// Long, and deliberately one function: this module exists so the API surface reads as a
/// list rather than being discovered by grepping for attributes, and splitting it into
/// `inventory_routes()`, `telemetry_routes()` and `alerting_routes()` would trade that
/// for a lint. The day it stops being readable is the day to split it, and forty routes
/// is not that day.
#[allow(clippy::too_many_lines)]
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/auth/login", post(auth::login))
        .route("/api/v1/auth/logout", post(auth::logout))
        .route("/api/v1/me", get(auth::me))
        .route(
            "/api/v1/resources",
            get(resources::list).post(resources::create),
        )
        .route("/api/v1/resources/{id}", get(resources::get))
        .route("/api/v1/resources/{id}", delete(resources::decommission))
        .route(
            "/api/v1/resources/{id}/status",
            patch(resources::set_status),
        )
        .route(
            "/api/v1/credentials",
            get(credentials::list).post(credentials::create),
        )
        .route("/api/v1/credentials/{id}", delete(credentials::revoke))
        .route(
            "/api/v1/resources/{id}/credential",
            put(credentials::assign),
        )
        .route(
            "/api/v1/resources/{id}/identifiers",
            get(credentials::identifiers).put(credentials::set_identifiers),
        )
        .route("/api/v1/resources/{id}/groups", get(groups::of_resource))
        // Tags, not attributes. PUT replaces the whole map, which is how a tag is
        // removed; see the route's own docs.
        .route("/api/v1/resources/{id}/tags", put(groups::set_tags))
        .route("/api/v1/groups", get(groups::list).post(groups::create))
        .route(
            "/api/v1/groups/{id}",
            put(groups::rename).delete(groups::delete),
        )
        .route(
            "/api/v1/groups/{id}/members",
            post(groups::add_members).delete(groups::remove_members),
        )
        .route(
            "/api/v1/resources/{id}/maintenance",
            get(maintenance::for_resource),
        )
        .route(
            "/api/v1/maintenance",
            get(maintenance::list).post(maintenance::schedule),
        )
        .route(
            "/api/v1/maintenance/{id}",
            get(maintenance::get).delete(maintenance::cancel),
        )
        // An alert rule is a saved Query AST plus a condition — which is why "alert
        // from a saved search" needs no conversion: the client posts back the `query`
        // the search route returned. Reading is Viewer; everything else, acknowledgement
        // included, is Operator.
        .route(
            "/api/v1/alerts/rules",
            get(alerts::list_rules).post(alerts::create_rule),
        )
        .route(
            "/api/v1/alerts/rules/{id}",
            get(alerts::get_rule)
                .put(alerts::update_rule)
                .delete(alerts::delete_rule),
        )
        .route(
            "/api/v1/alerts/rules/{id}/enabled",
            patch(alerts::set_enabled),
        )
        .route("/api/v1/alerts", get(alerts::list_alerts))
        .route("/api/v1/alerts/{id}/ack", post(alerts::acknowledge))
        // A saved search is a stored Query AST — the same object the route below takes,
        // and the same one an M4 alert rule will be an instance of. Reading is Viewer;
        // saving is Operator, because a saved search is the team's question rather than
        // a personal bookmark.
        .route("/api/v1/searches", get(searches::list).post(searches::save))
        .route(
            "/api/v1/searches/{id}",
            get(searches::get)
                .put(searches::update)
                .delete(searches::delete),
        )
        // A dashboard is a document: a name and an ordered list of panels, each a Query
        // AST and a picture to draw it as. Adding or moving a panel is a PUT of the whole
        // thing — see the module docs for why there is no panel-level route.
        .route(
            "/api/v1/dashboards",
            get(dashboards::list).post(dashboards::create),
        )
        .route(
            "/api/v1/dashboards/{id}",
            get(dashboards::get)
                .put(dashboards::update)
                .delete(dashboards::delete),
        )
        // Where a page goes. Reading is Viewer; writing is Operator, the same as an
        // alert rule, because changing a channel changes who gets woken up.
        .route(
            "/api/v1/channels",
            get(channels::list).post(channels::create),
        )
        .route(
            "/api/v1/channels/{id}",
            get(channels::get)
                .put(channels::update)
                .delete(channels::delete),
        )
        // "Why did nobody get paged" — the refusals are in here too, which is the whole
        // reason it exists.
        .route("/api/v1/notifications", get(channels::sent))
        .route("/api/v1/sites", get(sites::list))
        .route("/api/v1/sites/{id}/location", put(sites::place))
        .route("/api/v1/query", post(query::run))
        // The tail is its own route rather than a flag on the one above, because it is
        // its own physical query — time-ordered, so the `p_by_time` projection serves it
        // — and because its response carries a watermark that a search has no use for.
        .route("/api/v1/query/tail", post(query::tail))
        // Deliberately above the audit layer as well as outside authentication: an
        // orchestrator polling every five seconds would otherwise write an audit row
        // every five seconds, and an audit log that is mostly health checks is one
        // nobody reads. It establishes no scope, so the layer skips it anyway — this
        // route is simply where that stops being an accident.
        .route("/api/v1/health", get(health::health))
        // Anything else under /api is a 404 in problem+json, like every other error
        // here. It exists because uops-server may serve the web build as a fallback for
        // unmatched paths, and without this a mistyped API path would answer 200 with
        // an HTML page — which a client parses as JSON, fails on, and reports as
        // something other than "that endpoint does not exist".
        //
        // Static segments win over a wildcard in axum's router, so this never shadows a
        // real route.
        .route("/api/{*rest}", any(no_such_endpoint))
        // Wrapped around everything rather than a chosen list of routes: a request that
        // never establishes a scope leaves nothing to record and is skipped, so this
        // cannot be forgotten when a route is added. See crate::audit.
        .layer(middleware::from_fn_with_state(state.clone(), audit::layer))
        .with_state(state)
}

/// Every unmatched path under `/api`.
async fn no_such_endpoint() -> ApiError {
    ApiError::NotFound
}
