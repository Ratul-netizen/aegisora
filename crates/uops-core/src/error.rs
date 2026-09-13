//! Shared error type.
//!
//! SPEC conventions: `thiserror` internally, `anyhow` only in binaries, and the API
//! maps these onto RFC 7807 problem+json.

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("not found: {kind} {id}")]
    NotFound { kind: &'static str, id: String },

    #[error("invalid input: {0}")]
    Invalid(String),

    /// A cross-tenant access was attempted. This is a bug or an attack, never routine,
    /// and is always logged at error level with the full scope.
    #[error("tenant isolation violation")]
    TenantMismatch,

    #[error("permission denied: {0}")]
    Forbidden(&'static str),

    /// Two identifiers of the same kind mapped to different resources. Surfaced to the
    /// identity review queue rather than failing ingestion.
    #[error("identity conflict on {kind}: {value}")]
    IdentityConflict { kind: &'static str, value: String },

    #[error("storage: {0}")]
    Storage(String),

    #[error("serialization: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl Error {
    /// HTTP status for the API layer.
    // `NotFound` and `TenantMismatch` returning the same status is the point, not an
    // oversight: a distinct code for `TenantMismatch` would confirm to a caller that
    // the resource exists in some other tenant, which for an MSP deployment leaks one
    // customer's inventory to another. Keep the arms separate and identical.
    #[allow(clippy::match_same_arms)]
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        match self {
            Self::NotFound { .. } => 404,
            Self::Invalid(_) | Self::Serialization(_) => 400,
            // Deliberately 404, not 403: confirming that a resource exists in another
            // tenant is itself an information leak.
            Self::TenantMismatch => 404,
            Self::Forbidden(_) => 403,
            Self::IdentityConflict { .. } => 409,
            Self::Storage(_) => 500,
        }
    }

    /// RFC 7807 `type` slug.
    #[must_use]
    pub const fn problem_type(&self) -> &'static str {
        match self {
            Self::NotFound { .. } | Self::TenantMismatch => "not-found",
            Self::Invalid(_) => "invalid-input",
            Self::Forbidden(_) => "forbidden",
            Self::IdentityConflict { .. } => "identity-conflict",
            Self::Storage(_) => "internal",
            Self::Serialization(_) => "serialization",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_mismatch_is_indistinguishable_from_not_found() {
        // A 403 would confirm the resource exists in some other tenant. For an MSP
        // deployment that is a genuine information leak between customers.
        assert_eq!(Error::TenantMismatch.status_code(), 404);
        assert_eq!(Error::TenantMismatch.problem_type(), "not-found");
        assert_eq!(
            Error::NotFound {
                kind: "resource",
                id: "x".into()
            }
            .problem_type(),
            Error::TenantMismatch.problem_type(),
            "the two must be indistinguishable to a client"
        );
    }

    #[test]
    fn identity_conflict_is_a_conflict_not_an_error() {
        // Not a 500: a duplicate hostname across two devices is a routine operational
        // fact, and it belongs in the identity review queue rather than in an error log.
        assert_eq!(
            Error::IdentityConflict {
                kind: "hostname",
                value: "rtr-01".into(),
            }
            .status_code(),
            409
        );
    }
}
