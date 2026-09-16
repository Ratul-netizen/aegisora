//! `LocalVault` — the v0.1 `SecretStore` implementation.
//!
//! Seals and opens credentials using envelope encryption (see [`crate::record`]), and
//! writes an access-log entry for **every** decrypt, successful or not (SPEC §M0.8:
//! defence and law-enforcement buyers audit who *saw* what, not only who changed it).
//!
//! Persistence sits behind [`SealedStore`] so the crypto and the audit discipline are
//! testable without a database — the PostgreSQL implementation arrives with the
//! migrations and changes nothing here.

use chrono::Utc;
use uops_core::{CredentialMaterial, CredentialRef, Secret, TenantId};

use crate::aead::{AeadProvider, Key, Nonce};
use crate::audit::{AccessContext, AccessLog, AccessOutcome};
use crate::error::{Error, Result};
use crate::kek::KekRing;
use crate::record::{CredentialMeta, KeyId, Rewrapped, RotationReport, SealedCredential};
use crate::serialize::{deserialize_material, serialize_material};

/// Persistence for sealed rows. Holds no key material and can decrypt nothing.
///
/// Implemented for `Arc<S>` as well, so a caller can retain a handle for inspection or
/// health reporting while the vault owns its copy. That is also what lets the tests
/// observe what was persisted without giving [`LocalVault`] test-only backdoors.
pub trait SealedStore: Send + Sync {
    fn insert(&self, row: SealedCredential) -> Result<()>;
    fn get(&self, tenant: TenantId, id: CredentialRef) -> Result<SealedCredential>;
    /// Highest non-revoked version for a credential name within a tenant.
    fn latest_by_name(&self, tenant: TenantId, name: &str) -> Result<SealedCredential>;
    fn list_all(&self) -> Result<Vec<SealedCredential>>;
    /// Replace a row's wrapping, but only if it still holds `expected_wrapped_dek`.
    ///
    /// Conditional, and that is the whole point — see [`crate::record::Rewrapped`]. The
    /// caller computed the new wrapping from a DEK it unwrapped out of a row it read; if
    /// that row has since been replaced, the new wrapping is for a DEK the row no longer
    /// contains and writing it would destroy the credential.
    ///
    /// `expected_wrapped_dek` is sufficient on its own: it is a ciphertext sealed under a
    /// freshly generated nonce, so any change to the DEK *or* to the KEK produces
    /// different bytes.
    fn replace_wrapping(
        &self,
        id: CredentialRef,
        kek_id: KeyId,
        wrapped_dek: Vec<u8>,
        dek_nonce: [u8; crate::aead::NONCE_LEN],
        expected_wrapped_dek: &[u8],
    ) -> Result<Rewrapped>;
    fn revoke(&self, tenant: TenantId, id: CredentialRef) -> Result<()>;
}

impl<T: SealedStore> SealedStore for std::sync::Arc<T> {
    fn insert(&self, row: SealedCredential) -> Result<()> {
        (**self).insert(row)
    }
    fn get(&self, tenant: TenantId, id: CredentialRef) -> Result<SealedCredential> {
        (**self).get(tenant, id)
    }
    fn latest_by_name(&self, tenant: TenantId, name: &str) -> Result<SealedCredential> {
        (**self).latest_by_name(tenant, name)
    }
    fn list_all(&self) -> Result<Vec<SealedCredential>> {
        (**self).list_all()
    }
    fn replace_wrapping(
        &self,
        id: CredentialRef,
        kek_id: KeyId,
        wrapped_dek: Vec<u8>,
        dek_nonce: [u8; crate::aead::NONCE_LEN],
        expected_wrapped_dek: &[u8],
    ) -> Result<Rewrapped> {
        (**self).replace_wrapping(id, kek_id, wrapped_dek, dek_nonce, expected_wrapped_dek)
    }
    fn revoke(&self, tenant: TenantId, id: CredentialRef) -> Result<()> {
        (**self).revoke(tenant, id)
    }
}

/// What a credential is, with none of what it holds.
///
/// Everything here is safe to show an administrator and to put in an audit row: a name
/// somebody chose, the kind of thing it is, which version is current and whether it has
/// been retired. There is deliberately no field that could carry material, so a future
/// edit cannot add one by accident.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub id: CredentialRef,
    pub name: String,
    /// `snmp_community`, `snmp_v3`. Not the secret: a UI lists credentials by what they
    /// are without opening any of them.
    pub kind: String,
    pub version: u32,
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Summary {
    fn of(row: &SealedCredential) -> Self {
        Self {
            id: row.id,
            name: row.name.clone(),
            kind: row.kind.clone(),
            version: row.version,
            revoked_at: row.revoked_at,
        }
    }
}

/// Seals credentials so that neither the database nor the logs ever hold usable
/// material.
pub struct LocalVault<A: AeadProvider, S: SealedStore, L: AccessLog> {
    aead: A,
    store: S,
    audit: L,
    keks: KekRing,
}

impl<A: AeadProvider, S: SealedStore, L: AccessLog> std::fmt::Debug for LocalVault<A, S, L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalVault")
            .field("backend", &self.aead.backend_id())
            .field("active_kek", self.keks.active_id())
            .finish_non_exhaustive()
    }
}

impl<A: AeadProvider, S: SealedStore, L: AccessLog> LocalVault<A, S, L> {
    pub const fn new(aead: A, store: S, audit: L, keks: KekRing) -> Self {
        Self {
            aead,
            store,
            audit,
            keks,
        }
    }

    /// Seal new credential material.
    ///
    /// Takes `Secret<CredentialMaterial>` **by value on purpose**: the caller
    /// surrenders the plaintext at this boundary and it is zeroized when this returns.
    /// Borrowing instead would leave a live copy in the caller's frame for an
    /// unbounded time, which is exactly what the type exists to prevent. Clippy reads
    /// this as a needless move because the body only reads it; that analysis does not
    /// model the drop being the point.
    #[allow(clippy::needless_pass_by_value)]
    pub fn put(
        &self,
        tenant: TenantId,
        material: Secret<CredentialMaterial>,
        meta: &CredentialMeta,
    ) -> Result<CredentialRef> {
        let id = meta.supersedes.unwrap_or_default();
        let version = match meta.supersedes {
            // Rotation: a new version of an existing credential, so collectors can be
            // switched over without a window in which neither works.
            Some(prev) => self.store.get(tenant, prev)?.version.saturating_add(1),
            None => 1,
        };

        // A fresh DEK per credential, so a leaked DEK exposes one credential rather
        // than every credential sharing a key.
        let dek = Key::generate()?;
        let nonce = Nonce::generate()?;
        let dek_nonce = Nonce::generate()?;

        let aad = SealedCredential::aad(tenant, id, version);
        let plaintext = serialize_material(material.expose());
        let ciphertext = self
            .aead
            .seal(dek.expose(), &nonce, &aad, plaintext.expose())?;

        // Wrap the DEK under the active KEK. The KEK's own AAD binds the wrapping to
        // the same identity, so a wrapped DEK cannot be transplanted either.
        let wrapped_dek = self.aead.seal(
            self.keks.active()?,
            &dek_nonce,
            &aad,
            dek.expose().as_bytes(),
        )?;

        self.store.insert(SealedCredential {
            id,
            tenant_id: tenant,
            name: meta.name.clone(),
            kind: material.expose().kind().to_owned(),
            version,
            kek_id: self.keks.active_id().clone(),
            wrapped_dek,
            dek_nonce: *dek_nonce.as_bytes(),
            ciphertext,
            nonce: *nonce.as_bytes(),
            backend_id: self.aead.backend_id().to_owned(),
            created_at: Utc::now(),
            revoked_at: None,
        })?;

        Ok(id)
    }

    /// Open a sealed credential.
    ///
    /// Every call writes an access-log entry, including failures — a burst of failed
    /// credential reads is exactly the signal an auditor needs and exactly what a
    /// success-only log would hide.
    pub fn get(
        &self,
        tenant: TenantId,
        id: CredentialRef,
        ctx: &AccessContext,
    ) -> Result<Secret<CredentialMaterial>> {
        let result = self.open_inner(tenant, id);
        self.audit.record(
            tenant,
            id,
            ctx,
            if result.is_ok() {
                AccessOutcome::Granted
            } else {
                AccessOutcome::Denied
            },
        );
        result
    }

    fn open_inner(
        &self,
        tenant: TenantId,
        id: CredentialRef,
    ) -> Result<Secret<CredentialMaterial>> {
        let row = self.store.get(tenant, id)?;

        // Defence in depth. The AAD check below would also fail on a tenant mismatch,
        // but relying on a crypto failure to enforce an access-control rule makes the
        // rule invisible in review and the failure indistinguishable from corruption.
        if row.tenant_id != tenant {
            return Err(Error::TenantMismatch);
        }
        if !row.is_active() {
            return Err(Error::Revoked);
        }

        let aad = row.own_aad();
        let kek = self.keks.get(&row.kek_id)?;
        let dek_bytes = self
            .aead
            .open(kek, &row.dek_nonce(), &aad, &row.wrapped_dek)?;

        let dek_array: [u8; crate::aead::KEY_LEN] = dek_bytes
            .expose()
            .as_slice()
            .try_into()
            .map_err(|_| Error::Open)?;
        let dek = Secret::new(Key::from_bytes(dek_array));

        let plaintext = self
            .aead
            .open(dek.expose(), &row.nonce(), &aad, &row.ciphertext)?;

        deserialize_material(plaintext.expose())
    }

    /// Re-wrap every DEK under the active KEK.
    ///
    /// **Ciphertext is never touched.** That is the whole point of the envelope: the
    /// expensive, risky operation (re-encrypting every credential) is replaced by a
    /// cheap one (re-encrypting a 32-byte key per credential). A rotation that fails
    /// halfway leaves every row still openable, because the retired KEK stays in the
    /// ring.
    pub fn rotate_kek(&self) -> Result<RotationReport> {
        let active_id = self.keks.active_id().clone();
        let active = self.keks.active()?;
        let mut report = RotationReport::default();

        for row in self.store.list_all()? {
            if row.kek_id == active_id {
                report.already_current += 1;
                continue;
            }

            let Ok(old_kek) = self.keks.get(&row.kek_id) else {
                report.failed += 1;
                continue;
            };

            let aad = row.own_aad();
            let Ok(dek) = self
                .aead
                .open(old_kek, &row.dek_nonce(), &aad, &row.wrapped_dek)
            else {
                report.failed += 1;
                continue;
            };

            let new_nonce = Nonce::generate()?;
            let Ok(rewrapped) = self.aead.seal(active, &new_nonce, &aad, dek.expose()) else {
                report.failed += 1;
                continue;
            };

            // `row.wrapped_dek` is what this re-wrap was computed from. Passing it makes
            // the write conditional on the row not having moved underneath — a credential
            // rotation between the read above and this write would otherwise leave the
            // row wrapped for a DEK its ciphertext no longer uses.
            match self.store.replace_wrapping(
                row.id,
                active_id.clone(),
                rewrapped,
                *new_nonce.as_bytes(),
                &row.wrapped_dek,
            ) {
                Ok(Rewrapped::Replaced) => report.rewrapped += 1,
                Ok(Rewrapped::Superseded) => report.superseded += 1,
                Err(_) => report.failed += 1,
            }
        }

        Ok(report)
    }

    /// Fetch by name, taking the highest live version. This is how collectors resolve
    /// a credential, so a rotation takes effect without reconfiguring anything.
    pub fn get_latest(
        &self,
        tenant: TenantId,
        name: &str,
        ctx: &AccessContext,
    ) -> Result<Secret<CredentialMaterial>> {
        let row = self.store.latest_by_name(tenant, name)?;
        self.get(tenant, row.id, ctx)
    }

    /// What a credential is, without any of what it holds.
    ///
    /// For a UI that lists credentials by name: the material is not here, the wrapped
    /// DEK is not here, and the ciphertext is not here. `SealedCredential` carries all
    /// three — harmlessly, since they are encrypted — and handing it to a caller that
    /// wants a name would be handing it three things it has no use for.
    ///
    /// # Errors
    ///
    /// Storage failures, or a credential in another tenant — which is `NotFound`.
    pub fn describe(&self, tenant: TenantId, id: CredentialRef) -> Result<Summary> {
        Ok(Summary::of(&self.store.get(tenant, id)?))
    }

    /// Every credential in a tenant, as summaries.
    ///
    /// # Errors
    ///
    /// Storage failures.
    pub fn list(&self, tenant: TenantId) -> Result<Vec<Summary>> {
        // `list_all` is deliberately cross-tenant — it exists for KEK rotation — so the
        // filter is here. A vault method that returned another tenant's credentials
        // because its caller forgot a predicate is the shape this whole crate is written
        // against.
        let mut out: Vec<Summary> = self
            .store
            .list_all()?
            .iter()
            .filter(|row| row.tenant_id == tenant)
            .map(Summary::of)
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn revoke(&self, tenant: TenantId, id: CredentialRef) -> Result<()> {
        self.store.revoke(tenant, id)
    }

    /// Introduce a new active KEK, retaining the previous one for unwrapping.
    ///
    /// Call this, then [`LocalVault::rotate_kek`]. The two steps are separate because
    /// the retired key must stay available for the whole of the rotation — dropping it
    /// at promotion time would make every not-yet-rewrapped credential unreadable.
    pub fn promote_kek(&mut self, id: KeyId, key: Secret<Key>) {
        self.keks.promote(id, key);
    }

    #[must_use]
    pub fn backend_id(&self) -> &'static str {
        self.aead.backend_id()
    }
}
