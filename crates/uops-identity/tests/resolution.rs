//! The resolution rules, end to end against the in-memory store.
//!
//! These are the highest-leverage assertions in the product. Everything downstream —
//! correlation, blast radius, every dashboard that says "this device" — is worthless if
//! two telemetry streams from one machine land on two resources, or one stream from two
//! machines lands on one.
//!
//! No database: the rules are what is under test, and the `PostgreSQL` binding is tested
//! separately for the only thing it can independently get wrong, which is whether its
//! SQL agrees with the schema.

use uops_core::{
    ActorId, Identifier, IdentifierKind as K, ObservedIdentity, Resolution, ResourceId,
    ResourceKind, TenantId,
};
use uops_identity::{DecisionOutcome, IdentityStore, MemoryIdentityStore, Resolver};

fn resolver() -> (Resolver<MemoryIdentityStore>, TenantId) {
    (
        Resolver::new(MemoryIdentityStore::default()),
        TenantId::new(),
    )
}

/// Whichever resource this resolution attached telemetry to.
fn landed_on(r: &Resolution) -> ResourceId {
    match r {
        Resolution::Matched { resource_id, .. } | Resolution::Created { resource_id } => {
            *resource_id
        }
        Resolution::Review { provisional_id, .. } => *provisional_id,
    }
}

#[tokio::test]
async fn a_shared_tier_one_identifier_joins_two_sources_automatically() {
    // The product's premise, on the path that needs no human: SNMP reports a serial,
    // and anything else reporting that serial is the same box by definition. Tier-1
    // identifiers are globally unique by specification, which is what earns them 1.00
    // and what makes this an auto-merge rather than a question.
    let (resolver, tenant) = resolver();

    let snmp = ObservedIdentity::new("snmp")
        .with(K::Serial, "FTX1840ABCD")
        .with(K::MgmtIp, "10.0.0.1");
    let first = resolver.resolve(tenant, &snmp).await.unwrap();
    assert!(matches!(first, Resolution::Created { .. }));

    // A trap from the same device, carrying the serial and a hostname nobody has seen.
    let trap = ObservedIdentity::new("snmp_trap")
        .with(K::Serial, "FTX1840ABCD")
        .with(K::Hostname, "rtr-01");
    let second = resolver.resolve(tenant, &trap).await.unwrap();

    assert!(
        matches!(second, Resolution::Matched { .. }),
        "a matching tier-1 serial is decisive: {second:?}"
    );
    assert_eq!(landed_on(&first), landed_on(&second));
    assert_eq!(resolver.store().resource_count(), 1, "one device, not two");

    // And the resource has learned the hostname it did not previously know.
    let known = resolver
        .store()
        .identifiers_of(tenant, landed_on(&first))
        .await
        .unwrap();
    assert_eq!(known.len(), 3, "the new identifier joins: {known:?}");
}

#[tokio::test]
async fn two_sources_with_only_a_weak_identifier_in_common_need_a_human() {
    // The M1 acceptance criterion in full: two sources, a review-queue item in the
    // 0.60-0.95 band, then a merge that makes them one resource.
    //
    // This is the common shape and it is worth being plain about: syslog discovers a
    // device by hostname, SNMP finds it later, and the only thing they share is that
    // hostname — 0.65. SPEC chose deliberately not to auto-merge on that, because a
    // hostname can be reused by a replacement box, and a wrong merge is invisible while
    // a queue item costs ten seconds.
    let (resolver, tenant) = resolver();
    let actor = ActorId::new();

    let syslog = ObservedIdentity::new("syslog").with(K::Hostname, "rtr-01");
    let from_syslog = landed_on(&resolver.resolve(tenant, &syslog).await.unwrap());

    let snmp = ObservedIdentity::new("snmp")
        .with(K::Hostname, "rtr-01")
        .with(K::Serial, "FTX1840ABCD");
    let outcome = resolver.resolve(tenant, &snmp).await.unwrap();

    let Resolution::Review {
        provisional_id,
        candidates,
    } = outcome
    else {
        panic!("a lone hostname match is 0.65 — not enough to merge on")
    };
    assert_eq!(candidates[0].resource_id, from_syslog);

    // The queue has exactly one question in it.
    let queue = resolver.reviews(tenant, 50).await.unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].provisional_id, provisional_id);

    // A human answers it, and the two sources become one resource.
    resolver
        .merge(tenant, provisional_id, from_syslog, actor, "same device")
        .await
        .unwrap();

    assert_eq!(
        resolver.store().alias_of(tenant, provisional_id),
        Some(from_syslog),
        "telemetry written to the provisional must still resolve after the merge"
    );
    let joined = resolver
        .store()
        .identifiers_of(tenant, from_syslog)
        .await
        .unwrap();
    assert_eq!(
        joined.len(),
        2,
        "hostname and serial now describe one device"
    );
}

#[tokio::test]
async fn evidence_in_the_review_band_does_not_auto_merge() {
    // SPEC's own worked example: mgmt_ip (0.80) + hostname (0.65) → 0.93, below the 0.95
    // bar. A wrong auto-merge corrupts every correlation downstream and is nearly
    // invisible; a queue item costs someone ten seconds.
    let (resolver, tenant) = resolver();

    let known = ObservedIdentity::new("snmp")
        .with(K::MgmtIp, "10.0.0.1")
        .with(K::Hostname, "rtr-01");
    let original = landed_on(&resolver.resolve(tenant, &known).await.unwrap());

    // The same address and name again — possibly the same box, possibly a replacement
    // that inherited both.
    let again = ObservedIdentity::new("syslog")
        .with(K::MgmtIp, "10.0.0.1")
        .with(K::Hostname, "rtr-01");
    let outcome = resolver.resolve(tenant, &again).await.unwrap();

    let Resolution::Review {
        provisional_id,
        candidates,
    } = outcome
    else {
        panic!("0.93 must not auto-merge")
    };
    assert_ne!(provisional_id, original);
    assert_eq!(candidates[0].resource_id, original);
    assert!(
        (candidates[0].confidence - 0.93).abs() < 0.01,
        "noisy-OR of 0.80 and 0.65 is 0.93, got {}",
        candidates[0].confidence
    );

    // The matched identifiers stay with the original. Moving them would enact the merge
    // this review exists to ask about — and UNIQUE (tenant, kind, value) would refuse.
    let still = resolver
        .store()
        .identifiers_of(tenant, original)
        .await
        .unwrap();
    assert_eq!(still.len(), 2, "the original keeps what it had");
}

#[tokio::test]
async fn a_repeated_review_reuses_its_provisional_resource() {
    // Found by implementing it. SPEC describes the outcome bands but not what happens on
    // the second identical observation, and a device sends thousands an hour: without
    // this, one unanswered question mints a provisional resource per message.
    let (resolver, tenant) = resolver();

    let known = ObservedIdentity::new("snmp")
        .with(K::MgmtIp, "10.0.0.1")
        .with(K::Hostname, "rtr-01");
    resolver.resolve(tenant, &known).await.unwrap();

    let repeat = ObservedIdentity::new("syslog")
        .with(K::MgmtIp, "10.0.0.1")
        .with(K::Hostname, "rtr-01");

    let first = landed_on(&resolver.resolve(tenant, &repeat).await.unwrap());
    for _ in 0..20 {
        assert_eq!(
            landed_on(&resolver.resolve(tenant, &repeat).await.unwrap()),
            first,
            "telemetry must keep landing on the same provisional resource"
        );
    }

    assert_eq!(
        resolver.store().resource_count(),
        2,
        "one original, one provisional — not twenty-two"
    );
    assert_eq!(resolver.reviews(tenant, 50).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_tier_one_contradiction_creates_new_hardware() {
    // Same management address, different serial: the box was physically replaced.
    // Inheriting its predecessor's history would corrupt every trend attached to it.
    let (resolver, tenant) = resolver();

    let old = ObservedIdentity::new("snmp")
        .with(K::MgmtIp, "10.0.0.1")
        .with(K::Serial, "OLD-SERIAL");
    let old_id = landed_on(&resolver.resolve(tenant, &old).await.unwrap());

    let replacement = ObservedIdentity::new("snmp")
        .with(K::MgmtIp, "10.0.0.1")
        .with(K::Serial, "NEW-SERIAL");
    let outcome = resolver.resolve(tenant, &replacement).await.unwrap();

    let Resolution::Created { resource_id } = outcome else {
        panic!("a tier-1 contradiction is decisive, whatever else agrees")
    };
    assert_ne!(resource_id, old_id);

    // The management address now describes the new box, so it moves with the hardware.
    let hits = resolver
        .store()
        .lookup(tenant, &[Identifier::new(K::MgmtIp, "10.0.0.1")])
        .await
        .unwrap();
    assert_eq!(
        hits[0].resource_id, resource_id,
        "the IP follows the hardware"
    );

    // The predecessor keeps its serial, and therefore its history.
    let previous = resolver
        .store()
        .identifiers_of(tenant, old_id)
        .await
        .unwrap();
    assert!(previous.iter().any(|i| i.value == "OLD-SERIAL"));
}

#[tokio::test]
async fn an_envelope_with_no_identity_is_kept_not_dropped() {
    // Telemetry lost during an incident is the worst failure this system has, and "we
    // could not tell whose it was" is not a reason to lose it.
    let (resolver, tenant) = resolver();

    let outcome = resolver
        .resolve(tenant, &ObservedIdentity::new("otlp"))
        .await
        .unwrap();

    assert!(matches!(outcome, Resolution::Created { .. }), "{outcome:?}");
    assert_eq!(resolver.store().resource_count(), 1);
}

#[tokio::test]
async fn a_warm_resolver_does_not_touch_the_store() {
    // SPEC states the requirement as a number — >99% hit rate at 50k msg/s. This is that
    // number asserted rather than hoped for.
    let (resolver, tenant) = resolver();
    let observed = ObservedIdentity::new("snmp")
        .with(K::Serial, "FTX1")
        .with(K::Hostname, "rtr-01");

    resolver.resolve(tenant, &observed).await.unwrap(); // creates
    resolver.resolve(tenant, &observed).await.unwrap(); // matches, and caches
    let after_warmup = resolver.store().lookup_count();

    for _ in 0..1_000 {
        resolver.resolve(tenant, &observed).await.unwrap();
    }

    assert_eq!(
        resolver.store().lookup_count(),
        after_warmup,
        "a thousand repeats must be a thousand cache hits"
    );
    assert!(resolver.cache().stats().hit_rate().unwrap() > 0.99);
}

#[tokio::test]
async fn every_decision_is_recorded() {
    // What SPEC calls the difference between debuggable and mystifying. Six months after
    // two switches became one, this is the only account of why.
    let (resolver, tenant) = resolver();
    let observed = ObservedIdentity::new("snmp").with(K::Serial, "FTX1");

    resolver.resolve(tenant, &observed).await.unwrap();
    resolver.resolve(tenant, &observed).await.unwrap();

    let decisions = resolver.store().decisions();
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[0].outcome, DecisionOutcome::New);
    assert_eq!(decisions[1].outcome, DecisionOutcome::AutoMerge);
    assert_eq!(decisions[1].source, "snmp");
    assert!(
        !decisions[1].observed.is_empty(),
        "what was presented must be recorded, not only what matched"
    );
}

#[tokio::test]
async fn a_merge_is_reversible_by_a_split() {
    // SPEC §M0.2: "Merge is reversible." The merge records the identifiers it moved,
    // which IS the pre-merge partition — without it a split would have to guess which
    // identifiers came from which side.
    let (resolver, tenant) = resolver();
    let actor = ActorId::new();

    let surviving = landed_on(
        &resolver
            .resolve(
                tenant,
                &ObservedIdentity::new("snmp").with(K::Serial, "AAA"),
            )
            .await
            .unwrap(),
    );
    let merged_away = landed_on(
        &resolver
            .resolve(
                tenant,
                &ObservedIdentity::new("syslog").with(K::Hostname, "rtr-99"),
            )
            .await
            .unwrap(),
    );

    let merge = resolver
        .merge(tenant, merged_away, surviving, actor, "same device")
        .await
        .unwrap();

    assert_eq!(merge.outcome, DecisionOutcome::ManualMerge);
    assert_eq!(
        resolver.store().alias_of(tenant, merged_away),
        Some(surviving),
        "telemetry written under the old id must still resolve"
    );
    assert_eq!(
        resolver
            .store()
            .identifiers_of(tenant, surviving)
            .await
            .unwrap()
            .len(),
        2
    );

    // And the split puts it back, from what the merge recorded.
    let (restored, split) = resolver
        .split(tenant, surviving, &merge.observed, actor, "not the same")
        .await
        .unwrap();

    assert_eq!(split.outcome, DecisionOutcome::ManualSplit);
    assert_ne!(restored, surviving);
    let hits = resolver
        .store()
        .lookup(tenant, &[Identifier::new(K::Hostname, "rtr-99")])
        .await
        .unwrap();
    assert_eq!(hits[0].resource_id, restored);
    assert!(
        resolver.store().alias_of(tenant, merged_away).is_none(),
        "the alias must go, or history keeps resolving to the wrong side"
    );
}

#[tokio::test]
async fn a_merge_invalidates_the_cache() {
    // Otherwise the merged-away resource keeps receiving telemetry for as long as its
    // cache entries survive — and that telemetry is what keeps refreshing them.
    let (resolver, tenant) = resolver();
    let observed = ObservedIdentity::new("snmp")
        .with(K::Serial, "FTX1")
        .with(K::Hostname, "rtr-01");

    let merged_away = landed_on(&resolver.resolve(tenant, &observed).await.unwrap());
    resolver.resolve(tenant, &observed).await.unwrap(); // warm the cache

    let surviving = resolver
        .store()
        .create_provisional(tenant, ResourceKind::Device, "survivor")
        .await
        .unwrap();
    resolver
        .merge(tenant, merged_away, surviving, ActorId::new(), "duplicate")
        .await
        .unwrap();

    assert_eq!(
        landed_on(&resolver.resolve(tenant, &observed).await.unwrap()),
        surviving,
        "resolution must follow the merge, not a stale cache entry"
    );
}

#[tokio::test]
async fn merging_a_resource_into_itself_is_refused() {
    let (resolver, tenant) = resolver();
    let r = ResourceId::new();
    assert!(
        resolver
            .merge(tenant, r, r, ActorId::new(), "oops")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn resolution_does_not_cross_tenants() {
    // Two customers each have an rtr-01, and most will. If this fails, one customer's
    // telemetry lands on another customer's device.
    let (resolver, mine) = resolver();
    let theirs = TenantId::new();
    let observed = ObservedIdentity::new("syslog").with(K::Hostname, "rtr-01");

    let ours = landed_on(&resolver.resolve(mine, &observed).await.unwrap());
    let yours = landed_on(&resolver.resolve(theirs, &observed).await.unwrap());

    assert_ne!(ours, yours);
    assert_eq!(resolver.store().resource_count(), 2);
}
