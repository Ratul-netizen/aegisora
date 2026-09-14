//! Against a real PostgreSQL.
//!
//! The unit tests in this crate check the parts that are pure — pagination arithmetic,
//! error mapping, the tenant-predicate scanner. Everything that matters about a
//! repository is whether it agrees with the schema, and that can only be settled
//! against a server.
//!
//! ```bash
//! docker compose -f deploy/docker-compose.yml up -d
//! bash scripts/db.sh migrate
//! DATABASE_URL=postgres://uops:uops@localhost:5432/uops cargo test -p uops-store-pg
//! ```
//!
//! Each test creates its own organization and tenant, so they neither collide nor need
//! cleaning up — and "another tenant cannot see this" is asserted with a tenant that
//! genuinely exists rather than with a random UUID that matches nothing.

use uops_core::{ResourceKind, ResourceStatus, TenantId, TenantScope};
use uops_query::{ResourceSelector, compile, resolve};
use uops_store_pg::{Config, NewResource, PgCatalog, PgStore, ResourceFilter};

async fn store() -> PgStore {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "these tests need a database.\n  \
             docker compose -f deploy/docker-compose.yml up -d && bash scripts/db.sh migrate\n  \
             DATABASE_URL=postgres://uops:uops@localhost:5432/uops cargo test -p uops-store-pg"
        )
    });
    PgStore::connect(&Config {
        url,
        ..Config::default()
    })
    .await
    .expect("connect")
}

/// A fresh tenant, with the organization above it. Returns a scope for it.
async fn tenant(store: &PgStore, slug: &str) -> TenantScope {
    let org = uuid::Uuid::now_v7();
    let id = TenantId::new();

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org)
        .bind(format!("test-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");

    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(id.into_uuid())
        .bind(org)
        .bind(format!("test-{slug}"))
        .bind(format!("{slug}-{}", id.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");

    TenantScope::system(id)
}

async fn site(store: &PgStore, scope: &TenantScope, name: &str) -> uops_core::SiteId {
    let id = uops_core::SiteId::new();
    sqlx::query("INSERT INTO site (id, tenant_id, name) VALUES ($1, $2, $3)")
        .bind(id.into_uuid())
        .bind(scope.tenant_id().into_uuid())
        .bind(name)
        .execute(store.pool())
        .await
        .expect("site");
    id
}

#[tokio::test]
async fn a_resource_round_trips() {
    let store = store().await;
    let scope = tenant(&store, "roundtrip").await;

    let mut new = NewResource::new(ResourceKind::Device, "rtr-01");
    new.vendor = Some("cisco".into());
    new.attributes
        .insert(uops_core::attr::semconv::HOST_NAME, "rtr-01");

    let created = store.create_resource(&scope, &new).await.unwrap();
    let fetched = store.resource(&scope, created.id).await.unwrap();

    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.tenant_id, scope.tenant_id());
    assert_eq!(fetched.kind, ResourceKind::Device);
    assert_eq!(fetched.vendor.as_deref(), Some("cisco"));
    assert_eq!(
        fetched.status,
        ResourceStatus::Unknown,
        "the schema default"
    );
    // The jsonb column must survive the trip, not arrive as an empty map.
    assert_eq!(
        fetched.attributes.get(uops_core::attr::semconv::HOST_NAME),
        new.attributes.get(uops_core::attr::semconv::HOST_NAME)
    );
}

#[tokio::test]
async fn another_tenants_resource_is_not_found_rather_than_forbidden() {
    // The isolation assertion, made against a tenant that really exists — a random UUID
    // would pass this test even if the tenant predicate were missing entirely.
    let store = store().await;
    let mine = tenant(&store, "mine").await;
    let theirs = tenant(&store, "theirs").await;

    let hidden = store
        .create_resource(
            &theirs,
            &NewResource::new(ResourceKind::Device, "secret-01"),
        )
        .await
        .unwrap();

    let err = store.resource(&mine, hidden.id).await.unwrap_err();
    assert_eq!(
        err.status_code(),
        404,
        "a 403 would confirm the resource exists in another tenant: {err}"
    );
    assert_eq!(err.problem_type(), "not-found");

    let page = store
        .resources(&mine, &ResourceFilter::default())
        .await
        .unwrap();
    assert!(
        page.items.is_empty(),
        "listing must not reach across tenants either"
    );
}

#[tokio::test]
async fn listing_pages_through_everything_exactly_once() {
    // The property that breaks silently: a keyset that is off by one either skips a
    // device or shows it twice, and on an inventory of fifty thousand nobody notices.
    let store = store().await;
    let scope = tenant(&store, "paging").await;

    let mut created = Vec::new();
    for i in 0..25 {
        let r = store
            .create_resource(
                &scope,
                &NewResource::new(ResourceKind::Host, format!("host-{i:03}")),
            )
            .await
            .unwrap();
        created.push(r.id);
    }

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = store
            .resources(
                &scope,
                &ResourceFilter {
                    limit: Some(7),
                    cursor,
                    ..ResourceFilter::default()
                },
            )
            .await
            .unwrap();

        assert!(page.len() <= 7, "a page must not exceed the requested size");
        seen.extend(page.items.iter().map(|r| r.id));

        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), seen.len(), "a resource appeared on two pages");
    assert_eq!(seen.len(), created.len(), "a resource was skipped");

    let mut expected = created;
    expected.sort();
    assert_eq!(
        seen, expected,
        "UUIDv7 ordering means pages come back in creation order"
    );
}

#[tokio::test]
async fn filters_narrow_the_listing() {
    let store = store().await;
    let scope = tenant(&store, "filters").await;
    let dhaka = site(&store, &scope, "Dhaka DC").await;

    let mut device = NewResource::new(ResourceKind::Device, "rtr-core-01");
    device.site_id = Some(dhaka);
    store.create_resource(&scope, &device).await.unwrap();
    store
        .create_resource(
            &scope,
            &NewResource::new(ResourceKind::Host, "app-server-1"),
        )
        .await
        .unwrap();

    let by_kind = store
        .resources(
            &scope,
            &ResourceFilter {
                kind: Some(ResourceKind::Device),
                ..ResourceFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(by_kind.len(), 1);
    assert_eq!(by_kind.items[0].name, "rtr-core-01");

    let by_site = store
        .resources(
            &scope,
            &ResourceFilter {
                site_id: Some(dhaka),
                ..ResourceFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(by_site.len(), 1);

    let by_name = store
        .resources(
            &scope,
            &ResourceFilter {
                name_contains: Some("CORE".into()),
                ..ResourceFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(by_name.len(), 1, "name search is case-insensitive");

    // A wildcard in user input must be a literal, not a pattern: "%" would otherwise
    // match everything and look like a broken filter.
    let literal = store
        .resources(
            &scope,
            &ResourceFilter {
                name_contains: Some("%".into()),
                ..ResourceFilter::default()
            },
        )
        .await
        .unwrap();
    assert!(literal.is_empty(), "a literal % matched as a wildcard");
}

#[tokio::test]
async fn deleting_a_resource_retires_it_rather_than_removing_it() {
    // A hard delete would orphan every row of telemetry already written under this
    // resource_id, and history that resolves to nothing is worse than a retired row.
    let store = store().await;
    let scope = tenant(&store, "retire").await;

    let r = store
        .create_resource(
            &scope,
            &NewResource::new(ResourceKind::Device, "old-switch"),
        )
        .await
        .unwrap();

    let updated = store
        .set_resource_status(&scope, r.id, ResourceStatus::Decommissioned)
        .await
        .unwrap();
    assert_eq!(updated.status, ResourceStatus::Decommissioned);
    assert!(
        !updated.status.alertable(),
        "a retired device must not page"
    );

    assert!(
        store.resource(&scope, r.id).await.is_ok(),
        "the row must still be readable"
    );
}

#[tokio::test]
async fn the_catalog_collapses_aliases_and_drops_foreign_ids() {
    let store = store().await;
    let scope = tenant(&store, "catalog").await;
    let other = tenant(&store, "catalog-other").await;
    let catalog = PgCatalog::new(store.clone());

    let survivor = store
        .create_resource(&scope, &NewResource::new(ResourceKind::Device, "survivor"))
        .await
        .unwrap();
    let merged = store
        .create_resource(
            &scope,
            &NewResource::new(ResourceKind::Device, "merged-away"),
        )
        .await
        .unwrap();
    let foreign = store
        .create_resource(&other, &NewResource::new(ResourceKind::Device, "not-yours"))
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO resource_alias (tenant_id, historical_id, current_id) VALUES ($1,$2,$3)",
    )
    .bind(scope.tenant_id().into_uuid())
    .bind(merged.id.into_uuid())
    .bind(survivor.id.into_uuid())
    .execute(store.pool())
    .await
    .expect("alias");

    let resolved = resolve(
        &ResourceSelector::Ids {
            ids: vec![merged.id, survivor.id, foreign.id],
        },
        &scope,
        &catalog,
    )
    .await
    .unwrap();

    // The merged ID resolves to the survivor, asking for both does not double it, and
    // the other tenant's resource is gone without a distinguishable error.
    assert_eq!(resolved.ids().unwrap(), &[survivor.id]);
}

#[tokio::test]
async fn the_catalog_walks_the_topology_and_stays_in_its_tenant() {
    let store = store().await;
    let scope = tenant(&store, "walk").await;
    let catalog = PgCatalog::new(store.clone());

    let device = store
        .create_resource(&scope, &NewResource::new(ResourceKind::Device, "rtr-01"))
        .await
        .unwrap();
    let iface = store
        .create_resource(&scope, &NewResource::new(ResourceKind::Interface, "Gi0/1"))
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO resource_relationship (id, tenant_id, source_id, target_id, kind, discovered_by)
         VALUES ($1, $2, $3, $4, 'member_of', 'snmp-iftable')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(scope.tenant_id().into_uuid())
    .bind(iface.id.into_uuid())
    .bind(device.id.into_uuid())
    .execute(store.pool())
    .await
    .expect("edge");

    let resolved = resolve(
        &ResourceSelector::Descendants {
            root: device.id,
            max_depth: 4,
        },
        &scope,
        &catalog,
    )
    .await
    .unwrap();

    let ids = resolved.ids().unwrap();
    assert!(
        ids.contains(&device.id),
        "the root is part of its own blast radius"
    );
    assert!(ids.contains(&iface.id));

    // Another tenant's scope must not be able to walk this graph, even holding the ID.
    let outsider = tenant(&store, "walk-outsider").await;
    let nothing = resolve(
        &ResourceSelector::Descendants {
            root: device.id,
            max_depth: 4,
        },
        &outsider,
        &catalog,
    )
    .await
    .unwrap();
    assert!(
        nothing.is_empty_set(),
        "knowing a UUID is not authorization to traverse from it"
    );
}

#[tokio::test]
async fn a_selector_resolved_from_postgres_compiles_into_clickhouse_sql() {
    // The seam M0 built both halves of and never joined: a selector is expanded against
    // the control plane, and the resulting IDs become the resource predicate of a
    // telemetry query. Until this test existed, nothing connected them.
    let store = store().await;
    let scope = tenant(&store, "endtoend").await;
    let catalog = PgCatalog::new(store.clone());
    let dhaka = site(&store, &scope, "Dhaka DC").await;

    for name in ["rtr-01", "rtr-02"] {
        let mut r = NewResource::new(ResourceKind::Device, name);
        r.site_id = Some(dhaka);
        store.create_resource(&scope, &r).await.unwrap();
    }

    let resolved = resolve(&ResourceSelector::Site { site: dhaka }, &scope, &catalog)
        .await
        .unwrap();
    assert_eq!(resolved.ids().unwrap().len(), 2);

    let query = uops_query::Query::new(
        uops_query::SignalType::Log,
        uops_query::TimeRange::last(chrono::Duration::hours(1)),
    );
    let compiled = compile(&query, &scope, &resolved).unwrap();

    assert!(
        compiled
            .sql
            .text()
            .contains("resource_id IN ({p3:UUID}, {p4:UUID})"),
        "the site's resources must become the predicate: {}",
        compiled.sql.text()
    );
    // And the tenant still comes from the scope, not from the resolved set.
    assert_eq!(
        compiled.sql.params()["p0"].value,
        scope.tenant_id().to_string()
    );
}
