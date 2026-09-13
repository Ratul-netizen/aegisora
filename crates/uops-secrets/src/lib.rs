//! Credential sealing — SPEC §M0.4.
//!
//! This platform holds `SNMPv3` auth/priv keys, SSH private keys and API tokens for a
//! customer's entire network. A leak here is a full network compromise, not a data
//! breach, which is why the secrets subsystem is M0 work rather than an enterprise
//! feature.
//!
//! # Shape
//!
//! ```text
//! CredentialMaterial ──AEAD(DEK)──► ciphertext ──┐
//!                DEK ──AEAD(KEK)──► wrapped_dek ─┴──► SealedStore (PostgreSQL)
//!                KEK ◄── file 0600 | env | KMS/Vault      never in the database
//! ```
//!
//! | property | how |
//! |---|---|
//! | Material never leaves as plaintext | [`uops_core::Secret`] is not `Display`, `Serialize` or `Clone` |
//! | A row cannot be moved between tenants | AAD binds ciphertext to `tenant ‖ credential ‖ version` |
//! | KEK rotation is cheap | Re-wrap 32-byte DEKs; ciphertext is never touched |
//! | One leaked DEK exposes one credential | A fresh DEK per credential |
//! | Every read is auditable | [`AccessLog`] records grants *and* denials |
//! | Crypto is swappable per jurisdiction | [`AeadProvider`], selected at build time |
//!
//! # Example
//!
//! ```
//! use uops_core::{CredentialMaterial, Secret, TenantId, scope::Actor};
//! use uops_secrets::{
//!     AccessContext, CredentialMeta, KekRing, LocalVault, MemoryAccessLog,
//!     MemorySealedStore, RustCryptoAead,
//! };
//!
//! let vault = LocalVault::new(
//!     RustCryptoAead,
//!     MemorySealedStore::new(),
//!     MemoryAccessLog::new(),
//!     KekRing::ephemeral_for_tests().unwrap(),
//! );
//!
//! let tenant = TenantId::new();
//! let id = vault.put(
//!     tenant,
//!     Secret::new(CredentialMaterial::SnmpCommunity("public".into())),
//!     &CredentialMeta::new("core-switches"),
//! ).unwrap();
//!
//! let ctx = AccessContext::new(Actor::Collector, "snmp-poll");
//! let opened = vault.get(tenant, id, &ctx).unwrap();
//! assert_eq!(opened.expose().kind(), "snmp_community");
//! ```

pub mod aead;
pub mod audit;
pub mod error;
pub mod kek;
pub mod memory;
pub mod record;
pub mod serialize;
pub mod vault;

pub use aead::{AeadProvider, KEY_LEN, Key, NONCE_LEN, Nonce, default_provider};
pub use audit::{AccessContext, AccessLog, AccessOutcome, AccessRecord, MemoryAccessLog};
pub use error::{Error, Result};
pub use kek::KekRing;
pub use memory::MemorySealedStore;
pub use record::{CredentialMeta, KeyId, RotationReport, SealedCredential};
pub use vault::{LocalVault, SealedStore};

#[cfg(feature = "crypto-rustcrypto")]
pub use aead::RustCryptoAead;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use uops_core::{CredentialMaterial, Secret, TenantId, scope::Actor};

    use super::*;

    type TestVault = LocalVault<RustCryptoAead, Arc<MemorySealedStore>, Arc<MemoryAccessLog>>;

    /// The vault plus handles onto its store and audit log, so tests can inspect what
    /// was persisted without `LocalVault` needing test-only backdoors.
    fn vault() -> (TestVault, Arc<MemorySealedStore>, Arc<MemoryAccessLog>) {
        let store = Arc::new(MemorySealedStore::new());
        let audit = Arc::new(MemoryAccessLog::new());
        let v = LocalVault::new(
            RustCryptoAead,
            Arc::clone(&store),
            Arc::clone(&audit),
            KekRing::ephemeral_for_tests().unwrap(),
        );
        (v, store, audit)
    }

    fn snmpv3() -> Secret<CredentialMaterial> {
        Secret::new(CredentialMaterial::SnmpV3 {
            username: "netops".into(),
            auth_key: "auth-secret-material".into(),
            priv_key: "priv-secret-material".into(),
        })
    }

    fn ctx() -> AccessContext {
        AccessContext::new(Actor::Collector, "snmp-poll")
    }

    #[test]
    fn seals_and_opens() {
        let (v, _, _) = vault();
        let t = TenantId::new();
        let id = v.put(t, snmpv3(), &CredentialMeta::new("core")).unwrap();

        let opened = v.get(t, id, &ctx()).unwrap();
        match opened.expose() {
            CredentialMaterial::SnmpV3 {
                username,
                auth_key,
                priv_key,
            } => {
                assert_eq!(username, "netops");
                assert_eq!(auth_key, "auth-secret-material");
                assert_eq!(priv_key, "priv-secret-material");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn nothing_recognisable_is_persisted() {
        // The property the whole module exists for: if an attacker dumps the database,
        // there must be no usable material in it.
        let (v, store, _) = vault();
        let t = TenantId::new();
        let id = v.put(t, snmpv3(), &CredentialMeta::new("core")).unwrap();

        let row = store.peek(id).expect("row must exist");
        let blob = [row.ciphertext.clone(), row.wrapped_dek.clone()].concat();
        let hay = String::from_utf8_lossy(&blob);
        for needle in ["netops", "auth-secret-material", "priv-secret-material"] {
            assert!(!hay.contains(needle), "leaked {needle} into storage");
        }
        // The kind is stored in clear on purpose: the UI lists credentials by type,
        // and it reveals nothing usable.
        assert_eq!(row.kind, "snmpv3");
        assert_eq!(row.backend_id, "rustcrypto-aes256gcm");
    }

    #[test]
    fn another_tenant_cannot_open_the_row() {
        let (v, _, _) = vault();
        let a = TenantId::new();
        let b = TenantId::new();
        let id = v.put(a, snmpv3(), &CredentialMeta::new("core")).unwrap();

        let err = v.get(b, id, &ctx()).unwrap_err();
        assert!(
            matches!(err, Error::NotFound | Error::TenantMismatch),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn aad_stops_a_row_transplanted_between_tenants() {
        // Stronger than the check above. Simulate an attacker with database WRITE
        // access rewriting tenant_id on the stored row, defeating our own `if`. The
        // AAD no longer matches, so the crypto itself refuses — isolation does not
        // rest on application logic alone.
        let (v, store, _) = vault();
        let a = TenantId::new();
        let b = TenantId::new();
        let id = v.put(a, snmpv3(), &CredentialMeta::new("core")).unwrap();

        store.tamper_tenant(id, b);

        let err = v.get(b, id, &ctx()).unwrap_err();
        assert!(
            matches!(err, Error::Open),
            "AAD must reject it, got {err:?}"
        );
    }

    #[test]
    fn every_access_is_logged_including_failures() {
        let (v, _, audit) = vault();
        let t = TenantId::new();
        let id = v.put(t, snmpv3(), &CredentialMeta::new("core")).unwrap();

        let _ = v.get(t, id, &ctx());
        let _ = v.get(TenantId::new(), id, &ctx()); // denied

        let recs = audit.records();
        assert_eq!(
            recs.len(),
            2,
            "both the grant and the denial must be recorded"
        );
        assert_eq!(recs[0].outcome, AccessOutcome::Granted);
        assert_eq!(recs[1].outcome, AccessOutcome::Denied);
        assert_eq!(recs[0].purpose, "snmp-poll");
        assert_eq!(recs[0].actor, "collector");
    }

    #[test]
    fn kek_rotation_rewraps_without_touching_ciphertext() {
        let store = Arc::new(MemorySealedStore::new());
        let mut v = LocalVault::new(
            RustCryptoAead,
            Arc::clone(&store),
            Arc::new(MemoryAccessLog::new()),
            KekRing::new(KeyId::new("kek-1"), Key::generate().unwrap()),
        );
        let t = TenantId::new();
        let id = v.put(t, snmpv3(), &CredentialMeta::new("core")).unwrap();
        let before = store.peek(id).unwrap();

        v.promote_kek(KeyId::new("kek-2"), Key::generate().unwrap());
        let report = v.rotate_kek().unwrap();
        assert_eq!(report.rewrapped, 1);
        assert_eq!(report.failed, 0);

        let after = store.peek(id).unwrap();
        assert_eq!(
            after.kek_id,
            KeyId::new("kek-2"),
            "wrapping key must change"
        );
        assert_ne!(
            after.wrapped_dek, before.wrapped_dek,
            "DEK must be re-wrapped"
        );
        assert_eq!(
            after.ciphertext, before.ciphertext,
            "ciphertext must NOT be touched - that is the point of the envelope"
        );

        assert!(
            v.get(t, id, &ctx()).is_ok(),
            "must still open after rotation"
        );
    }

    #[test]
    fn rotation_is_idempotent() {
        let store = Arc::new(MemorySealedStore::new());
        let mut v = LocalVault::new(
            RustCryptoAead,
            Arc::clone(&store),
            Arc::new(MemoryAccessLog::new()),
            KekRing::new(KeyId::new("kek-1"), Key::generate().unwrap()),
        );
        v.put(TenantId::new(), snmpv3(), &CredentialMeta::new("core"))
            .unwrap();
        v.promote_kek(KeyId::new("kek-2"), Key::generate().unwrap());

        assert_eq!(v.rotate_kek().unwrap().rewrapped, 1);
        // Re-running must be a no-op, not a second re-wrap. On-prem operators will
        // re-run a rotation that appeared to fail partway through.
        let second = v.rotate_kek().unwrap();
        assert_eq!(second.rewrapped, 0);
        assert_eq!(second.already_current, 1);
    }

    #[test]
    fn revoked_credentials_do_not_open() {
        let (v, _, _) = vault();
        let t = TenantId::new();
        let id = v.put(t, snmpv3(), &CredentialMeta::new("core")).unwrap();
        v.revoke(t, id).unwrap();
        assert!(matches!(v.get(t, id, &ctx()), Err(Error::Revoked)));
    }

    #[test]
    fn rotation_creates_a_new_version_and_get_latest_follows_it() {
        let (v, _, _) = vault();
        let t = TenantId::new();
        let id = v.put(t, snmpv3(), &CredentialMeta::new("core")).unwrap();

        let mut meta = CredentialMeta::new("core");
        meta.supersedes = Some(id);
        v.put(
            t,
            Secret::new(CredentialMaterial::SnmpCommunity("rotated".into())),
            &meta,
        )
        .unwrap();

        let latest = v.get_latest(t, "core", &ctx()).unwrap();
        assert!(matches!(
            latest.expose(),
            CredentialMaterial::SnmpCommunity(s) if s == "rotated"
        ));
    }

    #[test]
    fn each_credential_gets_its_own_dek_and_nonce() {
        // A shared DEK would mean one leaked key exposes the whole estate, and a
        // repeated nonce under one key is catastrophic for GCM specifically.
        let (v, store, _) = vault();
        let t = TenantId::new();
        let a = v.put(t, snmpv3(), &CredentialMeta::new("a")).unwrap();
        let b = v.put(t, snmpv3(), &CredentialMeta::new("b")).unwrap();

        let (ra, rb) = (store.peek(a).unwrap(), store.peek(b).unwrap());
        assert_ne!(ra.wrapped_dek, rb.wrapped_dek);
        assert_ne!(ra.nonce, rb.nonce);
        // Identical plaintext must not produce identical ciphertext.
        assert_ne!(ra.ciphertext, rb.ciphertext);
    }
}
