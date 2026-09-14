//! The HTTP surface — M1.
//!
//! Thin on purpose. Everything this crate does has already been decided somewhere with
//! better tests: the repositories are in `uops-store-pg`, the query compiler in
//! `uops-query`, the resolution rules in `uops-identity`. What is here is the part that
//! cannot live anywhere else — turning a request into proof that the caller may act.
//!
//! # The one thing to read
//!
//! [`extract::Caller`]. It is the only place in the product that calls
//! `TenantScope::from_authenticated`, and therefore the point every isolation guarantee
//! below it depends on. The type system makes a query without a scope impossible; the
//! schema makes a row referencing another tenant impossible; this decides that a scope
//! may exist at all.

pub mod error;
pub mod extract;
pub mod state;

pub use error::{ApiError, ApiResult};
pub use extract::{Authenticated, Caller, SESSION_COOKIE, TENANT_HEADER};
pub use state::AppState;
