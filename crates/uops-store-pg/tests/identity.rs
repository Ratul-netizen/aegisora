//! `PgIdentityStore` against a real PostgreSQL.
//!
//! The resolution *rules* are proven in `uops-identity` against an in-memory store —
//! exhaustively, and without a database. Repeating them here would test the same logic
//! twice and the SQL not at all.
//!
//! So these test what only this implementation can get wrong: whether the statements
//! agree with the schema, whether `ON CONFLICT` does what the resolver assumes, whether
//! merge and split are atomic, and whether the review deduplication survives a round
//! trip through `jsonb` — where identifier order is not preserved and set equality has
//! to be asked for explicitly.
//!
//! ```bash
//! DATABASE_URL=postgres://uops:uops@localhost:5432/uops cargo test -p uops-store-pg
//! ```

use chrono::Utc;
use uops_core::{
    ActorId, DecisionId, Identifier, IdentifierKind as K, Match, ObservedIdentity, Resolution,
    ResourceId, ResourceKind, TenantId,
};
use uops_identity::{Decision, DecisionOutcome, IdentityStore, Resolver};
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
    let org = uuid::Uuid::now_v7();
    let id = TenantId::new();

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org)
        .bind(format!("id-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(id.into_uuid())
        .bind(org)
        .bind(format!("id-{slug}"))
        .bind(format!("{slug}-{}", id.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");
    id
}

/// Where an alias points, read straight from the table the query layer expands through.
async fn alias_of(store: &PgStore, tenant: TenantId, historical: ResourceId) -> Option<ResourceId> {
    sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT current_id FROM resource_alias WHERE tenant_id = $1 AND historical_id = $2",
    )
    .bind(tenant.into_uuid())
    .bind(historical.into_uuid())
    .fetch_optional(store.pool())
    .await
    .expect("alias")
    .map(ResourceId::from_uuid)
}

async fn decision_count(store: &PgStore, tenant: TenantId, outcome: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM identity_decision WHERE tenant_id = $1 AND outcome = $2",
    )
    .bind(tenant.into_uuid())
    .bind(outcome)
    .fetch_one(store.pool())
    .await
    .expect("count")
}

fn landed_on(r: &Resolution) -> ResourceId {
    match r {
        Resolution::Matched { resource_id, .. } | Resolution::Created { resource_id } => {
            *resource_id
        }
        Resolution::Review { provisional_id, .. } => *provisional_id,
    }
}

#[tokio::test]
async fn lookup_uses_the_enum_array_and_finds_what_was_attached() {
    // The binding that could not be verified without a server: a Vec<IdentifierKind>
    // reaching PostgreSQL as identifier_kind[], so the join can use
    // UNIQUE (tenant_id, kind, value) rather than casting every row to text.
    let store = store().await;
    let t = tenant(&store, "lookup").await;

    let r = store
        .create_provisional(t, ResourceKind::Device, "rtr-01")
        .await
        .unwrap();
    let identifiers = vec![
        Identifier::new(K::Serial, "FTX1"),
        Identifier::new(K::Hostname, "rtr-01"),
    ];
    store
        .attach_identifiers(t, r, &identifiers, "snmp")
        .await
        .unwrap();

    let hits = store.lookup(t, &identifiers).await.unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.resource_id == r));

    // An empty set must not become a statement at all.
    assert!(store.lookup(t, &[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn attaching_twice_is_not_an_error_and_does_not_move_the_identifier() {
    // Ingestion repeats: the same host sends the same hostname thousands of times a
    // minute. Every one of those hits ON CONFLICT, and the row must stay where it is.
    let store = store().await;
    let t = tenant(&store, "attach").await;

    let first = store
        .create_provisional(t, ResourceKind::Device, "first")
        .await
        .unwrap();
    let second = store
        .create_provisional(t, ResourceKind::Device, "second")
        .await
        .unwrap();
    let ids = vec![Identifier::new(K::Hostname, "rtr-01")];

    store
        .attach_identifiers(t, first, &ids, "syslog")
        .await
        .unwrap();
    store
        .attach_identifiers(t, first, &ids, "syslog")
        .await
        .unwrap();
    store
        .attach_identifiers(t, second, &ids, "snmp")
        .await
        .unwrap();

    let hits = store.lookup(t, &ids).await.unwrap();
    assert_eq!(hits.len(), 1, "UNIQUE (tenant, kind, value) holds");
    assert_eq!(
        hits[0].resource_id, first,
        "attach must never take an identifier from its owner"
    );
}

#[tokio::test]
async fn reassigning_takes_it_because_the_hardware_was_replaced() {
    let store = store().await;
    let t = tenant(&store, "reassign").await;

    let old = store
        .create_provisional(t, ResourceKind::Device, "old")
        .await
        .unwrap();
    let new = store
        .create_provisional(t, ResourceKind::Device, "new")
        .await
        .unwrap();
    let ip = vec![Identifier::new(K::MgmtIp, "10.0.0.1")];

    store.attach_identifiers(t, old, &ip, "snmp").await.unwrap();
    store
        .reassign_identifiers(t, new, &ip, "snmp")
        .await
        .unwrap();

    assert_eq!(store.lookup(t, &ip).await.unwrap()[0].resource_id, new);
    assert!(
        store.identifiers_of(t, old).await.unwrap().is_empty(),
        "the predecessor no longer answers to that address"
    );
}

#[tokio::test]
async fn a_merge_moves_identifiers_and_writes_the_alias_together() {
    // Both halves or neither. A merge that moved the identifiers but failed before the
    // alias would orphan every row of telemetry recorded under the old resource_id —
    // the query layer expands through resource_alias and would find nothing.
    let store = store().await;
    let t = tenant(&store, "merge").await;

    let surviving = store
        .create_provisional(t, ResourceKind::Device, "surviving")
        .await
        .unwrap();
    let merged = store
        .create_provisional(t, ResourceKind::Device, "merged")
        .await
        .unwrap();
    let moved = vec![Identifier::new(K::Hostname, "rtr-99")];
    store
        .attach_identifiers(t, merged, &moved, "syslog")
        .await
        .unwrap();

    let decision = manual(t, surviving, &moved, DecisionOutcome::ManualMerge);
    store.merge(t, merged, surviving, &decision).await.unwrap();

    assert_eq!(
        store.lookup(t, &moved).await.unwrap()[0].resource_id,
        surviving
    );
    assert_eq!(alias_of(&store, t, merged).await, Some(surviving));
    assert_eq!(decision_count(&store, t, "manual_merge").await, 1);
}

#[tokio::test]
async fn merging_onto_an_already_merged_resource_collapses_the_chain() {
    // A merged into B, then B merged into C. The trigger in 0004_identity.sql rewrites
    // A→C on the way in, which is what keeps alias expansion a single lookup on the hot
    // path of every telemetry query rather than a recursive walk.
    let store = store().await;
    let t = tenant(&store, "chain").await;

    let a = store
        .create_provisional(t, ResourceKind::Device, "a")
        .await
        .unwrap();
    let b = store
        .create_provisional(t, ResourceKind::Device, "b")
        .await
        .unwrap();
    let c = store
        .create_provisional(t, ResourceKind::Device, "c")
        .await
        .unwrap();

    store
        .merge(t, a, b, &manual(t, b, &[], DecisionOutcome::ManualMerge))
        .await
        .unwrap();
    store
        .merge(t, b, c, &manual(t, c, &[], DecisionOutcome::ManualMerge))
        .await
        .unwrap();

    assert_eq!(alias_of(&store, t, b).await, Some(c));
    assert_eq!(
        alias_of(&store, t, a).await,
        Some(c),
        "A must follow B to C, not keep pointing at a resource that is gone"
    );
}

#[tokio::test]
async fn a_split_refuses_identifiers_that_are_not_on_the_resource() {
    // Silently moving one would take an identifier off an unrelated device. The whole
    // statement is in a transaction, so the refusal must also leave nothing behind.
    let store = store().await;
    let t = tenant(&store, "split-refuse").await;

    let from = store
        .create_provisional(t, ResourceKind::Device, "from")
        .await
        .unwrap();
    let elsewhere = store
        .create_provisional(t, ResourceKind::Device, "elsewhere")
        .await
        .unwrap();

    let mine = Identifier::new(K::Hostname, "mine");
    let theirs = Identifier::new(K::Hostname, "theirs");
    store
        .attach_identifiers(t, from, std::slice::from_ref(&mine), "syslog")
        .await
        .unwrap();
    store
        .attach_identifiers(t, elsewhere, std::slice::from_ref(&theirs), "syslog")
        .await
        .unwrap();

    let asked = vec![mine.clone(), theirs.clone()];
    let err = store
        .split(
            t,
            from,
            &asked,
            &manual(t, from, &asked, DecisionOutcome::ManualSplit),
        )
        .await
        .unwrap_err();
    assert_eq!(err.status_code(), 400, "{err}");

    // Nothing moved, and no orphan resource was left behind by the rolled-back insert.
    assert_eq!(store.lookup(t, &[mine]).await.unwrap()[0].resource_id, from);
    assert_eq!(
        store.lookup(t, &[theirs]).await.unwrap()[0].resource_id,
        elsewhere
    );
    assert_eq!(
        decision_count(&store, t, "manual_split").await,
        0,
        "a rolled-back split must not leave its decision behind"
    );
}

#[tokio::test]
async fn a_split_undoes_a_merge_including_the_alias() {
    let store = store().await;
    let t = tenant(&store, "split").await;

    let surviving = store
        .create_provisional(t, ResourceKind::Device, "surviving")
        .await
        .unwrap();
    let merged = store
        .create_provisional(t, ResourceKind::Device, "merged")
        .await
        .unwrap();
    let moved = vec![Identifier::new(K::Hostname, "rtr-99")];
    store
        .attach_identifiers(t, merged, &moved, "syslog")
        .await
        .unwrap();
    store
        .merge(
            t,
            merged,
            surviving,
            &manual(t, surviving, &moved, DecisionOutcome::ManualMerge),
        )
        .await
        .unwrap();

    let restored = store
        .split(
            t,
            surviving,
            &moved,
            &manual(t, surviving, &moved, DecisionOutcome::ManualSplit),
        )
        .await
        .unwrap();

    assert_eq!(
        store.lookup(t, &moved).await.unwrap()[0].resource_id,
        restored
    );
    assert_eq!(
        alias_of(&store, t, merged).await,
        None,
        "the alias must go, or history keeps resolving to the wrong side"
    );
}

#[tokio::test]
async fn review_deduplication_survives_the_round_trip_through_jsonb() {
    // The reason find_pending_review compares with @> in both directions. jsonb does not
    // preserve array order, and collectors do not promise one — a positional comparison
    // would miss the match and mint a provisional resource per message.
    let store = store().await;
    let t = tenant(&store, "dedup").await;

    let provisional = store
        .create_provisional(t, ResourceKind::Device, "provisional")
        .await
        .unwrap();
    let observed = vec![
        Identifier::new(K::MgmtIp, "10.0.0.1"),
        Identifier::new(K::Hostname, "rtr-01"),
    ];
    store
        .record_decision(&Decision {
            outcome: DecisionOutcome::Review,
            ..manual(t, provisional, &observed, DecisionOutcome::Review)
        })
        .await
        .unwrap();

    // The same question, asked with the identifiers in the other order.
    let reversed: Vec<Identifier> = observed.iter().rev().cloned().collect();
    let found = store.find_pending_review(t, &reversed).await.unwrap();
    assert_eq!(
        found.map(|r| r.provisional_id),
        Some(provisional),
        "identifier order must not change which question this is"
    );

    // A different question must not match it.
    let different = vec![Identifier::new(K::Hostname, "rtr-02")];
    assert!(
        store
            .find_pending_review(t, &different)
            .await
            .unwrap()
            .is_none()
    );

    // And a subset is not the same question either — containment in ONE direction would
    // have matched here, which is why both are required.
    let subset = vec![Identifier::new(K::Hostname, "rtr-01")];
    assert!(
        store
            .find_pending_review(t, &subset)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn an_answered_review_stops_being_pending() {
    let store = store().await;
    let t = tenant(&store, "answered").await;

    let provisional = store
        .create_provisional(t, ResourceKind::Device, "provisional")
        .await
        .unwrap();
    let surviving = store
        .create_provisional(t, ResourceKind::Device, "surviving")
        .await
        .unwrap();
    let observed = vec![Identifier::new(K::Hostname, "rtr-01")];

    store
        .record_decision(&manual(t, provisional, &observed, DecisionOutcome::Review))
        .await
        .unwrap();
    assert_eq!(store.pending_reviews(t, 50).await.unwrap().len(), 1);

    // A human merges the provisional away. The question has been answered.
    store
        .merge(
            t,
            provisional,
            surviving,
            &manual(t, provisional, &observed, DecisionOutcome::ManualMerge),
        )
        .await
        .unwrap();

    assert!(
        store.pending_reviews(t, 50).await.unwrap().is_empty(),
        "an answered review must leave the queue"
    );
    assert!(
        store
            .find_pending_review(t, &observed)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_resolver_drives_postgres_end_to_end() {
    // The whole stack on real storage: the rules from uops-core, the service from
    // uops-identity, the SQL from this crate, against the schema in migrations/.
    let store = store().await;
    let t = tenant(&store, "endtoend").await;
    let resolver = Resolver::new(store.clone());

    let snmp = ObservedIdentity::new("snmp")
        .with(K::Serial, "FTX1840ABCD")
        .with(K::MgmtIp, "10.9.9.1");
    let created = resolver.resolve(t, &snmp).await.unwrap();
    assert!(matches!(created, Resolution::Created { .. }));

    let trap = ObservedIdentity::new("snmp_trap")
        .with(K::Serial, "FTX1840ABCD")
        .with(K::Hostname, "rtr-01");
    let matched = resolver.resolve(t, &trap).await.unwrap();

    assert!(matches!(matched, Resolution::Matched { .. }), "{matched:?}");
    assert_eq!(landed_on(&created), landed_on(&matched));

    // Three identifiers now describe one device, and both decisions are on record.
    assert_eq!(
        store
            .identifiers_of(t, landed_on(&created))
            .await
            .unwrap()
            .len(),
        3
    );
    assert_eq!(decision_count(&store, t, "new").await, 1);
    assert_eq!(decision_count(&store, t, "auto_merge").await, 1);
}

#[tokio::test]
async fn resolution_does_not_cross_tenants_against_real_storage() {
    // Two customers, each with an rtr-01. The in-memory suite asserts this too; it is
    // repeated here because the thing being tested is the WHERE clause, not the rule.
    let store = store().await;
    let mine = tenant(&store, "iso-mine").await;
    let theirs = tenant(&store, "iso-theirs").await;
    let resolver = Resolver::new(store.clone());

    let observed = ObservedIdentity::new("syslog").with(K::Hostname, "rtr-01");
    let ours = landed_on(&resolver.resolve(mine, &observed).await.unwrap());
    let yours = landed_on(&resolver.resolve(theirs, &observed).await.unwrap());

    assert_ne!(ours, yours);
    assert!(
        store
            .lookup(theirs, &[Identifier::new(K::Hostname, "rtr-01")])
            .await
            .unwrap()
            .iter()
            .all(|h| h.resource_id == yours),
        "one tenant's lookup must never return another's resource"
    );
}

/// A decision as a human action would record it.
fn manual(
    tenant: TenantId,
    resource: ResourceId,
    observed: &[Identifier],
    outcome: DecisionOutcome,
) -> Decision {
    Decision {
        id: DecisionId::new(),
        tenant_id: tenant,
        resource_id: Some(resource),
        outcome,
        confidence: 1.0,
        matched_by: Vec::<Match>::new(),
        observed: observed.to_vec(),
        source: "integration-test".into(),
        actor_id: Some(ActorId::new()),
        decided_at: Utc::now(),
    }
}
