//! Everything a telemetry row needs that the message itself cannot say.
//!
//! A syslog message knows its hostname and a `NetFlow` record knows an address. Neither
//! knows which resource it belongs to, which site that resource is at, or who makes it.
//! Answering those three questions is this module, and it is deliberately not in the
//! syslog crate: SPEC §M0.2 is explicit that *"resolution happens once, in the pipeline
//! (§M3.3), not in each collector"*.
//!
//! That is not tidiness. Two collectors resolving independently would each hold their own
//! cache, disagree during a merge, and — worse — each create their own provisional
//! resource for the same unknown device, so a switch that both logs and polls would
//! appear twice with half its telemetry on each.
//!
//! # The two lookups have different shapes
//!
//! Resolution is `identifiers → resource_id` and is [`uops_identity::Resolver`]'s job,
//! cache included. Enrichment is `resource_id → (site, vendor)` and is this module's,
//! because nothing else needed it until now.
//!
//! They are cached separately because they change for different reasons. An identifier
//! starts pointing somewhere else when a resource is merged or split; a site or a vendor
//! changes when somebody edits the resource. Sharing one cache would mean invalidating
//! correct entries because an unrelated thing moved.
//!
//! # Why enrichment is cached at all
//!
//! The same reason resolution is. SPEC §M0.2 puts a number on it — *"a syslog receiver at
//! 50k msg/s cannot hit `PostgreSQL` per message"* — and an uncached enrichment would
//! reintroduce exactly the per-message query that the resolution cache exists to remove.
//! The workload is the same shape: a device that sends ten thousand messages an hour has
//! one site and one vendor for all of them.

use std::num::NonZeroUsize;
use std::sync::Mutex;

use uops_core::{ResourceId, SiteId, TenantId};

/// What the resource says about itself, for the columns the message cannot fill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enriched {
    /// The nil uuid when the resource has no site. `MetricRow` and `LogRow` both make
    /// that choice and for the same reason: the column is not nullable, and one answer
    /// to "no site" beats two.
    pub site_id: SiteId,
    /// `cisco`, `mikrotik`, or empty. From the resource, not from the message — a vendor
    /// guessed per-message would disagree with itself across a single device's log.
    pub vendor: String,
}

/// Nothing known: no site, no vendor.
///
/// **Not** `#[derive(Default)]`. `SiteId::default()` mints a *fresh uuid v7*, like every
/// other id here, so a derived default would attach the row to a site that does not exist
/// and that is different for every row — silently wrong in the site rollup, and invisible
/// because the column would be populated. Found by a test that compared two defaults.
impl Default for Enriched {
    fn default() -> Self {
        Self {
            site_id: SiteId::nil(),
            vendor: String::new(),
        }
    }
}

/// Where `(site, vendor)` comes from.
///
/// A trait rather than `PgStore` directly, for the reason every other boundary here is
/// one: the pipeline's behaviour under a slow or failing lookup is what needs testing,
/// and a test that can only be written against real `PostgreSQL` is a test that does not
/// get written.
#[async_trait::async_trait]
pub trait Enricher: Send + Sync {
    /// Look up one resource.
    ///
    /// # Errors
    ///
    /// Whatever the store said. A resource that does not exist is **not** an error — it
    /// is `Ok(None)`, because the resolver may have created it moments ago in another
    /// process, and treating that as a failure would drop the telemetry.
    async fn enrich(
        &self,
        tenant: TenantId,
        resource: ResourceId,
    ) -> Result<Option<Enriched>, String>;
}

/// Counters, so the hit rate is something a test can assert rather than a comment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EnrichStats {
    pub hits: u64,
    pub misses: u64,
    /// Lookups that failed. The row is still written — see [`Enrichment::of`].
    pub errors: u64,
    pub invalidations: u64,
}

impl EnrichStats {
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

/// Enough resources for a large estate without being a memory problem: each entry is a
/// uuid pair and a short vendor string, so 100 000 of them is a few megabytes.
const DEFAULT_CAPACITY: usize = 100_000;

/// `resource_id → (site, vendor)`, bounded.
pub struct Enrichment<E: Enricher> {
    source: E,
    entries: Mutex<lru::LruCache<(TenantId, ResourceId), Enriched>>,
    stats: Mutex<EnrichStats>,
}

impl<E: Enricher> std::fmt::Debug for Enrichment<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Enrichment")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl<E: Enricher> Enrichment<E> {
    #[must_use]
    pub fn new(source: E) -> Self {
        Self::with_capacity(source, DEFAULT_CAPACITY)
    }

    #[must_use]
    pub fn with_capacity(source: E, capacity: usize) -> Self {
        let capacity = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);
        Self {
            source,
            entries: Mutex::new(lru::LruCache::new(capacity)),
            stats: Mutex::new(EnrichStats::default()),
        }
    }

    #[must_use]
    pub fn stats(&self) -> EnrichStats {
        *self.stats.lock().expect("enrichment stats")
    }

    /// What is known about this resource.
    ///
    /// **Never fails.** A lookup that errors, or a resource that is not there yet,
    /// returns the default — no site, no vendor — and counts it. The alternative is
    /// dropping a log because a control-plane query timed out, and a row with an empty
    /// vendor is a row somebody can still search; a row that was never written is not.
    /// `errors` being non-zero is the signal.
    pub async fn of(&self, tenant: TenantId, resource: ResourceId) -> Enriched {
        if let Some(hit) = self
            .entries
            .lock()
            .expect("enrichment cache")
            .get(&(tenant, resource))
            .cloned()
        {
            self.stats.lock().expect("enrichment stats").hits += 1;
            return hit;
        }

        match self.source.enrich(tenant, resource).await {
            Ok(Some(enriched)) => {
                self.stats.lock().expect("enrichment stats").misses += 1;
                self.entries
                    .lock()
                    .expect("enrichment cache")
                    .put((tenant, resource), enriched.clone());
                enriched
            }
            // Not an error. The resolver may have created this resource moments ago in
            // another process. Deliberately *not* cached: the next message should find
            // it once the row is visible, and caching "nothing" would blind this process
            // to a resource for as long as the entry lived.
            Ok(None) => {
                self.stats.lock().expect("enrichment stats").misses += 1;
                Enriched::default()
            }
            Err(why) => {
                let mut stats = self.stats.lock().expect("enrichment stats");
                stats.errors += 1;
                // One line per failure is still too many at 50k msg/s, so only the first
                // of a run is logged. The counter is the real signal.
                if stats.errors == 1 {
                    eprintln!("pipeline: enrichment failed, rows will lack site and vendor: {why}");
                }
                Enriched::default()
            }
        }
    }

    /// Forget one resource, because it was edited, merged or split.
    pub fn invalidate(&self, tenant: TenantId, resource: ResourceId) {
        if self
            .entries
            .lock()
            .expect("enrichment cache")
            .pop(&(tenant, resource))
            .is_some()
        {
            self.stats.lock().expect("enrichment stats").invalidations += 1;
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().expect("enrichment cache").len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    #[derive(Default)]
    struct Fake {
        calls: AtomicUsize,
        answer: Mutex<Option<Enriched>>,
        fail: AtomicBool,
    }

    #[async_trait::async_trait]
    impl Enricher for Arc<Fake> {
        async fn enrich(
            &self,
            _tenant: TenantId,
            _resource: ResourceId,
        ) -> Result<Option<Enriched>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err("the control plane is unavailable".to_owned());
            }
            Ok(self.answer.lock().expect("answer").clone())
        }
    }

    fn known() -> Enriched {
        Enriched {
            site_id: SiteId::new(),
            vendor: "cisco".to_owned(),
        }
    }

    #[tokio::test]
    async fn the_second_message_from_a_device_costs_nothing() {
        // The requirement SPEC puts a number on: a receiver at 50k msg/s cannot hit
        // PostgreSQL per message. A device sending ten thousand messages an hour has one
        // site and one vendor for all of them.
        let source = Arc::new(Fake::default());
        *source.answer.lock().expect("answer") = Some(known());
        let enrichment = Enrichment::new(Arc::clone(&source));
        let (tenant, resource) = (TenantId::new(), ResourceId::new());

        for _ in 0..1_000 {
            assert_eq!(enrichment.of(tenant, resource).await.vendor, "cisco");
        }

        assert_eq!(
            source.calls.load(Ordering::SeqCst),
            1,
            "one query for 1 000 messages"
        );
        let stats = enrichment.stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 999);
        assert!(stats.hit_rate().expect("a rate") > 0.99);
    }

    #[tokio::test]
    async fn a_failed_lookup_still_produces_a_row() {
        // The whole point. Dropping a log because a control-plane query timed out trades
        // a missing column for a missing message, and a row with an empty vendor is one
        // somebody can still search.
        let source = Arc::new(Fake::default());
        source.fail.store(true, Ordering::SeqCst);
        let enrichment = Enrichment::new(Arc::clone(&source));

        let enriched = enrichment.of(TenantId::new(), ResourceId::new()).await;
        assert_eq!(enriched, Enriched::default());
        assert_eq!(enrichment.stats().errors, 1);
    }

    #[tokio::test]
    async fn a_resource_that_is_not_there_yet_is_not_cached() {
        // The resolver may have created it moments ago in another process. Caching
        // "nothing" would blind this one to the resource for as long as the entry lived.
        let source = Arc::new(Fake::default());
        let enrichment = Enrichment::new(Arc::clone(&source));
        let (tenant, resource) = (TenantId::new(), ResourceId::new());

        assert_eq!(enrichment.of(tenant, resource).await, Enriched::default());
        *source.answer.lock().expect("answer") = Some(known());
        assert_eq!(enrichment.of(tenant, resource).await.vendor, "cisco");

        assert_eq!(source.calls.load(Ordering::SeqCst), 2, "it must ask again");
    }

    #[tokio::test]
    async fn an_edited_resource_is_looked_up_again() {
        let source = Arc::new(Fake::default());
        *source.answer.lock().expect("answer") = Some(known());
        let enrichment = Enrichment::new(Arc::clone(&source));
        let (tenant, resource) = (TenantId::new(), ResourceId::new());

        enrichment.of(tenant, resource).await;
        enrichment.invalidate(tenant, resource);
        *source.answer.lock().expect("answer") = Some(Enriched {
            vendor: "mikrotik".to_owned(),
            ..known()
        });

        assert_eq!(enrichment.of(tenant, resource).await.vendor, "mikrotik");
        assert_eq!(enrichment.stats().invalidations, 1);
    }

    #[tokio::test]
    async fn two_tenants_with_the_same_resource_id_do_not_share_an_entry() {
        // The id is a uuid so a collision is not realistic, but the key is asserted
        // rather than assumed: a cache keyed on resource alone would be a cross-tenant
        // leak of a vendor and a site, and it would be found by nobody.
        let source = Arc::new(Fake::default());
        *source.answer.lock().expect("answer") = Some(known());
        let enrichment = Enrichment::new(Arc::clone(&source));
        let resource = ResourceId::new();

        enrichment.of(TenantId::new(), resource).await;
        enrichment.of(TenantId::new(), resource).await;
        assert_eq!(source.calls.load(Ordering::SeqCst), 2);
    }
}
