//! SPEC §M1 acceptance, as one story:
//!
//! > Identity resolution resolves two sources to one resource; the review queue
//! > receives a 0.60–0.95 case; a merge is performed and then reverted by a split.
//!
//! Every piece of this is already tested. The rules are proven exhaustively in
//! `uops-identity` against an in-memory store; the SQL is proven in `identity.rs` next
//! to this file. What neither of them does is run the sequence — and the sequence is
//! the product. A resolver whose rules are right, whose statements are right, and whose
//! *order of operations* loses a resource somewhere in the middle would pass both
//! existing suites.
//!
//! So this is deliberately one test rather than four. Each step depends on the state the
//! previous one left, which is the only way to catch a merge that works in isolation and
//! leaves the review queue pointing at a resource that no longer owns its identifiers.
//!
//! The scenario is the ordinary one: an SNMP poller and a syslog receiver see the same
//! switch, a third source sees something that might be it, and an operator decides —
//! then changes their mind.

use uops_core::{
    ActorId, Identifier, IdentifierKind as K, ObservedIdentity, Resolution, ResourceId, TenantId,
};
use uops_identity::{DecisionOutcome, IdentityStore, Resolver};
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
        .bind(format!("scn-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(id.into_uuid())
        .bind(org)
        .bind(format!("scn-{slug}"))
        .bind(format!("{slug}-{}", id.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");
    id
}

/// What a collector reports.
fn observed(source: &str, identifiers: Vec<Identifier>) -> ObservedIdentity {
    ObservedIdentity {
        identifiers,
        source: source.to_owned(),
        site_hint: None,
    }
}

/// Every decision recorded for a tenant, oldest first.
///
/// Read with SQL rather than through the store, because the point is what an auditor
/// finds in the table a year later — not what the code that wrote it believes it wrote.
async fn decision_log(store: &PgStore, tenant: TenantId) -> Vec<(String, Option<ResourceId>)> {
    sqlx::query_as::<_, (String, Option<uuid::Uuid>)>(
        "SELECT outcome, resource_id FROM identity_decision
          WHERE tenant_id = $1 ORDER BY decided_at, id",
    )
    .bind(tenant.into_uuid())
    .fetch_all(store.pool())
    .await
    .expect("decision log")
    .into_iter()
    .map(|(outcome, id)| (outcome, id.map(ResourceId::from_uuid)))
    .collect()
}

/// Which resource an identifier currently points at, if any.
async fn owner_of(store: &PgStore, tenant: TenantId, id: &Identifier) -> Option<ResourceId> {
    let hits = store
        .lookup(tenant, std::slice::from_ref(id))
        .await
        .expect("lookup");
    hits.first().map(|h| h.resource_id)
}

// One long function on purpose. Every step depends on the state the previous one left,
// which is the whole point — splitting it into four tests would give four tests that
// pass while the sequence is broken, which is exactly the failure this file exists to
// catch. It found one: see the review-queue assertion below.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn two_sources_one_resource_then_a_review_a_merge_and_a_split() {
    let store = store().await;
    let tenant = tenant(&store, "full").await;
    let resolver = Resolver::new(store.clone());
    let operator = ActorId::new();

    // ---------------------------------------------------------------------------
    // 1. The SNMP poller finds a switch. Nothing is known yet, so a resource is born.
    // ---------------------------------------------------------------------------
    let serial = Identifier::new(K::Serial, "FOC1943X0AB");
    let mac = Identifier::new(K::Mac, "00:1b:0d:63:c2:26");
    let mgmt_ip = Identifier::new(K::MgmtIp, "10.20.0.1");
    let hostname = Identifier::new(K::Hostname, "core-sw-01");

    let first = resolver
        .resolve(
            tenant,
            &observed(
                "snmp",
                vec![
                    serial.clone(),
                    mac.clone(),
                    mgmt_ip.clone(),
                    hostname.clone(),
                ],
            ),
        )
        .await
        .expect("first resolve");

    let switch = match first {
        Resolution::Created { resource_id } => resource_id,
        other => panic!("an empty tenant must create, not match: {other:?}"),
    };

    // ---------------------------------------------------------------------------
    // 2. A flow exporter sees the same box and knows none of what SNMP knows except
    //    its MAC and its address. Neither is tier 1 — a NIC moves between chassis and
    //    DHCP reassigns addresses — but together they are 0.98 under noisy-OR, over
    //    the 0.95 auto-merge bar. Two sources, one resource, and no human involved.
    // ---------------------------------------------------------------------------
    let second = resolver
        .resolve(
            tenant,
            &observed("netflow", vec![mac.clone(), mgmt_ip.clone()]),
        )
        .await
        .expect("second resolve");

    match second {
        Resolution::Matched {
            resource_id,
            confidence,
            ..
        } => {
            assert_eq!(
                resource_id, switch,
                "the flow exporter must land on the resource SNMP created, not beside it"
            );
            assert!(
                confidence >= 0.95,
                "an auto-merge below the bar would mean the bands moved: {confidence}"
            );
        }
        other => panic!("two agreeing identifiers over 0.95 must auto-merge: {other:?}"),
    }

    // ---------------------------------------------------------------------------
    // 3. A log shipper reports the hostname it was configured with, and its own
    //    service name. The hostname matches the switch at 0.65 — inside the
    //    0.60–0.95 band — so this is a question for a human, not a decision.
    // ---------------------------------------------------------------------------
    let service = Identifier::new(K::ServiceName, "filebeat");

    let third = resolver
        .resolve(
            tenant,
            &observed("otel", vec![hostname.clone(), service.clone()]),
        )
        .await
        .expect("third resolve");

    let (provisional, candidates) = match third {
        Resolution::Review {
            provisional_id,
            candidates,
        } => (provisional_id, candidates),
        other => panic!("a hostname match alone must go to review, not decide: {other:?}"),
    };
    assert_eq!(
        candidates.first().map(|c| c.resource_id),
        Some(switch),
        "the reviewer has to be shown what it might be: {candidates:?}"
    );
    assert_ne!(
        provisional, switch,
        "an uncertain match must not silently attach to an existing resource"
    );

    // The subtle one, and the reason a review is not just a deferred merge: the
    // identifier that *matched* stays where it is. Moving it to the provisional would
    // enact the very merge this review exists to ask about — and UNIQUE (tenant_id,
    // kind, value) makes that a constraint rather than a preference.
    assert_eq!(
        owner_of(&store, tenant, &hostname).await,
        Some(switch),
        "a review must not move the identifier it is asking about"
    );

    // What did not match goes to the provisional, so the shipper's telemetry has
    // somewhere to land. Rule 1: a collector is never told to wait for a human.
    assert_eq!(
        owner_of(&store, tenant, &service).await,
        Some(provisional),
        "the unmatched identifier must land on the provisional resource"
    );

    // And the queue has it, which is how the operator ever finds out.
    let queue = store
        .pending_reviews(tenant, 50)
        .await
        .expect("pending reviews");
    assert!(
        queue.iter().any(|r| r.provisional_id == provisional),
        "the review queue must contain the case that was just deferred"
    );

    // ---------------------------------------------------------------------------
    // 4. The operator looks at the queue and decides they are the same switch.
    // ---------------------------------------------------------------------------
    let merge = resolver
        .merge(
            tenant,
            provisional,
            switch,
            operator,
            "same host, the shipper runs on it",
        )
        .await
        .expect("merge");
    assert_eq!(merge.outcome, DecisionOutcome::ManualMerge);
    assert_eq!(merge.actor_id, Some(operator));

    // The provisional's identifier moved onto the surviving resource.
    assert_eq!(
        owner_of(&store, tenant, &service).await,
        Some(switch),
        "a merge must move the identifiers, or the next message resolves to the old row"
    );

    // And the telemetry already written under the provisional id still resolves — an
    // alias, not a rewrite. A merge that rewrote history would be O(rows) and would
    // also be a lie about what was observed.
    let alias: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT current_id FROM resource_alias WHERE tenant_id = $1 AND historical_id = $2",
    )
    .bind(tenant.into_uuid())
    .bind(provisional.into_uuid())
    .fetch_optional(store.pool())
    .await
    .expect("alias");
    assert_eq!(
        alias.map(ResourceId::from_uuid),
        Some(switch),
        "a merge must leave an alias, or telemetry written before it becomes unreachable"
    );

    // And the queue no longer offers it. This is the assertion that only a sequence
    // can make: every step above is correct in isolation, and an operator who answers
    // a question and is asked it again tomorrow has a product that does not work.
    let queue = store
        .pending_reviews(tenant, 50)
        .await
        .expect("pending reviews");
    assert!(
        !queue.iter().any(|r| r.provisional_id == provisional),
        "a merged case must leave the review queue: {} item(s) still pending",
        queue.len()
    );

    // ---------------------------------------------------------------------------
    // 5. They were wrong. A split moves the identifier back out onto its own resource.
    //    SPEC's word is "reverted": the operator must be able to undo their own call.
    // ---------------------------------------------------------------------------
    let (restored, split) = resolver
        .split(
            tenant,
            switch,
            std::slice::from_ref(&service),
            operator,
            "filebeat moved to another host",
        )
        .await
        .expect("split");
    assert_eq!(split.outcome, DecisionOutcome::ManualSplit);
    assert_ne!(restored, switch);

    assert_eq!(
        owner_of(&store, tenant, &service).await,
        Some(restored),
        "a split must move the identifier off the surviving resource"
    );
    // The switch keeps everything that was always its own.
    assert_eq!(owner_of(&store, tenant, &serial).await, Some(switch));
    assert_eq!(owner_of(&store, tenant, &mac).await, Some(switch));
    assert_eq!(owner_of(&store, tenant, &hostname).await, Some(switch));
    assert_eq!(owner_of(&store, tenant, &mgmt_ip).await, Some(switch));

    // ---------------------------------------------------------------------------
    // 6. The whole thing is on the record, in order.
    //
    // This is the assertion an auditor's question actually lands on: not "is the
    // current state right" but "can you show me how it got here". Every step above
    // wrote a row, including the two a human made, and each names who made it.
    // ---------------------------------------------------------------------------
    let log = decision_log(&store, tenant).await;
    let outcomes: Vec<&str> = log.iter().map(|(o, _)| o.as_str()).collect();

    assert!(
        outcomes.len() >= 5,
        "every resolution and every human decision must be recorded: {outcomes:?}"
    );
    assert_eq!(outcomes.first(), Some(&"new"), "{outcomes:?}");
    assert_eq!(
        outcomes.last(),
        Some(&"manual_split"),
        "the split must be the most recent word on this: {outcomes:?}"
    );
    assert!(
        outcomes.contains(&"manual_merge"),
        "the merge must still be in the log after being reverted — an undone decision \
         is still a decision somebody made: {outcomes:?}"
    );

    // Both human decisions name the human. A decision log that cannot answer "who"
    // is a log that answers nothing an auditor asked.
    let human: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM identity_decision
          WHERE tenant_id = $1 AND actor_id = $2
            AND outcome IN ('manual_merge', 'manual_split')",
    )
    .bind(tenant.into_uuid())
    .bind(operator.into_uuid())
    .fetch_one(store.pool())
    .await
    .expect("human decisions");
    assert_eq!(human, 2, "both manual decisions must name the operator");
}

#[tokio::test]
async fn a_tier_one_contradiction_beats_everything_that_agrees() {
    // The other half of "resolves two sources to one resource": knowing when not to.
    // Two boxes behind the same DHCP address with the same truncated hostname agree on
    // everything a name or an address can say — and their serials say they are
    // different hardware. SPEC §M0.2: a tier-1 contradiction is decisive, whatever the
    // combined confidence of the agreeing identifiers would otherwise be.
    let store = store().await;
    let tenant = tenant(&store, "contra").await;
    let resolver = Resolver::new(store.clone());

    let shared_ip = Identifier::new(K::MgmtIp, "10.30.0.9");
    let shared_name = Identifier::new(K::Hostname, "edge-fw");

    let first = resolver
        .resolve(
            tenant,
            &observed(
                "snmp",
                vec![
                    Identifier::new(K::Serial, "AAAA1111"),
                    shared_ip.clone(),
                    shared_name.clone(),
                ],
            ),
        )
        .await
        .expect("first");
    let a = match first {
        Resolution::Created { resource_id } => resource_id,
        other => panic!("{other:?}"),
    };

    let second = resolver
        .resolve(
            tenant,
            &observed(
                "snmp",
                vec![
                    Identifier::new(K::Serial, "BBBB2222"),
                    shared_ip.clone(),
                    shared_name.clone(),
                ],
            ),
        )
        .await
        .expect("second");

    match second {
        Resolution::Created { resource_id } => assert_ne!(
            resource_id, a,
            "two different serials are two different boxes, whatever else agrees"
        ),
        other => panic!("a tier-1 contradiction must create, not match or review: {other:?}"),
    }
}
