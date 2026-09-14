//! What every handler is given.

use uops_store_pg::PgStore;

use crate::cookie::Secure;

/// Shared application state.
///
/// Cheap to clone — `PgStore` wraps a pool that is already an `Arc` — which is what
/// axum requires of state and what lets a handler hold it without ceremony.
#[derive(Clone, Debug)]
pub struct AppState {
    pub store: PgStore,
    /// Whether cookies carry `Secure`. On, except for a developer on plain HTTP
    /// against localhost, where the browser would silently discard them and the app
    /// would appear broken for a reason nothing logs.
    pub secure_cookies: Secure,
}

impl AppState {
    /// Production defaults.
    #[must_use]
    pub const fn new(store: PgStore) -> Self {
        Self {
            store,
            secure_cookies: Secure::Yes,
        }
    }

    /// Drop the `Secure` cookie attribute. For `http://localhost` only — and named so
    /// that it is visible in a diff if it ever reaches a deployment.
    #[must_use]
    pub const fn allowing_insecure_cookies(mut self) -> Self {
        self.secure_cookies = Secure::No;
        self
    }
}
