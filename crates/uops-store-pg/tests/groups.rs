//! Resource groups and operator tags, against a real `PostgreSQL`.
//!
//! The parts a unit test cannot reach: the composite foreign keys that make a
//! cross-tenant membership unwritable, the `CHECK` that makes a tag a flat string map,
//! and the two catalog queries the Query AST compiles a selector into.

use uops_core::{
    OrgId, ResourceGroupId, ResourceId, ResourceKind, Tags, TenantId, TenantScope, tags::well_known,
};
use uops_query::{ResourceCatalog, ResourceSelector, resolve};
use uops_store_pg::{Config, NewGroup, NewResource, PgCatalog, PgStore};

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

/// A tenant with three devices in it.
async fn tenant(store: &PgStore, slug: &str) -> (TenantScope, Vec<ResourceId>) {
    let org = OrgId::new();
    let tenant = TenantId::new();
    let unique = tenant.into_uuid().simple().to_string();

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("grp-org-{unique}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("grp-{slug}"))
        .bind(format!("{slug}-{unique}"))
        .execute(store.pool())
        .await
        .expect("tenant");

    let scope = TenantScope::collector(tenant);
    let mut ids = Vec::new();
    for n in 0..3 {
        let r = store
            .create_resource(
                &scope,
                &NewResource::new(ResourceKind::Device, format!("rtr-{slug}-{n}")),
            )
            .await
            .expect("device");
        ids.push(r.id);
    }
    (scope, ids)
}

#[tokio::test]
async fn a_group_holds_resources_and_reports_its_size() {
    let store = store().await;
    let (scope, ids) = tenant(&store, "basic").await;

    let group = store
        .create_group(
            &scope,
            &NewGroup::new("Core Routers").described("the spine"),
        )
        .await
        .expect("create");
    assert_eq!(group.name, "Core Routers");

    assert_eq!(
        store
            .add_to_group(&scope, group.id, &ids[..2])
            .await
            .expect("add"),
        2
    );

    // Re-adding is what a UI does when somebody re-selects a row. Not an error, and not
    // a second row.
    assert_eq!(
        store
            .add_to_group(&scope, group.id, &ids[..2])
            .await
            .expect("re-add"),
        0,
        "adding a member twice must be idempotent"
    );

    let listed = store.groups(&scope).await.expect("list");
    let mine = listed
        .iter()
        .find(|g| g.group.id == group.id)
        .expect("the group is listed");
    assert_eq!(mine.members, 2);

    // An empty group still appears, with a zero. A group somebody just created and has
    // not filled yet is exactly the one they are looking for.
    let empty = store
        .create_group(&scope, &NewGroup::new("Not Filled Yet"))
        .await
        .expect("create empty");
    let listed = store.groups(&scope).await.expect("list again");
    assert_eq!(
        listed
            .iter()
            .find(|g| g.group.id == empty.id)
            .expect("the empty group is listed")
            .members,
        0
    );

    // And the reverse direction, which the resource detail page asks.
    let of = store.groups_of(&scope, ids[0]).await.expect("groups_of");
    assert_eq!(of.len(), 1);
    assert_eq!(of[0].id, group.id);

    assert_eq!(
        store
            .remove_from_group(&scope, group.id, &[ids[0]])
            .await
            .expect("remove"),
        1
    );
    assert!(
        store
            .groups_of(&scope, ids[0])
            .await
            .expect("after")
            .is_empty()
    );
}

#[tokio::test]
async fn a_group_cannot_contain_another_tenants_resource() {
    // The structural half of tenant isolation. The composite foreign key in migration
    // 0011 refuses this, so it is the database saying no rather than a check in Rust that
    // could drift out of step with the schema.
    let store = store().await;
    let (a, _) = tenant(&store, "iso-a").await;
    let (_, theirs) = tenant(&store, "iso-b").await;

    let group = store
        .create_group(&a, &NewGroup::new("Mine"))
        .await
        .expect("create");

    let err = store
        .add_to_group(&a, group.id, &theirs[..1])
        .await
        .expect_err("another tenant's resource must not be addable");
    // The message is the database's; what matters is that it failed at all, and that
    // the failure is a constraint violation rather than a panic.
    assert!(!format!("{err}").is_empty(), "{err:?}");

    assert_eq!(
        store.groups(&a).await.expect("list")[0].members,
        0,
        "and nothing was written"
    );
}

#[tokio::test]
async fn another_tenants_group_is_not_found_rather_than_forbidden() {
    // 404-never-403. A distinguishable answer is a way to enumerate another customer's
    // groups by id, and this is exactly the bug the isolation harness found twice in the
    // credential routes.
    let store = store().await;
    let (a, _) = tenant(&store, "nf-a").await;
    let (b, _) = tenant(&store, "nf-b").await;

    let theirs = store
        .create_group(&b, &NewGroup::new("Theirs"))
        .await
        .expect("create");

    assert!(matches!(
        store.group(&a, theirs.id).await,
        Err(uops_core::Error::NotFound { .. })
    ));
    assert!(matches!(
        store.delete_group(&a, theirs.id).await,
        Err(uops_core::Error::NotFound { .. })
    ));
    assert!(matches!(
        store
            .rename_group(&a, theirs.id, &NewGroup::new("Stolen"))
            .await,
        Err(uops_core::Error::NotFound { .. })
    ));

    // A group that never existed answers identically, which is the point.
    assert!(matches!(
        store.group(&a, ResourceGroupId::new()).await,
        Err(uops_core::Error::NotFound { .. })
    ));

    // And theirs is untouched.
    assert_eq!(
        store.group(&b, theirs.id).await.expect("still there").name,
        "Theirs"
    );
}

#[tokio::test]
async fn deleting_a_group_keeps_the_resources() {
    let store = store().await;
    let (scope, ids) = tenant(&store, "del").await;

    let group = store
        .create_group(&scope, &NewGroup::new("Temporary"))
        .await
        .expect("create");
    store
        .add_to_group(&scope, group.id, &ids)
        .await
        .expect("add");
    store.delete_group(&scope, group.id).await.expect("delete");

    // The membership rows went with it; the devices did not.
    assert!(
        store
            .groups_of(&scope, ids[0])
            .await
            .expect("after")
            .is_empty()
    );
    assert!(
        store.resource(&scope, ids[0]).await.is_ok(),
        "the device survives"
    );
}

#[tokio::test]
async fn two_tenants_may_each_have_a_group_of_the_same_name() {
    // Scoped uniqueness. Two customers of one MSP both have core routers, and a global
    // constraint would mean whichever of them onboarded second could not say so.
    let store = store().await;
    let (a, _) = tenant(&store, "name-a").await;
    let (b, _) = tenant(&store, "name-b").await;

    store
        .create_group(&a, &NewGroup::new("Core Routers"))
        .await
        .expect("a");
    store
        .create_group(&b, &NewGroup::new("Core Routers"))
        .await
        .expect("b");

    // But one tenant may not have two.
    assert!(
        store
            .create_group(&a, &NewGroup::new("Core Routers"))
            .await
            .is_err(),
        "a duplicate name within one tenant must be refused"
    );
}

#[tokio::test]
async fn tags_are_written_by_humans_and_replaced_wholesale() {
    let store = store().await;
    let (scope, ids) = tenant(&store, "tags").await;

    let tagged = store
        .set_tags(
            &scope,
            ids[0],
            &Tags::new()
                .with(well_known::ENVIRONMENT, "production")
                .with(well_known::CRITICALITY, "critical"),
        )
        .await
        .expect("set");
    assert_eq!(tagged.tags.get(well_known::ENVIRONMENT), Some("production"));

    // Replace, not merge. A PUT of the whole map is how a human removes a tag; a
    // merge-only API would have no way to delete one, and "I removed criticality and it
    // came back" is a bug report nobody should have to file.
    let retagged = store
        .set_tags(
            &scope,
            ids[0],
            &Tags::new().with(well_known::ENVIRONMENT, "staging"),
        )
        .await
        .expect("replace");
    assert_eq!(retagged.tags.get(well_known::ENVIRONMENT), Some("staging"));
    assert_eq!(
        retagged.tags.get(well_known::CRITICALITY),
        None,
        "a replaced map must not keep what was not in it"
    );

    // And they survive a re-read, which is the only proof the column is really jsonb and
    // not something that round-trips through a string.
    let read = store.resource(&scope, ids[0]).await.expect("read");
    assert_eq!(read.tags, retagged.tags);
}

#[tokio::test]
async fn tags_and_attributes_are_different_columns() {
    // The entire point of the feature. Discovery writes attributes on every walk; if the
    // two shared a column it would overwrite `criticality=critical` and nothing would
    // report it.
    let store = store().await;
    let (scope, _) = tenant(&store, "sep").await;

    let created = store
        .create_resource(
            &scope,
            &NewResource {
                attributes: uops_core::AttrMap::new().with("host.name", "rtr-01"),
                ..NewResource::new(ResourceKind::Device, "rtr-sep")
            },
        )
        .await
        .expect("create");
    assert!(created.tags.is_empty(), "a created resource has no tags");

    let tagged = store
        .set_tags(
            &scope,
            created.id,
            &Tags::new().with("owner", "network-team"),
        )
        .await
        .expect("tag");

    assert_eq!(
        tagged.attributes.get_str("host.name"),
        Some("rtr-01"),
        "tagging must not disturb what discovery wrote"
    );
    assert_eq!(tagged.tags.get("owner"), Some("network-team"));
}

#[tokio::test]
async fn an_invalid_tag_is_refused_with_which_tag_it_was() {
    let store = store().await;
    let (scope, ids) = tenant(&store, "bad").await;

    // Checked in Rust before the database sees it, so the caller learns which tag is
    // wrong rather than a constraint name.
    let err = store
        .set_tags(&scope, ids[0], &Tags::new().with("note", "x".repeat(1_000)))
        .await
        .expect_err("an over-long value must be refused");
    assert!(
        format!("{err}").contains("note"),
        "the error must name the tag: {err}"
    );

    assert!(
        store
            .resource(&scope, ids[0])
            .await
            .expect("read")
            .tags
            .is_empty(),
        "and nothing was written"
    );
}

#[tokio::test]
async fn a_tag_cannot_be_set_on_another_tenants_resource() {
    let store = store().await;
    let (a, _) = tenant(&store, "tag-iso-a").await;
    let (b, theirs) = tenant(&store, "tag-iso-b").await;

    assert!(matches!(
        store
            .set_tags(&a, theirs[0], &Tags::new().with("owner", "me"))
            .await,
        Err(uops_core::Error::NotFound { .. })
    ));
    assert!(
        store
            .resource(&b, theirs[0])
            .await
            .expect("read")
            .tags
            .is_empty(),
        "and nothing was written"
    );
}

#[tokio::test]
async fn the_selectors_compile_against_real_sql() {
    // The two new `ResourceSelector` arms, end to end through `resolve` and the
    // PostgreSQL catalog. The unit tests prove the resolution rules against a fake; this
    // proves the SQL, including the `@>` containment the GIN index serves.
    let store = store().await;
    let (scope, ids) = tenant(&store, "sel").await;
    let catalog = PgCatalog::new(store.clone());

    let group = store
        .create_group(&scope, &NewGroup::new("Selected"))
        .await
        .expect("create");
    store
        .add_to_group(&scope, group.id, &ids[..2])
        .await
        .expect("add");

    let resolved = resolve(
        &ResourceSelector::Group { group: group.id },
        &scope,
        &catalog,
    )
    .await
    .expect("resolve group");
    let mut got = resolved.ids().expect("a set").to_vec();
    got.sort_unstable();
    let mut want = ids[..2].to_vec();
    want.sort_unstable();
    assert_eq!(got, want);

    store
        .set_tags(
            &scope,
            ids[2],
            &Tags::new().with(well_known::ENVIRONMENT, "production"),
        )
        .await
        .expect("tag");

    let resolved = resolve(
        &ResourceSelector::Tagged {
            key: well_known::ENVIRONMENT.to_owned(),
            value: "production".to_owned(),
        },
        &scope,
        &catalog,
    )
    .await
    .expect("resolve tag");
    assert_eq!(resolved.ids().expect("a set"), &[ids[2]]);

    // A value that matches nothing is an empty set, not the whole tenant. Those compile
    // to opposite queries, and getting it backwards would silently widen an alert rule to
    // every resource.
    let resolved = resolve(
        &ResourceSelector::Tagged {
            key: well_known::ENVIRONMENT.to_owned(),
            value: "staging".to_owned(),
        },
        &scope,
        &catalog,
    )
    .await
    .expect("resolve tag");
    assert!(resolved.is_empty_set());
}

#[tokio::test]
async fn a_selector_never_reaches_another_tenants_resources() {
    // Both new catalog queries are tenant-scoped in SQL, not only by the foreign keys.
    // Asserted directly, because a selector that widened would do so silently and would
    // be the worst possible bug in this product.
    let store = store().await;
    let (a, _) = tenant(&store, "sel-iso-a").await;
    let (b, theirs) = tenant(&store, "sel-iso-b").await;
    let catalog = PgCatalog::new(store.clone());

    let their_group = store
        .create_group(&b, &NewGroup::new("Theirs"))
        .await
        .expect("create");
    store
        .add_to_group(&b, their_group.id, &theirs)
        .await
        .expect("add");
    store
        .set_tags(
            &b,
            theirs[0],
            &Tags::new().with(well_known::ENVIRONMENT, "production"),
        )
        .await
        .expect("tag");

    assert!(
        catalog
            .in_group(a.tenant_id(), their_group.id)
            .await
            .expect("in_group")
            .is_empty(),
        "another tenant's group must resolve to nothing"
    );
    assert!(
        catalog
            .tagged(a.tenant_id(), well_known::ENVIRONMENT, "production")
            .await
            .expect("tagged")
            .is_empty(),
        "another tenant's tagged resources must not appear"
    );

    // And the owner still sees them, so the emptiness above is scoping rather than a
    // query that returns nothing for everyone.
    assert_eq!(
        catalog
            .in_group(b.tenant_id(), their_group.id)
            .await
            .expect("theirs")
            .len(),
        3
    );
}
