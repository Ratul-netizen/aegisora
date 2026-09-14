//! Telemetry storage errors.

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("clickhouse is unreachable at {url}: {detail}")]
    Unreachable { url: String, detail: String },

    #[error("clickhouse returned {status}: {message}")]
    Server { status: u16, message: String },

    #[error("malformed request: {0}")]
    Request(String),

    #[error("unexpected response from clickhouse: {0}")]
    Protocol(String),

    /// The query could not be compiled. Not a storage failure — the caller asked for
    /// something the AST refuses, and the message already explains it.
    #[error(transparent)]
    Query(#[from] uops_query::Error),
}

impl From<Error> for uops_core::Error {
    fn from(e: Error) -> Self {
        match e {
            // A query the compiler rejected is the caller's problem, and the API should
            // say so with a 400 rather than blaming the database with a 500.
            Error::Query(q) => q.into(),
            other => Self::Storage(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rejected_query_is_the_callers_fault_and_a_dead_server_is_not() {
        let mapped: uops_core::Error =
            Error::Query(uops_query::Error::Invalid("no time range".into())).into();
        assert_eq!(mapped.status_code(), 400);

        let mapped: uops_core::Error = Error::Unreachable {
            url: "http://ch:8123".into(),
            detail: "connection refused".into(),
        }
        .into();
        assert_eq!(mapped.status_code(), 500);
    }

    #[test]
    fn a_tenant_mismatch_survives_the_journey_as_a_404() {
        // It passes through two From impls on its way to the API, and must still be
        // indistinguishable from "not found" at the end of them.
        let mapped: uops_core::Error = Error::Query(uops_query::Error::TenantMismatch).into();
        assert_eq!(mapped.status_code(), 404);
        assert_eq!(mapped.problem_type(), "not-found");
    }
}
