//! The route table.
//!
//! Assembled in one place so the surface reads as a list rather than being discovered by
//! grepping for attributes. SPEC §M1 has the full intended surface; this is what exists.
//!
//! `POST /api/v1/query` takes the AST from SPEC §M0.5 directly — the same type the UI
//! builds, saved alerts are instances of, and the M6 text language will parse onto.
//! There is one path to telemetry, and this is it.

pub mod auth;
pub mod query;
pub mod resources;

use axum::routing::{delete, get, patch, post};
use axum::{Router, middleware};

use crate::audit;
use crate::state::AppState;

/// Everything under `/api/v1`.
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
        .route("/api/v1/query", post(query::run))
        // Wrapped around everything rather than a chosen list of routes: a request that
        // never establishes a scope leaves nothing to record and is skipped, so this
        // cannot be forgotten when a route is added. See crate::audit.
        .layer(middleware::from_fn_with_state(state.clone(), audit::layer))
        .with_state(state)
}
