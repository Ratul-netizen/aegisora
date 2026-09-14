//! The route table.

pub mod auth;

use axum::Router;
use axum::routing::{get, post};

use crate::state::AppState;

/// Everything under `/api/v1`.
///
/// Assembled in one place so the surface is readable as a list rather than discovered by
/// grepping for `#[route]`. SPEC §M1 has the full intended surface; this is what exists.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/auth/login", post(auth::login))
        .route("/api/v1/auth/logout", post(auth::logout))
        .route("/api/v1/me", get(auth::me))
        .with_state(state)
}
