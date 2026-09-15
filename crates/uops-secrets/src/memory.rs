//! In-memory [`SealedStore`], for tests and for bring-up before PostgreSQL exists.
//!
//! It holds only sealed rows — no keys — so losing it loses ciphertext, never material.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::Utc;
use uops_core::{CredentialRef, TenantId};

use crate::aead::NONCE_LEN;
use crate::error::{Error, Result};
use crate::record::{KeyId, Rewrapped, SealedCredential};
use crate::vault::SealedStore;

#[derive(Debug, Default)]
pub struct MemorySealedStore {
    rows: Mutex<HashMap<CredentialRef, SealedCredential>>,
}

impl MemorySealedStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn with<R>(&self, f: impl FnOnce(&mut HashMap<CredentialRef, SealedCredential>) -> R) -> R {
        match self.rows.lock() {
            Ok(mut g) => f(&mut g),
            Err(e) => f(&mut e.into_inner()),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.with(|r| r.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Test hook: read a row without decrypting, to assert on what is persisted.
    #[must_use]
    pub fn peek(&self, id: CredentialRef) -> Option<SealedCredential> {
        self.with(|r| r.get(&id).cloned())
    }

    /// Test-only: rewrite a row's tenant, simulating an attacker with database write
    /// access moving a credential between customers. The AAD should then refuse to
    /// decrypt — see the transplant test in `lib.rs`.
    #[cfg(test)]
    pub fn tamper_tenant(&self, id: CredentialRef, tenant: TenantId) {
        self.with(|r| {
            if let Some(row) = r.get_mut(&id) {
                row.tenant_id = tenant;
            }
        });
    }
}

impl SealedStore for MemorySealedStore {
    fn insert(&self, row: SealedCredential) -> Result<()> {
        self.with(|r| r.insert(row.id, row));
        Ok(())
    }

    fn get(&self, tenant: TenantId, id: CredentialRef) -> Result<SealedCredential> {
        self.with(|r| {
            r.get(&id)
                // Filtered by tenant here as well as in the vault: a store that can
                // return another tenant's row is one refactor away from a leak.
                .filter(|row| row.tenant_id == tenant)
                .cloned()
                .ok_or(Error::NotFound)
        })
    }

    fn latest_by_name(&self, tenant: TenantId, name: &str) -> Result<SealedCredential> {
        self.with(|r| {
            r.values()
                .filter(|row| row.tenant_id == tenant && row.name == name && row.is_active())
                .max_by_key(|row| row.version)
                .cloned()
                .ok_or(Error::NotFound)
        })
    }

    fn list_all(&self) -> Result<Vec<SealedCredential>> {
        Ok(self.with(|r| r.values().cloned().collect()))
    }

    fn replace_wrapping(
        &self,
        id: CredentialRef,
        kek_id: KeyId,
        wrapped_dek: Vec<u8>,
        dek_nonce: [u8; NONCE_LEN],
        expected_wrapped_dek: &[u8],
    ) -> Result<Rewrapped> {
        self.with(|r| {
            let row = r.get_mut(&id).ok_or(Error::NotFound)?;
            // The condition, and the only reason this method takes six arguments. See
            // the trait: writing unconditionally would leave a row wrapped for a DEK its
            // ciphertext no longer uses. A map behind a lock can race exactly as a
            // database can, so this implementation checks too — one that did not would
            // make the memory store the place the bug still lives, which is also the
            // store every other crate's tests use.
            if row.wrapped_dek != expected_wrapped_dek {
                return Ok(Rewrapped::Superseded);
            }
            row.kek_id = kek_id;
            row.wrapped_dek = wrapped_dek;
            row.dek_nonce = dek_nonce;
            Ok(Rewrapped::Replaced)
        })
    }

    fn revoke(&self, tenant: TenantId, id: CredentialRef) -> Result<()> {
        self.with(|r| {
            let row = r
                .get_mut(&id)
                .filter(|x| x.tenant_id == tenant)
                .ok_or(Error::NotFound)?;
            row.revoked_at = Some(Utc::now());
            Ok(())
        })
    }
}
