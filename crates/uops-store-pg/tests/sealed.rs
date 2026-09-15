//! Sealed credentials against a real `PostgreSQL`.
//!
//! The rules are proven in `uops-secrets` against `MemorySealedStore`. What only this
//! implementation can get wrong is whether the bytes survive the round trip through
//! `bytea` and whether the two "highest live version" and "revoked but still readable"
//! queries agree with what the vault assumes — so those are what this tests, plus the
//! one property the memory store cannot have at all: that a credential is still there
//! after a restart.

use uops_core::{AuthProtocol, CredentialMaterial, OrgId, PrivProtocol, Secret, TenantId};
use uops_secrets::{AccessContext, CredentialMeta, KekRing, LocalVault};
use uops_store_pg::{Config, PgSealedStore, PgStore};

async fn store() -> PgStore {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://uops:uops@localhost:5432/uops".into());
    PgStore::connect(&Config {
        url,
        ..Config::default()
    })
    .await
    .expect("connect")
}

async fn tenant(store: &PgStore, slug: &str) -> TenantId {
    let org = OrgId::new();
    let id = TenantId::new();
    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("seal-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(id.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("seal-{slug}"))
        .bind(format!("{slug}-{}", id.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");
    id
}

fn snmpv3(auth_key: &str) -> Secret<CredentialMaterial> {
    Secret::new(CredentialMaterial::SnmpV3 {
        username: "netops".into(),
        auth: AuthProtocol::Sha256,
        auth_key: auth_key.into(),
        privacy: PrivProtocol::Aes256,
        priv_key: "priv-key-material".into(),
    })
}

fn ctx() -> AccessContext {
    AccessContext::new(uops_core::scope::Actor::Collector, "snmp-poll")
}

/// A KEK built from fixed bytes, so two rings open the same ciphertext.
///
/// `KekRing` is deliberately not `Clone` — it holds key material — and
/// `ephemeral_for_tests` generates a fresh key each call, which is exactly what a
/// restart must *not* do. Built from a file instead, which is one of the two ways a
/// deployment supplies one and needs no environment mutation (unsafe since Rust 2024,
/// and forbidden in this workspace).
fn fixed_kek(dir: &std::path::Path) -> KekRing {
    let path = dir.join("kek.hex");
    if !path.exists() {
        std::fs::write(&path, "0".repeat(64)).expect("write the test kek");
    }
    KekRing::from_file(&path, uops_secrets::record::KeyId("test-kek".to_owned()))
        .expect("load the test kek")
}

/// A vault over PostgreSQL, with a fixed KEK so a second one can open what the first
/// sealed — which is what a restart is.
type Vault = LocalVault<uops_secrets::RustCryptoAead, PgSealedStore, uops_secrets::MemoryAccessLog>;

fn vault(store: &PgStore, kek: KekRing) -> Vault {
    LocalVault::new(
        uops_secrets::RustCryptoAead,
        PgSealedStore::new(store.clone()),
        uops_secrets::MemoryAccessLog::new(),
        kek,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_survives_a_restart() {
    // The property MemorySealedStore cannot have, and the reason this file exists. Two
    // vaults over the same database and the same KEK: the second is what the process
    // looks like after it is restarted.
    let store = store().await;
    let tenant = tenant(&store, "restart").await;
    let dir = std::env::temp_dir();

    let id = {
        let before = vault(&store, fixed_kek(&dir));
        before
            .put(
                tenant,
                snmpv3("auth-key-material"),
                &CredentialMeta::new("core-switches"),
            )
            .expect("seal")
    };

    let after = vault(&store, fixed_kek(&dir));
    let opened = after.get(tenant, id, &ctx()).expect("open after restart");

    match opened.expose() {
        CredentialMaterial::SnmpV3 {
            username,
            auth,
            auth_key,
            privacy,
            ..
        } => {
            assert_eq!(username, "netops");
            assert_eq!(*auth, AuthProtocol::Sha256);
            assert_eq!(auth_key, "auth-key-material");
            assert_eq!(*privacy, PrivProtocol::Aes256);
        }
        other => panic!("wrong variant after the round trip: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ciphertext_in_the_database_is_not_the_material() {
    // Belt and braces on the thing that would be catastrophic and silent: a row whose
    // "ciphertext" is readable. Asserted against the database rather than the vault,
    // because the vault is the thing that would be wrong.
    let store = store().await;
    let tenant = tenant(&store, "opaque").await;
    let dir = std::env::temp_dir();

    let id = vault(&store, fixed_kek(&dir))
        .put(
            tenant,
            snmpv3("auth-key-material"),
            &CredentialMeta::new("opaque"),
        )
        .expect("seal");

    let (ciphertext, kind): (Vec<u8>, String) =
        sqlx::query_as("SELECT ciphertext, kind FROM credential WHERE id = $1")
            .bind(id.into_uuid())
            .fetch_one(store.pool())
            .await
            .expect("row");

    let as_text = String::from_utf8_lossy(&ciphertext);
    assert!(
        !as_text.contains("auth-key-material"),
        "the material is in the clear"
    );
    assert!(!as_text.contains("netops"), "the username is in the clear");
    // The kind is deliberately in the clear: the UI lists credentials by what they are
    // without opening any of them, and "this tenant has an SNMPv3 credential" is not
    // the secret.
    assert_eq!(kind, "snmpv3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rotation_supersedes_the_previous_version() {
    // How a collector resolves a credential: by name, highest live version, so a
    // rotation takes effect without reconfiguring anything.
    //
    // What this does NOT assert is rollback. Migration 0005 says "rotation writes a new
    // row rather than overwriting one ... a rotation that turns out to be wrong is
    // undone by revoking a row" — and LocalVault::put reuses the credential's id, so
    // the new version replaces the old one in both this store and the memory one. The
    // previous material is gone and revoking leaves nothing to fall back to. That is
    // recorded in STATUS as a decision about the primary key, not patched over here
    // with a test asserting behaviour neither implementation has.
    let store = store().await;
    let tenant = tenant(&store, "rotate").await;
    let dir = std::env::temp_dir();
    let v = vault(&store, fixed_kek(&dir));

    let id = v
        .put(tenant, snmpv3("first-key"), &CredentialMeta::new("core"))
        .expect("v1");

    let mut rotation = CredentialMeta::new("core");
    rotation.supersedes = Some(id);
    v.put(tenant, snmpv3("second-key"), &rotation).expect("v2");

    let latest = v.get_latest(tenant, "core", &ctx()).expect("latest");
    match latest.expose() {
        CredentialMaterial::SnmpV3 { auth_key, .. } => assert_eq!(auth_key, "second-key"),
        other => panic!("{other:?}"),
    }

    // One row, at version 2: the id is stable so that `resource.credential_ref` keeps
    // pointing at the right thing across a rotation.
    let (rows, version): (i64, i32) = sqlx::query_as(
        "SELECT count(*), max(version) FROM credential WHERE tenant_id = $1 AND name = 'core'",
    )
    .bind(tenant.into_uuid())
    .fetch_one(store.pool())
    .await
    .expect("count");
    assert_eq!(rows, 1, "rotation replaces the row rather than adding one");
    assert_eq!(version, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_tenants_credential_is_not_found() {
    let store = store().await;
    let mine = tenant(&store, "mine").await;
    let theirs = tenant(&store, "theirs").await;
    let dir = std::env::temp_dir();
    let v = vault(&store, fixed_kek(&dir));

    let id = v
        .put(theirs, snmpv3("not-yours"), &CredentialMeta::new("theirs"))
        .expect("seal");

    assert!(
        v.get(mine, id, &ctx()).is_err(),
        "a credential id from another tenant must not open"
    );
    assert!(v.get_latest(mine, "theirs", &ctx()).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotating_the_kek_rewraps_without_touching_the_ciphertext() {
    // Envelope encryption's whole point: a KEK rotation re-wraps a 32-byte key per row
    // rather than re-encrypting every credential. On a fleet with thousands of stored
    // credentials that is the difference between a rotation being routine and being a
    // maintenance window.
    let store = store().await;
    let tenant = tenant(&store, "rekey").await;
    let dir = std::env::temp_dir();
    let mut v = vault(&store, fixed_kek(&dir));

    let id = v
        .put(
            tenant,
            snmpv3("survives-rekey"),
            &CredentialMeta::new("rekey"),
        )
        .expect("seal");

    let before: Vec<u8> = sqlx::query_scalar("SELECT ciphertext FROM credential WHERE id = $1")
        .bind(id.into_uuid())
        .fetch_one(store.pool())
        .await
        .expect("before");

    // A rotation needs somewhere to rotate *to*. Promoting first, retiring the old key
    // rather than dropping it, is what keeps un-rewrapped rows readable while the
    // rotation runs — see KekRing's own docs.
    v.promote_kek(
        uops_secrets::record::KeyId("test-kek-2".to_owned()),
        uops_core::Secret::new(uops_secrets::Key::generate().expect("key").expose().clone()),
    );

    let report = v.rotate_kek().expect("rotate");
    assert!(
        report.rewrapped > 0,
        "something must have been re-wrapped: {report:?}"
    );
    assert_eq!(report.failed, 0);

    let after: Vec<u8> = sqlx::query_scalar("SELECT ciphertext FROM credential WHERE id = $1")
        .bind(id.into_uuid())
        .fetch_one(store.pool())
        .await
        .expect("after");
    assert_eq!(
        before, after,
        "a KEK rotation must not re-encrypt the material"
    );

    let opened = v.get(tenant, id, &ctx()).expect("open after rekey");
    match opened.expose() {
        CredentialMaterial::SnmpV3 { auth_key, .. } => assert_eq!(auth_key, "survives-rekey"),
        other => panic!("{other:?}"),
    }
}
