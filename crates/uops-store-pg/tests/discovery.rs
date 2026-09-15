//! Interface discovery, against a real `PostgreSQL`.
//!
//! The half of SPEC §M2's *"creates child resources **and** `member_of` relationships"*
//! that a unit test cannot reach: the upsert key, the composite foreign keys, and the
//! transaction that makes "and" true.

use uops_core::{
    Identifier, IdentifierKind, OrgId, ResourceId, ResourceKind, SiteId, TenantId, TenantScope,
};
use uops_store_pg::{Config, DiscoveredChild, NewResource, PgStore};

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

/// A tenant with a site, and a device in it to hang interfaces off.
async fn device(store: &PgStore, slug: &str) -> (TenantScope, SiteId, ResourceId) {
    let org = OrgId::new();
    let tenant = TenantId::new();
    let site = SiteId::new();
    let unique = tenant.into_uuid().simple().to_string();

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("disc-org-{unique}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("disc-{slug}"))
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

    let scope = TenantScope::collector(tenant);
    let parent = store
        .create_resource(
            &scope,
            &NewResource {
                site_id: Some(site),
                ..NewResource::new(ResourceKind::Device, format!("rtr-{slug}"))
            },
        )
        .await
        .expect("device");

    (scope, site, parent.id)
}

fn interface(name: &str, index: u32) -> DiscoveredChild {
    DiscoveredChild {
        name: name.to_owned(),
        kind: ResourceKind::Interface,
        index: index.to_string(),
        identifiers: Vec::new(),
    }
}

#[tokio::test]
async fn a_walk_creates_a_child_and_an_edge_for_every_row() {
    // The acceptance criterion, in the smallest form it can be checked in. The **and**
    // is the part worth asserting: a child with no edge is unreachable from the device
    // it belongs to, which is the state the topology UI would render as a device with no
    // ports.
    let store = store().await;
    let (scope, site, parent) = device(&store, "creates").await;

    let report = store
        .record_discovery(
            &scope,
            parent,
            &[interface("Gi0/1", 1), interface("Gi0/2", 2)],
        )
        .await
        .expect("discovery");

    assert_eq!(report.created, 2);
    assert_eq!(report.seen, 0);
    assert_eq!(report.edges, 2);

    let children = store.children_of(&scope, parent).await.expect("children");
    assert_eq!(
        children.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>(),
        vec!["Gi0/1", "Gi0/2"]
    );

    let members = store.members_of(&scope, parent).await.expect("members");
    assert_eq!(members.len(), 2, "every child must have its member_of edge");
    let child_ids: Vec<ResourceId> = children.iter().map(|(id, _)| *id).collect();
    for member in &members {
        assert!(
            child_ids.contains(member),
            "an edge points at something that is not one of the children"
        );
    }

    // The child inherits the parent's site: an interface is in the same rack as the
    // device it is part of, and one with no site vanishes from every site-scoped view
    // its parent appears in.
    let child = store
        .resource(&scope, child_ids[0])
        .await
        .expect("the child resource");
    assert_eq!(child.site_id, Some(site));
    assert_eq!(child.parent_id, Some(parent));
    assert_eq!(child.kind, ResourceKind::Interface);
}

#[tokio::test]
async fn rediscovery_updates_rather_than_duplicating() {
    // The property this whole design is for. Discovery runs every fifteen minutes for
    // the life of a device; without a key to match on, a 48-port switch accumulates
    // 4 608 interfaces a day, each holding a fraction of the telemetry and none of them
    // identifiably the real Gi0/1.
    let store = store().await;
    let (scope, _, parent) = device(&store, "again").await;
    let found = [interface("Gi0/1", 1), interface("Gi0/2", 2)];

    let first = store
        .record_discovery(&scope, parent, &found)
        .await
        .expect("first pass");
    assert_eq!((first.created, first.seen), (2, 0));

    let second = store
        .record_discovery(&scope, parent, &found)
        .await
        .expect("second pass");
    assert_eq!(
        (second.created, second.seen),
        (0, 2),
        "a second walk of the same device must find the same interfaces"
    );

    assert_eq!(store.children_of(&scope, parent).await.unwrap().len(), 2);
    assert_eq!(
        store.members_of(&scope, parent).await.unwrap().len(),
        2,
        "the edge is upserted too, or the relationship table grows instead"
    );
}

#[tokio::test]
async fn a_reboot_that_renumbers_an_interface_keeps_the_same_resource() {
    // Why the key is the name and not `ifIndex`. The MIB promises only that an index is
    // stable "between re-initializations"; a switch that reboots and renumbers its ports
    // must not become a switch with twice as many ports.
    let store = store().await;
    let (scope, _, parent) = device(&store, "reboot").await;

    store
        .record_discovery(&scope, parent, &[interface("Gi0/1", 1)])
        .await
        .expect("before");
    let before = store.children_of(&scope, parent).await.unwrap();

    // Same port, new index.
    let report = store
        .record_discovery(&scope, parent, &[interface("Gi0/1", 17)])
        .await
        .expect("after");
    assert_eq!((report.created, report.seen), (0, 1));

    let after = store.children_of(&scope, parent).await.unwrap();
    assert_eq!(after, before, "the resource id must not change");

    // And the new index is recorded, because that is what joins a sample's label back
    // to this row.
    let child = store.resource(&scope, after[0].0).await.expect("child");
    assert_eq!(
        child
            .attributes
            .get("network.interface.index")
            .map(|v| v.as_storage_string().into_owned()),
        Some("17".to_owned())
    );
}

#[tokio::test]
async fn two_devices_may_each_have_a_gi0_1() {
    // The reason the unique index is partial and keyed on the parent. Every switch in a
    // network has a Gi0/1, and an index that refused the second one would make discovery
    // fail on the second device a customer added.
    let store = store().await;
    let (scope, _, first) = device(&store, "dev-a").await;
    let second = store
        .create_resource(&scope, &NewResource::new(ResourceKind::Device, "rtr-b"))
        .await
        .expect("a second device in the same tenant");

    store
        .record_discovery(&scope, first, &[interface("Gi0/1", 1)])
        .await
        .expect("first device");
    store
        .record_discovery(&scope, second.id, &[interface("Gi0/1", 1)])
        .await
        .expect("second device");

    assert_eq!(store.children_of(&scope, first).await.unwrap().len(), 1);
    assert_eq!(store.children_of(&scope, second.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn an_interfaces_mac_is_attached_so_the_resolver_can_find_it_later() {
    // Discovery does not resolve identity — an interface's parent is not in question —
    // but it records what a *later* resolution needs. A flow record or an LLDP neighbour
    // arrives with a MAC and nothing else, and this row is how it reaches the interface.
    let store = store().await;
    let (scope, _, parent) = device(&store, "mac").await;

    let mac = format!("02:00:00:{:02x}:{:02x}:01", rand_byte(), rand_byte());
    let child = DiscoveredChild {
        identifiers: vec![Identifier::new(IdentifierKind::Mac, mac.clone())],
        ..interface("Gi0/1", 1)
    };

    let report = store
        .record_discovery(&scope, parent, &[child])
        .await
        .expect("discovery");
    assert_eq!(report.identifiers, 1);

    let children = store.children_of(&scope, parent).await.unwrap();
    let stored: Vec<(String, String)> = sqlx::query_as(
        "SELECT kind::text, value FROM resource_identifier
          WHERE tenant_id = $1 AND resource_id = $2",
    )
    .bind(scope.tenant_id().into_uuid())
    .bind(children[0].0.into_uuid())
    .fetch_all(store.pool())
    .await
    .expect("identifiers");
    assert_eq!(stored, vec![("mac".to_owned(), mac)]);
}

#[tokio::test]
async fn a_parent_in_another_tenant_is_not_found() {
    // The composite foreign key would refuse this anyway, with a constraint name. Saying
    // which resource is what makes the failure actionable — and 404-never-403 means a
    // caller cannot learn that the id exists somewhere else.
    let store = store().await;
    let (mine, _, _) = device(&store, "mine").await;
    let (_, _, theirs) = device(&store, "theirs").await;

    let err = store
        .record_discovery(&mine, theirs, &[interface("Gi0/1", 1)])
        .await
        .expect_err("another tenant's device must not accept children");
    assert!(
        matches!(err, uops_core::Error::NotFound { .. }),
        "expected NotFound, got {err:?}"
    );
}

#[tokio::test]
async fn nothing_is_written_when_the_walk_found_nothing() {
    // A device with no interface table, or a walk that came back empty. Writing nothing
    // is right; the wrong behaviour would be treating an empty walk as "every interface
    // has gone".
    let store = store().await;
    let (scope, _, parent) = device(&store, "empty").await;

    store
        .record_discovery(&scope, parent, &[interface("Gi0/1", 1)])
        .await
        .expect("one interface");

    let report = store
        .record_discovery(&scope, parent, &[])
        .await
        .expect("an empty walk");
    assert_eq!(report, uops_store_pg::DiscoveryReport::default());
    assert_eq!(
        store.children_of(&scope, parent).await.unwrap().len(),
        1,
        "an empty walk must not remove what an earlier one found"
    );
}

/// A byte from the process id and the clock, so concurrent runs do not collide on the
/// tenant-unique MAC constraint. Not randomness that needs to be good — just different.
fn rand_byte() -> u8 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    u8::try_from((u64::from(nanos) ^ u64::from(std::process::id())) % 256).unwrap_or(0)
}
