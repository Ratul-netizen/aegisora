//! What every handler is given.

use uops_store_pg::PgStore;

/// Shared application state.
///
/// Cheap to clone — `PgStore` wraps a pool that is already an `Arc` — which is what
/// axum requires of state and what lets a handler hold it without ceremony.
#[derive(Clone, Debug)]
pub struct AppState {
    pub store: PgStore,
}

impl AppState {
    #[must_use]
    pub const fn new(store: PgStore) -> Self {
        Self { store }
    }
}
