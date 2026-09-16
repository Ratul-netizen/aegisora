//! Sites on a map, against a real `PostgreSQL`.
//!
//! The rollup and the constraints. Both are the kind of thing that looks obviously right
//! and is obviously wrong the first time a decommissioned device is counted or a typo
//! puts a site in the sea.

use uops_core::{OrgId, ResourceKind, ResourceStatus, SiteId, TenantId, TenantScope};
use uops_store_pg::{Config, Location, NewResource, PgStore};

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

/// A tenant with one site.
async fn tenant_with_site(store: &PgStore, slug: &str) -> (TenantScope, SiteId) {
    let org = OrgId::new();
    let tenant = TenantId::new();
    let site = SiteId::new();
    let unique = tenant.into_uuid().simple().to_string();

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("map-org-{unique}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("map-{slug}"))
        .bind(format!("{slug}-{unique}"))
        .execute(store.pool())
        .await
        .expect("tenant");
    sqlx::query("INSERT INTO site (id, tenant_id, name) VALUES ($1, $2, $3)")
        .bind(site.into_uuid())
        .bind(tenant.into_uuid())
        .bind("dhaka")
        .execute(store.pool())
        .await
        .expect("site");

    (TenantScope::collector(tenant), site)
}

async fn device_at(store: &PgStore, scope: &TenantScope, site: SiteId, status: ResourceStatus) {
    let resource = store
        .create_resource(
            scope,
            &NewResource {
                site_id: Some(site),
                ..NewResource::new(ResourceKind::Device, format!("d-{}", SiteId::new()))
            },
        )
        .await
        .expect("device");
    store
        .set_resource_status(scope, resource.id, status)
        .await
        .expect("status");
}

/// Dhaka, to four decimal places.
const DHAKA: Location = Location {
    latitude: 23.8103,
    longitude: 90.4125,
};

#[tokio::test]
async fn a_site_starts_unplaced_and_can_be_put_on_the_map() {
    // Unplaced is the normal state, not an error: an operator places the sites that
    // matter and leaves the rest.
    let store = store().await;
    let (scope, site) = tenant_with_site(&store, "place").await;

    let before = store.site_overview(&scope).await.expect("overview");
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].location, None);

    store
        .place_site(&scope, site, Some(DHAKA))
        .await
        .expect("place");

    let after = store.site_overview(&scope).await.expect("overview");
    let placed = after[0].location.expect("the site is placed");
    assert!((placed.latitude - DHAKA.latitude).abs() < 1e-9);
    assert!((placed.longitude - DHAKA.longitude).abs() < 1e-9);

    // And taken off again.
    store.place_site(&scope, site, None).await.expect("clear");
    assert_eq!(
        store.site_overview(&scope).await.expect("overview")[0].location,
        None
    );
}

#[tokio::test]
async fn the_rollup_counts_each_status_separately() {
    // Not a worst-case reduction. "One of two hundred is down" and "two hundred of two
    // hundred are down" are the same colour on a map and a very different morning, so
    // the pin needs the numbers rather than a verdict.
    let store = store().await;
    let (scope, site) = tenant_with_site(&store, "rollup").await;

    for _ in 0..3 {
        device_at(&store, &scope, site, ResourceStatus::Up).await;
    }
    device_at(&store, &scope, site, ResourceStatus::Down).await;
    device_at(&store, &scope, site, ResourceStatus::Degraded).await;
    device_at(&store, &scope, site, ResourceStatus::Maintenance).await;

    let counts = store.site_overview(&scope).await.expect("overview")[0].resources;
    assert_eq!(counts.up, 3);
    assert_eq!(counts.down, 1);
    assert_eq!(counts.degraded, 1);
    assert_eq!(counts.maintenance, 1);
    assert_eq!(counts.total, 6);
}

#[tokio::test]
async fn a_decommissioned_device_is_not_on_the_map() {
    // It is kept so history resolves — SPEC makes decommissioning a soft delete — and a
    // map that showed it would be showing an estate the customer no longer has.
    let store = store().await;
    let (scope, site) = tenant_with_site(&store, "gone").await;

    device_at(&store, &scope, site, ResourceStatus::Up).await;
    device_at(&store, &scope, site, ResourceStatus::Decommissioned).await;

    let counts = store.site_overview(&scope).await.expect("overview")[0].resources;
    assert_eq!(counts.total, 1, "the decommissioned device was counted");
    assert_eq!(counts.up, 1);
}

#[tokio::test]
async fn a_site_with_nothing_in_it_is_still_a_site() {
    // A site an operator has just created and placed should appear on the map before
    // anything is put in it. An INNER JOIN would drop it.
    let store = store().await;
    let (scope, site) = tenant_with_site(&store, "empty").await;
    store
        .place_site(&scope, site, Some(DHAKA))
        .await
        .expect("place");

    let sites = store.site_overview(&scope).await.expect("overview");
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].resources.total, 0);
    assert!(sites[0].location.is_some());
}

#[tokio::test]
async fn a_coordinate_outside_its_range_is_refused_with_the_number_in_the_message() {
    // The constraint is what makes it true; the check in Rust is what makes the message
    // say which number was wrong, which a caller filling in a form needs.
    let store = store().await;
    let (scope, site) = tenant_with_site(&store, "typo").await;

    for (lat, lon, expect) in [
        (91.0, 0.0, "latitude"),
        (-91.0, 0.0, "latitude"),
        (0.0, 181.0, "longitude"),
        (0.0, -181.0, "longitude"),
    ] {
        let err = store
            .place_site(
                &scope,
                site,
                Some(Location {
                    latitude: lat,
                    longitude: lon,
                }),
            )
            .await
            .expect_err("a coordinate outside its range must be refused");
        let message = err.to_string();
        assert!(message.contains(expect), "{message}");
    }

    // And nothing was written by any of them.
    assert_eq!(
        store.site_overview(&scope).await.expect("overview")[0].location,
        None
    );
}

#[tokio::test]
async fn the_database_refuses_half_a_coordinate() {
    // Nothing in Rust can express one — `Location` holds both — so this asserts the
    // constraint directly. It is the reason no reader has to decide what half a location
    // means.
    let store = store().await;
    let (scope, site) = tenant_with_site(&store, "half").await;

    let result = sqlx::query("UPDATE site SET latitude = 23.8 WHERE tenant_id = $1 AND id = $2")
        .bind(scope.tenant_id().into_uuid())
        .bind(site.into_uuid())
        .execute(store.pool())
        .await;
    assert!(
        result.is_err(),
        "the schema must refuse a latitude with no longitude"
    );
}

#[tokio::test]
async fn another_tenants_site_is_not_found() {
    // 404-never-403. Confirming the id exists would leak that customer's estate.
    let store = store().await;
    let (mine, _) = tenant_with_site(&store, "mine").await;
    let (_, theirs) = tenant_with_site(&store, "theirs").await;

    let err = store
        .place_site(&mine, theirs, Some(DHAKA))
        .await
        .expect_err("another tenant's site must not be placeable");
    assert!(
        matches!(err, uops_core::Error::NotFound { .. }),
        "expected NotFound, got {err:?}"
    );

    // And it is not in the listing either.
    assert_eq!(store.site_overview(&mine).await.expect("overview").len(), 1);
}
