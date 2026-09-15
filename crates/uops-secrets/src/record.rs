//! What actually gets stored — SPEC §M0.4.
//!
//! Envelope encryption: every credential gets its own data encryption key (DEK), and
//! the DEK is wrapped by a key encryption key (KEK) that **never enters the database**.
//!
//! ```text
//! CredentialMaterial ──AEAD(DEK)──► ciphertext ──┐
//!                DEK ──AEAD(KEK)──► wrapped_dek ─┴──► PostgreSQL
//!                KEK ◄── env var | file (0600) | OS keyring | KMS (later)
//! ```
//!
//! Two properties fall out of this, and both are the reason for the extra indirection:
//!
//! - **KEK rotation is cheap.** Re-wrap the DEKs; never touch the ciphertext. Rotating
//!   the key protecting thousands of credentials is then a fast, low-risk operation
//!   rather than a bulk re-encryption that can fail halfway.
//! - **Compromise is bounded.** A leaked DEK exposes one credential, not the estate.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uops_core::{CredentialRef, TenantId};

use crate::aead::{NONCE_LEN, Nonce};

/// Names a KEK without revealing it. Recorded on every row so rotation can find rows
/// still wrapped by an old key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyId(pub String);

impl KeyId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A sealed credential, exactly as it is persisted. Contains no usable material.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SealedCredential {
    pub id: CredentialRef,
    pub tenant_id: TenantId,
    /// Human-facing name, e.g. "core-switches-snmpv3".
    pub name: String,
    /// Discriminant from `CredentialMaterial::kind()`.
    pub kind: String,
    /// Bumped on rotation. Collectors read the highest non-revoked version.
    pub version: u32,

    /// Which KEK wrapped `wrapped_dek`.
    pub kek_id: KeyId,
    /// The DEK, encrypted under the KEK.
    pub wrapped_dek: Vec<u8>,
    /// Nonce used to wrap the DEK.
    pub dek_nonce: [u8; NONCE_LEN],

    /// The credential material, encrypted under the DEK.
    pub ciphertext: Vec<u8>,
    /// Nonce used for the material.
    pub nonce: [u8; NONCE_LEN],

    /// Which crypto build sealed this. Makes a backend change detectable, and tells an
    /// auditor what actually produced the bytes.
    pub backend_id: String,

    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl SealedCredential {
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }

    #[must_use]
    pub fn nonce(&self) -> Nonce {
        Nonce::from_bytes(self.nonce)
    }

    #[must_use]
    pub fn dek_nonce(&self) -> Nonce {
        Nonce::from_bytes(self.dek_nonce)
    }

    /// Additional authenticated data binding this ciphertext to its identity.
    ///
    /// **This is what stops a sealed row being moved between tenants.** The AAD is
    /// authenticated but not encrypted, so if an attacker with database access copies
    /// tenant A's credential row into tenant B, decryption fails rather than quietly
    /// succeeding and handing B the use of A's device credentials.
    ///
    /// Length-prefixed rather than concatenated: plain concatenation of variable-length
    /// fields is ambiguous, and two different identities could otherwise produce the
    /// same AAD bytes.
    #[must_use]
    pub fn aad(tenant_id: TenantId, id: CredentialRef, version: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(48);
        out.extend_from_slice(b"uops-cred-v1\0");
        out.extend_from_slice(tenant_id.as_uuid().as_bytes());
        out.extend_from_slice(id.as_uuid().as_bytes());
        out.extend_from_slice(&version.to_be_bytes());
        out
    }

    /// The AAD for this row.
    #[must_use]
    pub fn own_aad(&self) -> Vec<u8> {
        Self::aad(self.tenant_id, self.id, self.version)
    }
}

/// Metadata supplied when sealing a new credential.
#[derive(Clone, Debug)]
pub struct CredentialMeta {
    pub name: String,
    /// `None` for a new credential; `Some(previous)` when rotating.
    pub supersedes: Option<CredentialRef>,
}

impl CredentialMeta {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            supersedes: None,
        }
    }
}

/// Outcome of a KEK rotation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RotationReport {
    pub rewrapped: usize,
    pub already_current: usize,
    pub failed: usize,
    /// Rows that changed between being read and being re-wrapped — an operator rotating
    /// a credential while the KEK rotation ran. Not a failure: the row now holds a
    /// wrapping newer than the one this rotation computed, and the next rotation will
    /// find it under the old KEK and re-wrap it then. See [`Rewrapped::Superseded`].
    pub superseded: usize,
}

/// What happened to one row in a KEK rotation.
///
/// # Why this is not a `bool`, and why the store has to check
///
/// A KEK rotation reads a row, unwraps its DEK, re-wraps that DEK under the new key and
/// writes the wrapping back. If the credential is *itself* rotated in between —
/// `LocalVault::put` reuses the credential's id, so a rotation replaces the row's
/// ciphertext and its DEK — then writing the wrapping back unconditionally leaves the
/// row holding a wrapping for the **old** DEK over the **new** ciphertext. Unwrapping
/// then yields a key that decrypts nothing, and the credential is permanently
/// unopenable.
///
/// It is a narrow window and it destroys data silently, which is the worst combination
/// a race can have. So the write is conditional on the wrapping still being the one that
/// was read, and this says which way it went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rewrapped {
    /// The row still held the wrapping it was read with, and now holds the new one.
    Replaced,
    /// The row changed since it was read; nothing was written.
    Superseded,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aad_is_unique_per_identity() {
        let t1 = TenantId::new();
        let t2 = TenantId::new();
        let c1 = CredentialRef::new();
        let c2 = CredentialRef::new();

        let base = SealedCredential::aad(t1, c1, 1);
        assert_ne!(base, SealedCredential::aad(t2, c1, 1), "tenant must matter");
        assert_ne!(
            base,
            SealedCredential::aad(t1, c2, 1),
            "credential must matter"
        );
        assert_ne!(
            base,
            SealedCredential::aad(t1, c1, 2),
            "version must matter"
        );
        assert_eq!(
            base,
            SealedCredential::aad(t1, c1, 1),
            "must be deterministic"
        );
    }

    #[test]
    fn aad_is_unambiguous_across_field_boundaries() {
        // Fixed-width fields with a domain-separation prefix, so no two distinct
        // identities can produce identical AAD bytes. With naive concatenation of
        // variable-length encodings that is not guaranteed.
        let t = TenantId::new();
        let c = CredentialRef::new();
        let aad = SealedCredential::aad(t, c, 1);
        assert_eq!(aad.len(), 13 + 16 + 16 + 4);
        assert!(aad.starts_with(b"uops-cred-v1\0"));
    }
}
