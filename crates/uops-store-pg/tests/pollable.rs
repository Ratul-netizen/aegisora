//! Which resources the poller is handed, against a real `PostgreSQL`.
//!
//! The query is a join with three conditions, and each one is a decision the poller
//! depends on being right: a resource with no address is not a device, a decommissioned
//! one must stop being polled, and another tenant's devices must not appear at all.

use uops_core::{
    AttrValue, Identifier, IdentifierKind as K, OrgId, ResourceKind, ResourceStatus, TenantId,
    TenantScope,
};
use uops_identity::IdentityStore;
use uops_store_pg::{Config, NewResource, PgStore, SYSOBJECTID_KEY};

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
        .bind(format!("poll-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(id.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("poll-{slug}"))
        .bind(format!("{slug}-{}", id.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");
    id
}

/// A resource with an address, which is what makes it a device.
async fn device(
    store: &PgStore,
    scope: &TenantScope,
    name: &str,
    ip: &str,
) -> uops_core::ResourceId {
    let created = store
        .create_resource(scope, &NewResource::new(ResourceKind::Device, name))
        .await
        .expect("resource");
    store
        .attach_identifiers(
            scope.tenant_id(),
            created.id,
            &[Identifier::new(K::MgmtIp, ip)],
            "test",
        )
        .await
        .expect("identifier");
    created.id
}

#[tokio::test]
async fn a_resource_with_an_address_is_a_device_and_one_without_is_not() {
    // The join's premise. A `resource` row is inventory; a device is something the
    // poller can send a packet to, and the difference is one identifier.
    let store = store().await;
    let tenant = tenant(&store, "addr").await;
    let scope = TenantScope::system(tenant);

    let with_address = device(&store, &scope, "switch-1", "10.90.0.1").await;
    let without = store
        .create_resource(&scope, &NewResource::new(ResourceKind::Service, "billing"))
        .await
        .expect("resource")
        .id;

    let devices = store.pollable_devices(&scope, 100).await.expect("query");
    let ids: Vec<_> = devices.iter().map(|d| d.resource_id).collect();

    assert!(ids.contains(&with_address));
    assert!(
        !ids.contains(&without),
        "a resource with no address is not pollable"
    );
    assert_eq!(
        devices
            .iter()
            .find(|d| d.resource_id == with_address)
            .map(|d| d.address.as_str()),
        Some("10.90.0.1")
    );
}

#[tokio::test]
async fn a_decommissioned_device_stops_being_polled() {
    // Decommissioning is a soft delete so history still resolves — SPEC §M1. Continuing
    // to poll something an operator retired produces telemetry nobody asked for and
    // alerts nobody wants.
    let store = store().await;
    let tenant = tenant(&store, "decom").await;
    let scope = TenantScope::system(tenant);

    let id = device(&store, &scope, "switch-2", "10.90.0.2").await;
    assert_eq!(store.pollable_devices(&scope, 100).await.unwrap().len(), 1);

    store
        .set_resource_status(&scope, id, ResourceStatus::Decommissioned)
        .await
        .expect("decommission");

    assert!(
        store
            .pollable_devices(&scope, 100)
            .await
            .unwrap()
            .is_empty(),
        "a decommissioned device must not be polled"
    );
}

#[tokio::test]
async fn a_device_belongs_to_exactly_one_tenant() {
    // The same property every other query here has, asserted at the one place a poller
    // would otherwise reach across tenants: it runs with a system scope and is trusted
    // to have asked for the right one.
    let store = store().await;
    let mine = tenant(&store, "mine").await;
    let theirs = tenant(&store, "theirs").await;

    let ours = device(&store, &TenantScope::system(mine), "ours", "10.91.0.1").await;
    let _ = device(&store, &TenantScope::system(theirs), "theirs", "10.91.0.2").await;

    let devices = store
        .pollable_devices(&TenantScope::system(mine), 100)
        .await
        .unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].resource_id, ours);
}

#[tokio::test]
async fn a_discovered_sysobjectid_is_cached_and_read_back() {
    // Profile resolution matches on it and obtaining it costs a round trip. Caching it
    // is the difference between one extra request per device per cycle and one per
    // device per hardware replacement.
    let store = store().await;
    let tenant = tenant(&store, "sysoid").await;
    let scope = TenantScope::system(tenant);
    let id = device(&store, &scope, "switch-3", "10.92.0.1").await;

    let before = store.pollable_devices(&scope, 10).await.unwrap();
    assert_eq!(
        before[0].sysobjectid, None,
        "a device that has never been polled has no cached sysObjectID"
    );

    store
        .record_sysobjectid(&scope, id, "1.3.6.1.4.1.9.1.2494")
        .await
        .expect("record");

    let after = store.pollable_devices(&scope, 10).await.unwrap();
    assert_eq!(
        after[0].sysobjectid.as_deref(),
        Some("1.3.6.1.4.1.9.1.2494")
    );
}

#[tokio::test]
async fn caching_a_sysobjectid_keeps_the_other_attributes() {
    // attributes also holds semconv keys written by identity resolution and by
    // collectors. Replacing the object rather than merging into it would delete them,
    // and nothing would say so until a query that used one came back empty.
    let store = store().await;
    let tenant = tenant(&store, "attrs").await;
    let scope = TenantScope::system(tenant);

    let mut new = NewResource::new(ResourceKind::Device, "switch-4");
    new.attributes.insert(
        "host.name".to_owned(),
        AttrValue::Str("switch-4.example".into()),
    );
    let created = store.create_resource(&scope, &new).await.expect("resource");
    store
        .attach_identifiers(
            tenant,
            created.id,
            &[Identifier::new(K::MgmtIp, "10.93.0.1")],
            "test",
        )
        .await
        .expect("identifier");

    store
        .record_sysobjectid(&scope, created.id, "1.3.6.1.4.1.14988.1")
        .await
        .expect("record");

    let back = store.resource(&scope, created.id).await.expect("read");
    assert_eq!(
        back.attributes
            .get("host.name")
            .map(AttrValue::as_storage_string),
        Some(std::borrow::Cow::Borrowed("switch-4.example")),
        "the existing attributes must survive: {:?}",
        back.attributes
    );
    assert_eq!(
        back.attributes
            .get(SYSOBJECTID_KEY)
            .map(AttrValue::as_storage_string),
        Some(std::borrow::Cow::Borrowed("1.3.6.1.4.1.14988.1"))
    );
}
