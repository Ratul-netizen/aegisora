//! The resolution cache.
//!
//! SPEC §M0.2 states the requirement as a number: *"A syslog receiver at 50k msg/s
//! cannot hit PostgreSQL per message. Target: >99% hit rate steady-state."* That is
//! achievable because the workload is extraordinarily repetitive — a device sends
//! thousands of messages an hour, every one of them carrying the same hostname.
//!
//! # What it is allowed to answer
//!
//! Only the unambiguous case: every observed identifier is cached and they all agree on
//! one resource. Anything else — one unknown identifier, two identifiers disagreeing —
//! goes to the store, because an identifier that matched nothing is new evidence and
//! attaching it is a decision.
//!
//! That restriction is what makes the cache safe. A cache that answered partial matches
//! would be deciding, and deciding is what the resolver does with the whole picture in
//! front of it.
//!
//! # Invalidation
//!
//! A merge or split changes which resource an identifier points at, so both invalidate.
//! Within one process that is a direct call. Across processes it needs the bus, and the
//! subscription belongs with the pipeline that owns both — noted here rather than built,
//! because there is exactly one process in v0.1 and a broadcast to nobody is theatre.

use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;
use uops_core::{Identifier, ResourceId, TenantId};

/// What a cached lookup concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cached {
    /// Every identifier agreed, with enough confidence to attach telemetry.
    Resolved(ResourceId),
    /// Not answerable here. Ask the store.
    Miss,
}

type Key = (TenantId, &'static str, String);

/// Counters for `/api/v1/health` and for proving the hit rate in a test rather than
/// asserting it in a comment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub invalidations: u64,
}

impl CacheStats {
    /// Hit rate as a fraction, or `None` before anything has been looked up.
    #[must_use]
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hits + self.misses;
        (total > 0).then(|| {
            #[allow(clippy::cast_precision_loss)]
            {
                self.hits as f64 / total as f64
            }
        })
    }
}

/// `(tenant, kind, value) → resource_id`, bounded.
pub struct ResolutionCache {
    entries: Mutex<LruCache<Key, ResourceId>>,
    stats: Mutex<CacheStats>,
}

impl std::fmt::Debug for ResolutionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolutionCache")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// Enough for a mid-sized estate's worth of identifiers without being a memory
/// surprise: roughly 100k entries at a few dozen bytes each.
pub const DEFAULT_CAPACITY: usize = 100_000;

impl Default for ResolutionCache {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl ResolutionCache {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = NonZeroUsize::new(capacity.max(1)).expect("non-zero");
        Self {
            entries: Mutex::new(LruCache::new(capacity)),
            stats: Mutex::new(CacheStats::default()),
        }
    }

    #[must_use]
    pub fn stats(&self) -> CacheStats {
        *self.lock_stats()
    }

    /// Answer only if every identifier is known and they all agree on one resource.
    ///
    /// That is the same rule `Resolver::resolve` applies on a cold cache — see
    /// `exclusive_match` — and it is deliberately **not** the confidence model. An
    /// identifier belongs to exactly one resource, so an observation whose every
    /// identifier is already attached to the same resource is that resource by prior
    /// assertion; the base confidences answer the different question of whether two
    /// independently discovered resources are the same box.
    ///
    /// An earlier version also required the combined confidence to clear the auto-merge
    /// bar. It made this cache answer nothing in the case it exists for: a syslog
    /// message carries a hostname and an address, which combine to 0.93, so every
    /// message missed and went to `PostgreSQL`.
    #[must_use]
    pub fn lookup(&self, tenant: TenantId, identifiers: &[Identifier]) -> Cached {
        if identifiers.is_empty() {
            self.lock_stats().misses += 1;
            return Cached::Miss;
        }

        let mut resolved: Option<ResourceId> = None;

        {
            let mut entries = self.lock_entries();
            for identifier in identifiers {
                let Some(found) = entries.get(&key(tenant, identifier)).copied() else {
                    self.lock_stats().misses += 1;
                    return Cached::Miss;
                };
                // Two known identifiers pointing at different resources is exactly the
                // ambiguity the resolver exists to adjudicate. The cache must not pick.
                if resolved.is_some_and(|already| already != found) {
                    self.lock_stats().misses += 1;
                    return Cached::Miss;
                }
                resolved = Some(found);
            }
        }

        self.lock_stats().hits += 1;
        resolved.map_or(Cached::Miss, Cached::Resolved)
    }

    /// Record what the store decided, so the next identical message does not ask again.
    pub fn remember(&self, tenant: TenantId, identifiers: &[Identifier], resource: ResourceId) {
        let mut entries = self.lock_entries();
        for identifier in identifiers {
            entries.put(key(tenant, identifier), resource);
        }
    }

    /// Forget everything pointing at a resource.
    ///
    /// Called on merge and split. Scans rather than indexing by resource: merges are
    /// rare, the cache is bounded, and a reverse index would have to be maintained on
    /// the hot path to save work on the cold one.
    pub fn invalidate_resource(&self, tenant: TenantId, resource: ResourceId) {
        let mut entries = self.lock_entries();
        let stale: Vec<Key> = entries
            .iter()
            .filter(|(k, v)| k.0 == tenant && **v == resource)
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            entries.pop(&k);
        }
        drop(entries);
        self.lock_stats().invalidations += 1;
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.lock_entries().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A poisoned lock means another thread panicked while holding it. The contents are
    /// a cache — there is no invariant a panic could have left half-applied — and
    /// refusing to resolve telemetry because of it would turn one panic into an ingest
    /// outage. Same reasoning as the access log in `uops-secrets`.
    fn lock_entries(&self) -> std::sync::MutexGuard<'_, LruCache<Key, ResourceId>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_stats(&self) -> std::sync::MutexGuard<'_, CacheStats> {
        self.stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn key(tenant: TenantId, identifier: &Identifier) -> Key {
    (tenant, identifier.kind.as_str(), identifier.value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uops_core::IdentifierKind;

    fn ident(kind: IdentifierKind, value: &str) -> Identifier {
        Identifier::new(kind, value)
    }

    #[test]
    fn a_strong_unambiguous_set_is_answered_without_the_store() {
        // The steady-state path: the same device's syslog, thousands of times an hour.
        let cache = ResolutionCache::default();
        let tenant = TenantId::new();
        let resource = ResourceId::new();
        let observed = vec![ident(IdentifierKind::Serial, "FTX1")];

        assert_eq!(cache.lookup(tenant, &observed), Cached::Miss);
        cache.remember(tenant, &observed, resource);
        assert_eq!(cache.lookup(tenant, &observed), Cached::Resolved(resource));
    }

    #[test]
    fn a_single_weak_identifier_is_answered_once_it_is_known() {
        // This test asserted the opposite until 2026-09-16, and the opposite was wrong.
        //
        // The old rule was that a hostname alone is 0.65, so the cache must refuse it and
        // apply the same bar the resolver would. That reasoning confuses two different
        // questions. The base confidences say how much sharing an identifier proves that
        // two *independently discovered* resources are the same box. An entry in this
        // cache is not that: it is `remember` recording that this identifier already
        // belongs to that resource, which is `UNIQUE (tenant_id, kind, value)` — one
        // resource, by assertion, not by inference.
        //
        // Applying the bar here made the cache answer nothing in the case it exists for.
        // A syslog message carries a hostname and a source address, which combine to
        // 0.93, under 0.95 — so every message missed, went to PostgreSQL, matched its own
        // resource at 0.93, and was filed as a review. Every device in the estate became
        // a queue item by talking to us twice. See `resolver::exclusive_match`.
        let cache = ResolutionCache::default();
        let tenant = TenantId::new();
        let observed = vec![ident(IdentifierKind::Hostname, "rtr-01")];
        let resource = ResourceId::new();

        cache.remember(tenant, &observed, resource);
        assert_eq!(cache.lookup(tenant, &observed), Cached::Resolved(resource));
    }

    #[test]
    fn an_identifier_nobody_has_seen_still_misses() {
        // The rule that did not change, and the one that matters: an identifier which
        // matched nothing is new evidence, and attaching it is a decision the resolver
        // makes with the whole picture. A cache that guessed here would be deciding.
        let cache = ResolutionCache::default();
        let tenant = TenantId::new();
        let resource = ResourceId::new();

        cache.remember(
            tenant,
            &[ident(IdentifierKind::Hostname, "rtr-01")],
            resource,
        );

        // The hostname is known; the address is not. Not answerable here.
        let with_new_evidence = vec![
            ident(IdentifierKind::Hostname, "rtr-01"),
            ident(IdentifierKind::MgmtIp, "10.0.0.1"),
        ];
        assert_eq!(cache.lookup(tenant, &with_new_evidence), Cached::Miss);
    }

    #[test]
    fn disagreement_goes_to_the_store_rather_than_being_picked() {
        // Two known identifiers pointing at different resources is the ambiguity the
        // resolver exists to adjudicate, with the full picture. A cache that chose one
        // would be making that decision with less information and no audit record.
        let cache = ResolutionCache::default();
        let tenant = TenantId::new();

        cache.remember(
            tenant,
            &[ident(IdentifierKind::Serial, "FTX1")],
            ResourceId::new(),
        );
        cache.remember(
            tenant,
            &[ident(IdentifierKind::ChassisId, "AA:BB")],
            ResourceId::new(),
        );

        let both = vec![
            ident(IdentifierKind::Serial, "FTX1"),
            ident(IdentifierKind::ChassisId, "AA:BB"),
        ];
        assert_eq!(cache.lookup(tenant, &both), Cached::Miss);
    }

    #[test]
    fn one_unknown_identifier_sends_the_whole_set_to_the_store() {
        // Otherwise a new serial appearing on a known host would be silently ignored
        // rather than recorded — and the serial is the strongest evidence there is.
        let cache = ResolutionCache::default();
        let tenant = TenantId::new();
        let resource = ResourceId::new();

        cache.remember(tenant, &[ident(IdentifierKind::Serial, "FTX1")], resource);

        let with_new = vec![
            ident(IdentifierKind::Serial, "FTX1"),
            ident(IdentifierKind::Mac, "00:11:22:33:44:55"),
        ];
        assert_eq!(cache.lookup(tenant, &with_new), Cached::Miss);
    }

    #[test]
    fn tenants_do_not_share_entries() {
        // Two customers each have an rtr-01, and most will. The tenant is part of the
        // key for the same reason it is part of every UNIQUE constraint in the schema.
        let cache = ResolutionCache::default();
        let a = TenantId::new();
        let b = TenantId::new();
        let observed = vec![ident(IdentifierKind::Serial, "FTX1")];
        let theirs = ResourceId::new();

        cache.remember(a, &observed, theirs);
        assert_eq!(cache.lookup(b, &observed), Cached::Miss);
        assert_eq!(cache.lookup(a, &observed), Cached::Resolved(theirs));
    }

    #[test]
    fn a_merge_invalidates_everything_pointing_at_the_merged_resource() {
        // Without this, a merged-away device keeps receiving telemetry under its old
        // resource_id for as long as its entries survive — and the entries are refreshed
        // by that very telemetry, so "as long as" is forever.
        let cache = ResolutionCache::default();
        let tenant = TenantId::new();
        let merged = ResourceId::new();
        let other = ResourceId::new();

        let a = vec![ident(IdentifierKind::Serial, "FTX1")];
        let b = vec![ident(IdentifierKind::ChassisId, "AA:BB")];
        cache.remember(tenant, &a, merged);
        cache.remember(tenant, &b, other);

        cache.invalidate_resource(tenant, merged);

        assert_eq!(cache.lookup(tenant, &a), Cached::Miss);
        assert_eq!(
            cache.lookup(tenant, &b),
            Cached::Resolved(other),
            "an unrelated resource must survive the invalidation"
        );
        assert_eq!(cache.stats().invalidations, 1);
    }

    #[test]
    fn the_cache_is_bounded() {
        // 50k msg/s across a large estate would otherwise grow without limit, and the
        // first sign of it is the whole process being killed.
        let cache = ResolutionCache::new(4);
        let tenant = TenantId::new();
        for i in 0..10 {
            cache.remember(
                tenant,
                &[ident(IdentifierKind::Serial, &format!("FTX{i}"))],
                ResourceId::new(),
            );
        }
        assert_eq!(cache.len(), 4);
    }

    #[test]
    fn the_hit_rate_is_measurable_not_asserted_in_prose() {
        let cache = ResolutionCache::default();
        let tenant = TenantId::new();
        let observed = vec![ident(IdentifierKind::Serial, "FTX1")];
        let resource = ResourceId::new();

        assert_eq!(cache.stats().hit_rate(), None, "nothing looked up yet");

        let _ = cache.lookup(tenant, &observed); // miss
        cache.remember(tenant, &observed, resource);
        for _ in 0..99 {
            let _ = cache.lookup(tenant, &observed);
        }

        let rate = cache.stats().hit_rate().unwrap();
        assert!(rate > 0.98, "steady-state hit rate was {rate}");
    }
}
