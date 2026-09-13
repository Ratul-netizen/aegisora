//! Errors for the secrets subsystem.
//!
//! Note what is *absent*: no error variant carries plaintext, a key, or a nonce, and
//! the crypto failures are deliberately coarse. Distinguishing "wrong key" from "wrong
//! AAD" from "tampered ciphertext" hands an attacker with database access an oracle.

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("encryption failed")]
    Seal,

    /// Covers wrong key, wrong AAD, tampering and corruption. Kept as one variant on
    /// purpose — see the module docs.
    #[error("decryption failed")]
    Open,

    #[error("credential not found")]
    NotFound,

    #[error("credential has been revoked")]
    Revoked,

    /// The sealed row belongs to a different tenant. Checked explicitly rather than
    /// left to the AAD, so the access-control rule is visible in review.
    #[error("tenant isolation violation")]
    TenantMismatch,

    #[error("unknown key encryption key: {0}")]
    UnknownKek(String),

    #[error("key encryption key unavailable: {0}")]
    KekUnavailable(String),

    /// A KEK file readable by group or others is rejected outright. A key that other
    /// local accounts can read is not a root of trust, and a warning is not a control.
    #[error("key encryption key at {path} has unsafe permissions {mode:o}; require 0600")]
    KekPermissions { path: String, mode: u32 },

    #[error("malformed key encryption key: {0}")]
    KekMalformed(String),

    #[error("sealed payload is corrupt: {0}")]
    Corrupt(&'static str),

    #[error("randomness unavailable: {0}")]
    Random(String),

    #[error("storage: {0}")]
    Storage(String),
}

impl From<Error> for uops_core::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::NotFound => Self::NotFound {
                kind: "credential",
                id: String::new(),
            },
            // Also 404 upstream: confirming that a credential exists in another tenant
            // is itself a leak, so this must be indistinguishable from "not found".
            Error::TenantMismatch => Self::TenantMismatch,
            Error::Revoked => Self::Invalid("credential revoked".into()),
            other => Self::Storage(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crypto_failures_do_not_distinguish_causes() {
        // One message for every decryption failure. A caller must not be able to tell
        // a wrong key from a tampered ciphertext.
        assert_eq!(Error::Open.to_string(), "decryption failed");
    }

    #[test]
    fn no_error_message_carries_material() {
        // Messages are rendered into logs and API responses, so none may echo a
        // credential value. KEK identifiers and file paths are fine; material is not.
        for e in [
            Error::Seal,
            Error::Open,
            Error::NotFound,
            Error::Revoked,
            Error::TenantMismatch,
            Error::Corrupt("truncated field"),
        ] {
            let msg = e.to_string();
            assert!(!msg.contains("password"), "{msg}");
            assert!(!msg.contains("key="), "{msg}");
        }
    }

    #[test]
    fn tenant_mismatch_surfaces_as_not_found_upstream() {
        let core: uops_core::Error = Error::TenantMismatch.into();
        assert_eq!(core.status_code(), 404);
    }
}
