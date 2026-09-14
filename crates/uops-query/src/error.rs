//! Query errors.
//!
//! These are all *caller* errors — a malformed AST, a field that does not exist on the
//! signal being queried, an aggregation the chosen rollup cannot serve. Nothing here
//! reaches the database, because compilation is pure: a query that cannot be compiled
//! never becomes SQL.

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The resolved resource set belongs to a different tenant than the scope.
    ///
    /// Reachable only by constructing a `ResolvedResources` under one scope and
    /// compiling under another. That is a programming error rather than user input,
    /// and it is the reason resolution carries its tenant with it.
    #[error("tenant isolation violation")]
    TenantMismatch,

    #[error("invalid query: {0}")]
    Invalid(String),

    #[error("field `{field}` is not available on {signal} queries")]
    FieldNotAvailable { field: String, signal: &'static str },

    /// The time span forces a rollup table, and the rollup cannot answer what was
    /// asked. Raw data for that span no longer exists, so this is not something the
    /// compiler can silently work around.
    #[error("{what} cannot be served from {table}: {why}")]
    RollupCannotServe {
        what: String,
        table: &'static str,
        why: &'static str,
    },

    #[error("{0} is not implemented until a later milestone")]
    Unsupported(&'static str),
}

impl From<Error> for uops_core::Error {
    fn from(e: Error) -> Self {
        match e {
            // Stays a 404, indistinguishable from NotFound — see uops_core::Error.
            Error::TenantMismatch => Self::TenantMismatch,
            other => Self::Invalid(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_mismatch_keeps_its_meaning_when_it_crosses_crates() {
        let mapped: uops_core::Error = Error::TenantMismatch.into();
        assert_eq!(mapped.status_code(), 404);
        assert_eq!(mapped.problem_type(), "not-found");
    }

    #[test]
    fn caller_errors_become_400_not_500() {
        let mapped: uops_core::Error = Error::Invalid("no time range".into()).into();
        assert_eq!(mapped.status_code(), 400);
    }
}
