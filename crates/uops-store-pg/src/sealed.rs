//! Sealed credentials in `PostgreSQL`.
//!
//! The `credential` table has existed since migration 0005 and `SealedStore` since M0.4;
//! what did not exist until now is an implementation joining them, so every credential
//! in the system lived in `MemorySealedStore` and did not survive a restart. The poller
//! is the first thing that needs one to still be there tomorrow.
//!
//! # This store never sees plaintext
//!
//! It moves sealed rows. The wrapping and unwrapping are `LocalVault`'s, the KEK is
//! `KekRing`'s, and neither is reachable from here — this file could be read aloud in a
//! meeting. That is the same division `PgStore::create_user` has with password hashing,
//! and for the same reason: a repository that could see credential material is a
//! repository that could log it.
//!
//! # The trait is synchronous and `sqlx` is not
//!
//! `SealedStore` has no `async`, which is right for its other implementations and
//! awkward for this one. The options were to make the trait async — infecting every
//! caller, including `LocalVault::get`, which is on a poll's hot path — or to bridge
//! here. Bridging here, with `block_in_place`, so exactly one file pays for it and the
//! cost is visible in the place that chose it.
//!
//! That requires a multi-threaded runtime. Every binary in this workspace uses one; a
//! current-thread runtime would panic, loudly, at the first credential read rather than
//! deadlocking, which is the failure mode to prefer.

use uops_core::{CredentialRef, TenantId};
use uops_secrets::record::{KeyId, SealedCredential};
use uops_secrets::{Error as SecretError, Result as SecretResult, SealedStore};

use crate::store::PgStore;

/// `SealedStore` over the `credential` table.
///
/// Cheap to clone — it holds a `PgStore`, which is a pool behind an `Arc`.
#[derive(Clone, Debug)]
pub struct PgSealedStore {
    store: PgStore,
}

impl PgSealedStore {
    #[must_use]
    pub const fn new(store: PgStore) -> Self {
        Self { store }
    }
}

/// Run an async query from a synchronous trait method.
///
/// See the module docs. `block_in_place` moves the blocking work off the async worker so
/// the runtime keeps making progress; `Handle::current` panics outside a runtime, which
/// is a clearer failure than a hang.
fn blocking<F: Future>(future: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

/// A nonce from the database, which is `bytea` and therefore any length.
///
/// A row whose nonce is the wrong size cannot be decrypted and was not written by this
/// code. Refusing it by name beats a panic on a slice conversion, and beats silently
/// padding it into something that decrypts to nothing.
fn nonce(bytes: &[u8], what: &'static str, id: CredentialRef) -> SecretResult<[u8; 12]> {
    <[u8; 12]>::try_from(bytes)
        .map_err(|_| {
            SecretError::Corrupt(match what {
                "dek" => "stored dek nonce is not 12 bytes",
                _ => "stored nonce is not 12 bytes",
            })
        })
        .inspect_err(|_| {
            // The id is not in the error because Corrupt takes a &'static str; it is worth
            // having somewhere, and the caller knows which credential it asked for.
            let _ = id;
        })
}

/// A `credential` row, in the column order every query below selects.
///
/// A named tuple rather than thirteen positional parameters: the columns are fixed by
/// the table and passing them one at a time was both unreadable and, as clippy pointed
/// out, thirteen chances to transpose two of them.
type CredentialRow = (
    uuid::Uuid,
    uuid::Uuid,
    String,
    String,
    i32,
    String,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    String,
    chrono::DateTime<chrono::Utc>,
    Option<chrono::DateTime<chrono::Utc>>,
);

fn row_to_sealed(row: CredentialRow) -> SecretResult<SealedCredential> {
    let id = CredentialRef::from_uuid(row.0);
    Ok(SealedCredential {
        id,
        tenant_id: TenantId::from_uuid(row.1),
        name: row.2,
        kind: row.3,
        version: u32::try_from(row.4).unwrap_or(1),
        kek_id: KeyId(row.5),
        wrapped_dek: row.6,
        dek_nonce: nonce(&row.7, "dek", id)?,
        ciphertext: row.8,
        nonce: nonce(&row.9, "material", id)?,
        backend_id: row.10,
        created_at: row.11,
        revoked_at: row.12,
    })
}

impl SealedStore for PgSealedStore {
    fn insert(&self, row: SealedCredential) -> SecretResult<()> {
        blocking(async {
            // Upsert on id, because that is what rotation does: LocalVault::put reuses
            // the credential's id and bumps the version, so the new row replaces the
            // old one. MemorySealedStore does the same (a map keyed by id), and the two
            // implementations agreeing is what lets the vault's tests mean anything
            // about this one.
            //
            // NOTE — migration 0005 says "rotation writes a new row rather than
            // overwriting one", which this does not do and neither does the memory
            // store. See STATUS: the rollback-by-revoking property that comment
            // describes is not currently delivered by either, and reconciling them is a
            // decision about the primary key rather than a bug in this file.
            sqlx::query(
                "INSERT INTO credential
                   (id, tenant_id, name, kind, version,
                    kek_id, wrapped_dek, dek_nonce, ciphertext, nonce, backend_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                 ON CONFLICT (id) DO UPDATE SET
                    name        = EXCLUDED.name,
                    kind        = EXCLUDED.kind,
                    version     = EXCLUDED.version,
                    kek_id      = EXCLUDED.kek_id,
                    wrapped_dek = EXCLUDED.wrapped_dek,
                    dek_nonce   = EXCLUDED.dek_nonce,
                    ciphertext  = EXCLUDED.ciphertext,
                    nonce       = EXCLUDED.nonce,
                    backend_id  = EXCLUDED.backend_id",
            )
            .bind(row.id.into_uuid())
            .bind(row.tenant_id.into_uuid())
            .bind(&row.name)
            .bind(&row.kind)
            .bind(i32::try_from(row.version).unwrap_or(i32::MAX))
            .bind(&row.kek_id.0)
            .bind(&row.wrapped_dek)
            .bind(row.dek_nonce.as_slice())
            .bind(&row.ciphertext)
            .bind(row.nonce.as_slice())
            .bind(&row.backend_id)
            .execute(self.store.pool())
            .await
            .map(|_| ())
            .map_err(|e| SecretError::Storage(e.to_string()))
        })
    }

    fn get(&self, tenant: TenantId, id: CredentialRef) -> SecretResult<SealedCredential> {
        blocking(async {
            // Revoked rows are still readable by id. A revoked credential is how you
            // decrypt something sealed before the rotation that revoked it, and a
            // rotation that could not read what it was replacing would be useless.
            let row = sqlx::query_as::<_, CredentialRow>(
                "SELECT id, tenant_id, name, kind, version,
                        kek_id, wrapped_dek, dek_nonce, ciphertext, nonce, backend_id,
                        created_at, revoked_at
                   FROM credential
                  WHERE tenant_id = $1 AND id = $2",
            )
            .bind(tenant.into_uuid())
            .bind(id.into_uuid())
            .fetch_optional(self.store.pool())
            .await
            .map_err(|e| SecretError::Storage(e.to_string()))?
            .ok_or(SecretError::NotFound)?;

            row_to_sealed(row)
        })
    }

    fn latest_by_name(&self, tenant: TenantId, name: &str) -> SecretResult<SealedCredential> {
        blocking(async {
            // Highest live version. This is how a collector resolves a credential, so a
            // rotation takes effect without reconfiguring anything — and revoking the
            // new row rolls it back to the previous one with no restore.
            let row = sqlx::query_as::<_, CredentialRow>(
                "SELECT id, tenant_id, name, kind, version,
                        kek_id, wrapped_dek, dek_nonce, ciphertext, nonce, backend_id,
                        created_at, revoked_at
                   FROM credential
                  WHERE tenant_id = $1 AND name = $2 AND revoked_at IS NULL
                  ORDER BY version DESC
                  LIMIT 1",
            )
            .bind(tenant.into_uuid())
            .bind(name)
            .fetch_optional(self.store.pool())
            .await
            .map_err(|e| SecretError::Storage(e.to_string()))?
            .ok_or(SecretError::NotFound)?;

            row_to_sealed(row)
        })
    }

    fn list_all(&self) -> SecretResult<Vec<SealedCredential>> {
        blocking(async {
            // tenant-exempt: deliberately every row in every tenant. The one caller is
            // KEK rotation, which has to re-wrap everything or leave behind a row that
            // only the retired key can open. It reads no plaintext — a sealed row is
            // opaque to this store — so crossing the boundary here exposes nothing.
            //
            // Marked explicitly rather than relying on the scanner: this statement
            // happens to mention tenant_id in its ORDER BY, so it would have passed for
            // the wrong reason.
            let rows = sqlx::query_as::<_, CredentialRow>(
                "SELECT id, tenant_id, name, kind, version,
                        kek_id, wrapped_dek, dek_nonce, ciphertext, nonce, backend_id,
                        created_at, revoked_at
                   FROM credential
                  ORDER BY tenant_id, name, version",
            )
            .fetch_all(self.store.pool())
            .await
            .map_err(|e| SecretError::Storage(e.to_string()))?;

            rows.into_iter().map(row_to_sealed).collect()
        })
    }

    fn replace_wrapping(
        &self,
        id: CredentialRef,
        kek_id: KeyId,
        wrapped_dek: Vec<u8>,
        dek_nonce: [u8; 12],
    ) -> SecretResult<()> {
        blocking(async {
            // Only the wrapping changes. The ciphertext is untouched, which is the point
            // of envelope encryption: rotating the KEK re-wraps a 32-byte key per row
            // rather than re-encrypting every credential.
            //
            // tenant-exempt: addressed by credential id, which is globally unique, and
            // reached only from KEK rotation — a platform operation across every tenant
            // by definition. A tenant predicate here would make rotation per-tenant,
            // which is not what a key ring is.
            let affected = sqlx::query(
                "UPDATE credential
                    SET kek_id = $2, wrapped_dek = $3, dek_nonce = $4
                  WHERE id = $1",
            )
            .bind(id.into_uuid())
            .bind(&kek_id.0)
            .bind(&wrapped_dek)
            .bind(dek_nonce.as_slice())
            .execute(self.store.pool())
            .await
            .map_err(|e| SecretError::Storage(e.to_string()))?
            .rows_affected();

            if affected == 0 {
                return Err(SecretError::NotFound);
            }
            Ok(())
        })
    }

    fn revoke(&self, tenant: TenantId, id: CredentialRef) -> SecretResult<()> {
        blocking(async {
            // Stamped, not deleted. Telemetry and audit rows reference a credential by
            // id, and a row that vanished would leave them pointing at nothing.
            sqlx::query(
                "UPDATE credential SET revoked_at = now()
                  WHERE tenant_id = $1 AND id = $2 AND revoked_at IS NULL",
            )
            .bind(tenant.into_uuid())
            .bind(id.into_uuid())
            .execute(self.store.pool())
            .await
            .map(|_| ())
            .map_err(|e| SecretError::Storage(e.to_string()))
        })
    }
}
