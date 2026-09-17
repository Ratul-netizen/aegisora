//! Neighbour tables reaching the database — `docs/M5-discovery.md` §4, the last two.
//!
//! * an LLDP walk between two known devices produces exactly one `connected_to` edge, and
//!   re-walking produces no duplicate;
//! * an LLDP neighbour with no matching resource produces a candidate, not a resource.

use std::net::IpAddr;

use uops_core::{
    Identifier, IdentifierKind, OrgId, ResourceId, ResourceKind, TenantId, TenantScope,
};
use uops_discover::Neighbour;
use uops_discover::neighbour::Protocol;
use uops_identity::IdentityStore;
use uops_store_pg::discovery_jobs::{CandidateSource, CandidateState};
use uops_store_pg::sweep_ingest::SweepContext;
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

async fn tenant(store: &PgStore, slug: &str) -> TenantScope {
    let org = OrgId::new();
    let tenant = TenantId::new();
    let unique = tenant.into_uuid().simple().to_string();

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("nb-org-{unique}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("nb-{unique}"))
        .bind(format!("{slug}-{unique}"))
        .execute(store.pool())
        .await
        .expect("tenant");

    TenantScope::collector(tenant)
}

/// A device that already exists, with the identifiers a neighbour sighting would match.
async fn known_device(
    store: &PgStore,
    scope: &TenantScope,
    name: &str,
    chassis: Option<&str>,
    address: Option<&str>,
) -> ResourceId {
    let id = store
        .create_provisional(scope.tenant_id(), ResourceKind::Device, name)
        .await
        .expect("provisional");

    let mut identifiers = vec![Identifier::new(IdentifierKind::Hostname, name)];
    if let Some(chassis) = chassis {
        identifiers.push(Identifier::new(IdentifierKind::ChassisId, chassis));
    }
    if let Some(address) = address {
        identifiers.push(Identifier::new(IdentifierKind::MgmtIp, address));
    }
    store
        .attach_identifiers(scope.tenant_id(), id, &identifiers, "test-fixture")
        .await
        .expect("identifiers");
    id
}

fn lldp_neighbour(chassis: &str, name: &str) -> Neighbour {
    Neighbour {
        protocol: Protocol::Lldp,
        chassis_id: Some(chassis.to_owned()),
        port_id: Some("GigabitEthernet0/1".to_owned()),
        sys_name: Some(name.to_owned()),
        ..Neighbour::default()
    }
}

fn address(s: &str) -> IpAddr {
    s.parse().expect("a test address parses")
}

/// The `connected_to` edges in this tenant, as (source, target) pairs.
async fn edges(store: &PgStore, scope: &TenantScope) -> Vec<(ResourceId, ResourceId)> {
    sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid)>(
        "SELECT source_id, target_id FROM resource_relationship
          WHERE tenant_id = $1 AND kind = 'connected_to'
          ORDER BY source_id, target_id",
    )
    .bind(scope.tenant_id().into_uuid())
    .fetch_all(store.pool())
    .await
    .expect("edges")
    .into_iter()
    .map(|(s, t)| (ResourceId::from_uuid(s), ResourceId::from_uuid(t)))
    .collect()
}

async fn resource_count(store: &PgStore, scope: &TenantScope) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resource WHERE tenant_id = $1")
        .bind(scope.tenant_id().into_uuid())
        .fetch_one(store.pool())
        .await
        .expect("count")
}

#[tokio::test]
async fn two_known_devices_produce_exactly_one_edge() {
    // §4's fifth criterion. Both ends exist, so there is a cable to draw.
    let store = store().await;
    let scope = tenant(&store, "cable").await;

    let core = known_device(&store, &scope, "core-01", Some("00:1b:21:00:00:01"), None).await;
    let edge = known_device(&store, &scope, "edge-01", Some("00:1b:21:00:00:02"), None).await;

    let outcome = store
        .record_neighbours(
            &scope,
            core,
            &[lldp_neighbour("00:1b:21:00:00:02", "edge-01")],
            SweepContext::default(),
        )
        .await
        .expect("record");

    assert_eq!(outcome.edges, 1);
    assert_eq!(outcome.candidates, 0);

    let drawn = edges(&store, &scope).await;
    assert_eq!(drawn.len(), 1);
    let (a, b) = drawn[0];
    assert!(
        (a == core && b == edge) || (a == edge && b == core),
        "the edge must join the two devices"
    );
}

#[tokio::test]
async fn re_walking_produces_no_duplicate() {
    // A nightly walk that added an edge every night would give a topology view with
    // seven parallel cables after a week.
    let store = store().await;
    let scope = tenant(&store, "rewalk").await;

    let core = known_device(&store, &scope, "core-01", Some("00:1b:21:00:01:01"), None).await;
    known_device(&store, &scope, "edge-01", Some("00:1b:21:00:01:02"), None).await;
    let seen = [lldp_neighbour("00:1b:21:00:01:02", "edge-01")];

    for _ in 0..3 {
        store
            .record_neighbours(&scope, core, &seen, SweepContext::default())
            .await
            .expect("record");
    }

    assert_eq!(
        edges(&store, &scope).await.len(),
        1,
        "one cable, three walks"
    );
}

#[tokio::test]
async fn walking_both_ends_of_a_cable_still_produces_one_edge() {
    // The duplicate the schema's UNIQUE does *not* catch: A→B and B→A differ in
    // source_id and target_id, so both would be stored. `connected_to` is symmetric, so
    // the pair is sorted before it is written.
    let store = store().await;
    let scope = tenant(&store, "bothends").await;

    let core = known_device(&store, &scope, "core-01", Some("00:1b:21:00:02:01"), None).await;
    let edge = known_device(&store, &scope, "edge-01", Some("00:1b:21:00:02:02"), None).await;

    store
        .record_neighbours(
            &scope,
            core,
            &[lldp_neighbour("00:1b:21:00:02:02", "edge-01")],
            SweepContext::default(),
        )
        .await
        .expect("from core");
    store
        .record_neighbours(
            &scope,
            edge,
            &[lldp_neighbour("00:1b:21:00:02:01", "core-01")],
            SweepContext::default(),
        )
        .await
        .expect("from edge");

    assert_eq!(
        edges(&store, &scope).await.len(),
        1,
        "one cable, however many of its ends were walked"
    );
}

#[tokio::test]
async fn an_unknown_neighbour_is_a_candidate_and_not_a_resource() {
    // §4's sixth criterion, and §2.5's whole argument. A chassis ID is an identifier, not
    // a device; inventing the far end produces an inventory full of half-devices that
    // never get polled because nothing knows how to reach them.
    let store = store().await;
    let scope = tenant(&store, "stranger").await;

    let core = known_device(&store, &scope, "core-01", Some("00:1b:21:00:03:01"), None).await;
    let before = resource_count(&store, &scope).await;

    let outcome = store
        .record_neighbours(
            &scope,
            core,
            &[lldp_neighbour("00:1b:21:ff:ff:ff", "mystery-switch")],
            SweepContext::default(),
        )
        .await
        .expect("record");

    assert_eq!(outcome.edges, 0, "there is nothing to draw a cable to");
    assert_eq!(outcome.candidates, 1);
    assert_eq!(
        resource_count(&store, &scope).await,
        before,
        "a neighbour walk must never create the far end"
    );

    let candidates = store.discovery_candidates(&scope, 10).await.expect("list");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].source, CandidateSource::Lldp);
    assert_eq!(
        candidates[0].chassis_id.as_deref(),
        Some("00:1b:21:ff:ff:ff")
    );
    assert_eq!(
        candidates[0].seen_from,
        Some(core),
        "an operator looking at an unexpected device must be able to see which switch \
         reported it"
    );
    assert!(
        candidates[0].reason.contains("address"),
        "and be told why it cannot simply be added: {}",
        candidates[0].reason
    );
}

#[tokio::test]
async fn a_chassis_id_alone_identifies_a_device() {
    // Tier 1: globally unique by specification, so one hit is proof rather than evidence.
    // This is why LLDP is worth more than the other two protocols put together.
    let store = store().await;
    let scope = tenant(&store, "tierone").await;

    let core = known_device(&store, &scope, "core-01", Some("00:1b:21:00:04:01"), None).await;
    known_device(&store, &scope, "edge-01", Some("00:1b:21:00:04:02"), None).await;

    // Reported with a chassis id that matches and a name that does not -- a device that
    // was renamed since the last walk. The chassis id decides.
    let outcome = store
        .record_neighbours(
            &scope,
            core,
            &[lldp_neighbour("00:1b:21:00:04:02", "renamed-since")],
            SweepContext::default(),
        )
        .await
        .expect("record");

    assert_eq!(outcome.edges, 1, "the chassis id settles it on its own");
    assert_eq!(outcome.candidates, 0);
}

#[tokio::test]
async fn a_neighbour_matching_two_devices_is_ambiguous_rather_than_a_guess() {
    // "core-01 in every building" reaching the topology. Drawing a cable to the wrong
    // building is worse than drawing none, and there is no tier-1 identifier here to
    // break the tie.
    let store = store().await;
    let scope = tenant(&store, "twins").await;

    let walker = known_device(&store, &scope, "walker", Some("00:1b:21:00:05:00"), None).await;
    // Two devices that between them answer to the neighbour's name and address.
    known_device(&store, &scope, "shared-name", None, None).await;
    known_device(&store, &scope, "other", None, Some("10.40.0.9")).await;

    let outcome = store
        .record_neighbours(
            &scope,
            walker,
            &[Neighbour {
                protocol: Protocol::Cdp,
                sys_name: Some("shared-name".to_owned()),
                address: Some(address("10.40.0.9")),
                ..Neighbour::default()
            }],
            SweepContext::default(),
        )
        .await
        .expect("record");

    assert_eq!(outcome.edges, 0, "no cable may be drawn on a guess");
    assert_eq!(outcome.candidates, 1);

    let candidates = store.discovery_candidates(&scope, 10).await.expect("list");
    assert_eq!(candidates[0].state, CandidateState::Ambiguous);
    assert_eq!(candidates[0].source, CandidateSource::Cdp);
}

#[tokio::test]
async fn a_device_reporting_itself_is_not_an_edge() {
    // Some agents list their own chassis in lldpRemTable when two of their ports are
    // patched together. The schema refuses a self-loop, so without this the walk would
    // fail rather than shrug.
    let store = store().await;
    let scope = tenant(&store, "selfloop").await;

    let core = known_device(&store, &scope, "core-01", Some("00:1b:21:00:06:01"), None).await;

    let outcome = store
        .record_neighbours(
            &scope,
            core,
            &[lldp_neighbour("00:1b:21:00:06:01", "core-01")],
            SweepContext::default(),
        )
        .await
        .expect("a self-sighting must not be an error");

    assert_eq!(outcome.edges, 0);
    assert_eq!(outcome.candidates, 0, "it is not a finding either");
    assert!(edges(&store, &scope).await.is_empty());
}

#[tokio::test]
async fn an_arp_sighting_becomes_a_candidate_and_never_an_edge() {
    // §2.5. An ARP table proves an address is in use on a subnet; a router with a
    // thousand laptops behind it has a thousand entries and none of them is adjacency.
    let store = store().await;
    let scope = tenant(&store, "arp").await;

    let router = known_device(&store, &scope, "rtr-01", Some("00:1b:21:00:07:01"), None).await;
    // Even when the address *does* match a device we know, ARP does not draw a cable.
    known_device(&store, &scope, "server-01", None, Some("10.41.0.20")).await;

    let outcome = store
        .record_neighbours(
            &scope,
            router,
            &[Neighbour {
                protocol: Protocol::Arp,
                address: Some(address("10.41.0.99")),
                mac: Some("00:0c:29:aa:bb:cc".to_owned()),
                ..Neighbour::default()
            }],
            SweepContext::default(),
        )
        .await
        .expect("record");

    assert_eq!(outcome.candidates, 1);
    let candidates = store.discovery_candidates(&scope, 10).await.expect("list");
    assert_eq!(candidates[0].source, CandidateSource::Arp);
    assert!(
        candidates[0].reason.contains("ARP"),
        "the reason must say how weak the evidence is: {}",
        candidates[0].reason
    );
}

#[tokio::test]
async fn a_neighbour_candidate_is_one_row_however_often_it_is_seen() {
    // Nightly walks of a switch with an unadopted neighbour on it. One row, updated.
    let store = store().await;
    let scope = tenant(&store, "repeat").await;

    let core = known_device(&store, &scope, "core-01", Some("00:1b:21:00:08:01"), None).await;
    let seen = [lldp_neighbour("00:1b:21:aa:aa:aa", "not-adopted")];

    for _ in 0..3 {
        store
            .record_neighbours(&scope, core, &seen, SweepContext::default())
            .await
            .expect("record");
    }

    let candidates = store.discovery_candidates(&scope, 10).await.expect("list");
    assert_eq!(candidates.len(), 1, "one neighbour, three walks, one row");
    assert!(candidates[0].last_seen >= candidates[0].first_seen);
}

#[tokio::test]
async fn an_edge_cannot_be_drawn_into_another_tenant() {
    // The composite foreign keys on both endpoints. An edge is the one shape that could
    // bridge two tenants' graphs, and a bridged graph is a cross-tenant read for every
    // traversal that follows it.
    let store = store().await;
    let ours = tenant(&store, "ourgraph").await;
    let theirs = tenant(&store, "theirgraph").await;

    let ours_core = known_device(&store, &ours, "core-01", Some("00:1b:21:00:09:01"), None).await;
    // Their device answers to the same chassis id -- which is impossible in reality and
    // is exactly what an attacker would arrange.
    known_device(&store, &theirs, "theirs", Some("00:1b:21:00:09:02"), None).await;

    let outcome = store
        .record_neighbours(
            &ours,
            ours_core,
            &[lldp_neighbour("00:1b:21:00:09:02", "theirs")],
            SweepContext::default(),
        )
        .await
        .expect("record");

    assert_eq!(
        outcome.edges, 0,
        "their device is not visible from our tenant, so there is nothing to join to"
    );
    assert_eq!(
        outcome.candidates, 1,
        "it is a stranger to us, which is a candidate"
    );
    assert!(edges(&store, &ours).await.is_empty());
}
