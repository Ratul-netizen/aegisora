//! Profile storage against a real `PostgreSQL`.
//!
//! The rules here are precedence rules, and precedence is the kind of thing that is
//! obvious in one query and wrong in the next one somebody writes. Resolving it in SQL
//! and testing it here is what stops every caller reimplementing it slightly differently.

use uops_core::{OrgId, TenantId};
use uops_profile::{Profile, builtin};
use uops_store_pg::{Config, PgStore};

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
        .bind(format!("prof-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(id.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("prof-{slug}"))
        .bind(format!("{slug}-{}", id.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");
    id
}

/// A profile with the given key, distinguishable by its metric name.
fn custom(key: &str, version: u32, metric: &str) -> Profile {
    Profile::from_yaml(&format!(
        r#"
id: {key}
version: {version}
name: Custom
metrics:
  - name: {metric}
    oid: "1.3.6.1.2.1.1.3.0"
    kind: gauge
    unit: s
    interval: 60s
"#
    ))
    .expect("fixture profile")
}

#[tokio::test]
async fn seeding_is_idempotent() {
    // It runs at every startup. A second call that wrote rows would mean the table
    // churned on every restart, and `updated_at` would stop meaning anything.
    let store = store().await;
    let builtins = builtin::all().unwrap();

    store.seed_builtin_profiles(&builtins).await.expect("seed");
    let second = store.seed_builtin_profiles(&builtins).await.expect("seed");

    assert_eq!(
        second, 0,
        "a second seed of an unchanged binary writes nothing"
    );
}

#[tokio::test]
async fn every_built_in_is_readable_back_as_itself() {
    // The round trip through jsonb. A profile that serialises but does not deserialise
    // would be found at a customer's first poll rather than here.
    let store = store().await;
    let builtins = builtin::all().unwrap();
    store.seed_builtin_profiles(&builtins).await.expect("seed");

    let tenant = tenant(&store, "readback").await;
    let visible = store.profiles_for(tenant).await.expect("read");

    for original in &builtins {
        let found = visible
            .iter()
            .find(|p| p.id == original.id)
            .unwrap_or_else(|| panic!("{} is not visible to a tenant", original.id));
        assert_eq!(found, original, "{} changed in the round trip", original.id);
    }
}

#[tokio::test]
async fn a_tenants_own_profile_shadows_the_built_in() {
    // The reason the table exists: a customer fixes a wrong OID on a Thursday without
    // waiting for a release.
    let store = store().await;
    store
        .seed_builtin_profiles(&builtin::all().unwrap())
        .await
        .expect("seed");

    let mine = tenant(&store, "shadow").await;
    let theirs = tenant(&store, "unshadowed").await;

    store
        .put_profile(mine, &custom("cisco-ios", 1, "vendor.custom.metric"))
        .await
        .expect("put");

    let ours = store.profiles_for(mine).await.unwrap();
    let cisco = ours.iter().find(|p| p.id == "cisco-ios").unwrap();
    assert_eq!(
        cisco.metrics.len(),
        1,
        "the tenant's own profile must win, not the built-in"
    );
    assert_eq!(cisco.metrics[0].name, "vendor.custom.metric");

    // And nobody else is affected by it.
    let unaffected = store.profiles_for(theirs).await.unwrap();
    let their_cisco = unaffected.iter().find(|p| p.id == "cisco-ios").unwrap();
    assert!(
        their_cisco.metrics.len() > 1,
        "another tenant must still see the built-in"
    );
}

#[tokio::test]
async fn the_highest_version_wins() {
    let store = store().await;
    let tenant = tenant(&store, "versions").await;

    store
        .put_profile(tenant, &custom("my-device", 1, "first.version"))
        .await
        .unwrap();
    store
        .put_profile(tenant, &custom("my-device", 2, "second.version"))
        .await
        .unwrap();

    let profiles = store.profiles_for(tenant).await.unwrap();
    let mine: Vec<_> = profiles.iter().filter(|p| p.id == "my-device").collect();
    assert_eq!(mine.len(), 1, "one row per key, not one per version");
    assert_eq!(mine[0].version, 2);
    assert_eq!(mine[0].metrics[0].name, "second.version");
}

#[tokio::test]
async fn seeding_never_removes_a_tenants_override() {
    // The failure that would only appear on the *next restart*: a seed that reconciled
    // the table to match the binary would delete a customer's profile, and their
    // devices would silently go back to the built-in.
    let store = store().await;
    let tenant = tenant(&store, "survive").await;

    store
        .put_profile(tenant, &custom("linux-snmp", 9, "survives.restart"))
        .await
        .unwrap();

    store
        .seed_builtin_profiles(&builtin::all().unwrap())
        .await
        .expect("seed");

    let profiles = store.profiles_for(tenant).await.unwrap();
    let linux = profiles.iter().find(|p| p.id == "linux-snmp").unwrap();
    assert_eq!(
        linux.metrics[0].name, "survives.restart",
        "a startup seed must not remove a tenant's override"
    );
}

#[tokio::test]
async fn a_disabled_profile_is_not_offered() {
    // How an operator turns one off without deleting something they cannot put back.
    let store = store().await;
    let tenant = tenant(&store, "disabled").await;

    store
        .put_profile(tenant, &custom("switched-off", 1, "never.polled"))
        .await
        .unwrap();
    assert!(
        store
            .profiles_for(tenant)
            .await
            .unwrap()
            .iter()
            .any(|p| p.id == "switched-off")
    );

    sqlx::query("UPDATE monitoring_profile SET enabled = false WHERE tenant_id = $1")
        .bind(tenant.into_uuid())
        .execute(store.pool())
        .await
        .expect("disable");

    assert!(
        !store
            .profiles_for(tenant)
            .await
            .unwrap()
            .iter()
            .any(|p| p.id == "switched-off"),
        "a disabled profile must not be offered"
    );
}

#[tokio::test]
async fn a_changed_built_in_is_refreshed_within_its_version() {
    // What a bug fix in a shipped profile looks like: same key, same version, different
    // OID. Without the update the fix would ship in the binary and never reach the
    // table, and the poller reads the table.
    let store = store().await;
    let tenant = tenant(&store, "refresh").await;

    let before = custom("fixture-refresh", 1, "before.the.fix");
    // Seeded as a built-in, which is what makes this the built-in path.
    store.seed_builtin_profiles(&[before]).await.expect("seed");

    let after = custom("fixture-refresh", 1, "after.the.fix");
    let written = store
        .seed_builtin_profiles(std::slice::from_ref(&after))
        .await
        .expect("seed");
    assert_eq!(written, 1, "a changed definition must be written");

    let profiles = store.profiles_for(tenant).await.unwrap();
    let fixed = profiles.iter().find(|p| p.id == "fixture-refresh").unwrap();
    assert_eq!(fixed.metrics[0].name, "after.the.fix");
}
