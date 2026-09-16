//! The pipeline — SPEC §M3.3, the stage every collector shares.
//!
//! ```text
//! receiver → decode → observed identity
//!                         ↓
//!                   identity resolution   (cached, §M0.2)
//!                         ↓
//!                     normalize           (severity mapping, semconv keys)
//!                         ↓
//!                      enrich             (site, vendor; GeoIP at M7)
//!                         ↓
//!                   batch accumulator     (N rows or T ms, whichever first)
//!                         ↓
//!                      ClickHouse
//! ```
//!
//! Decode and normalize are the collector's — they are the only stages that know what a
//! syslog message or an OTLP record looks like. Everything else is here, once, because
//! SPEC §M0.2 says so and gives the reason:
//!
//! > **Resolution happens once, in the pipeline (§M3.3), not in each collector.**
//!
//! Two collectors resolving independently would each hold their own cache, disagree
//! during a merge, and each create their own provisional resource for the same unknown
//! device — so a switch that both logs and polls would appear twice, with half its
//! telemetry on each. That is the failure this product exists to prevent, and it would be
//! caused by the product.
//!
//! # Nothing is dropped, at any stage
//!
//! The rule the syslog parsers hold — an unreadable message is still a message — is held
//! here too, and it is harder here because the failures are other systems':
//!
//! | what failed | what happens |
//! |---|---|
//! | the message did not parse | a row, with `parse.error` set (the collector's job) |
//! | nothing matched any identifier | a resource is created; the row attaches to it |
//! | the match was plausible but weak | a **provisional** resource and a review item; ingestion continues |
//! | `PostgreSQL` is unreachable, so resolution failed | a row against [`ResourceId::nil`], counted |
//! | `PostgreSQL` answered but the resource is not readable | a row with no site and no vendor, counted |
//! | `ClickHouse` is unreachable | the batch is retried with backoff and kept |
//!
//! The fourth row is the ugly one and is worth being explicit about. A log with no
//! resource is nearly useless — it appears in no resource's timeline — but it is **not**
//! unrecoverable, because the identifiers that would have resolved it are in the row's
//! own attributes: `host.name` and `syslog.source.address` are written before resolution
//! is attempted. A later backfill can attribute them. A dropped message cannot be
//! backfilled from anything.
//!
//! [`ResourceId::nil`]: uops_core::ResourceId::nil

pub mod attribute;
pub mod batch;
pub mod wal;

use std::sync::Mutex;

use tokio::sync::mpsc;
use uops_core::{ObservedIdentity, Resolution, ResourceId, TenantId};
use uops_identity::{IdentityStore, Resolver};
use uops_store_ch::LogRow;

pub use attribute::{EnrichStats, Enriched, Enricher, Enrichment};
pub use batch::{Config as BatchConfig, Sink, Stats as BatchStats};
pub use wal::{Config as WalConfig, Wal};

/// Everything a row needs that the message itself cannot say.
///
/// Produced by [`Pipeline::attribute`] and consumed by whichever `to_row` the collector
/// owns. It is the pipeline's output and the collector's input, which is why it lives
/// here rather than in either collector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribution {
    pub tenant_id: TenantId,
    pub resource_id: ResourceId,
    /// The nil uuid when the resource has no site — `MetricRow` makes the same choice and
    /// for the same reason: the column is not nullable and one answer to "no site" beats
    /// two.
    pub site_id: uops_core::SiteId,
    /// `cisco`, `mikrotik`, or empty. From the resource's profile, not from the message:
    /// a vendor guessed per-message would disagree with itself across a device's log.
    pub vendor: String,
}

/// What the pipeline has done. Every counter is a thing an operator would want to see on
/// `/api/v1/health`, and every one of them is asserted somewhere rather than described.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Observations that resolved to an existing resource.
    pub matched: u64,
    /// Observations that created one, because nothing matched.
    pub created: u64,
    /// Observations that created a **provisional** one and a review item, because the
    /// evidence was plausible but under the auto-merge bar.
    pub review: u64,
    /// Resolutions that failed outright — `PostgreSQL` unreachable, in practice.
    ///
    /// The rows are still written, against the nil resource. Non-zero means telemetry
    /// arrived that is attached to nothing and needs a backfill, which is a different
    /// and much louder problem than a slow query.
    pub unresolved: u64,
}

/// Resolve and enrich, once, for every collector.
pub struct Pipeline<S: IdentityStore, E: Enricher> {
    resolver: Resolver<S>,
    enrichment: Enrichment<E>,
    stats: Mutex<Stats>,
}

impl<S: IdentityStore, E: Enricher> std::fmt::Debug for Pipeline<S, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("stats", &self.stats())
            .field("enrichment", &self.enrichment.stats())
            .finish_non_exhaustive()
    }
}

impl<S: IdentityStore, E: Enricher> Pipeline<S, E> {
    #[must_use]
    pub fn new(resolver: Resolver<S>, enrichment: Enrichment<E>) -> Self {
        Self {
            resolver,
            enrichment,
            stats: Mutex::new(Stats::default()),
        }
    }

    #[must_use]
    pub fn stats(&self) -> Stats {
        *self.stats.lock().expect("pipeline stats")
    }

    /// The resolution cache's counters, which is where the >99% hit-rate criterion is
    /// measured.
    #[must_use]
    pub fn resolution_stats(&self) -> uops_identity::CacheStats {
        self.resolver.cache().stats()
    }

    #[must_use]
    pub fn enrichment_stats(&self) -> EnrichStats {
        self.enrichment.stats()
    }

    #[must_use]
    pub const fn resolver(&self) -> &Resolver<S> {
        &self.resolver
    }

    /// Who this observation belongs to, and what is known about them.
    ///
    /// **Never fails.** See the module docs for what each failure produces instead; the
    /// short version is that every one of them produces a row, because a pipeline that
    /// returned `Err` here would leave its caller with a message and nowhere to put it,
    /// and the only thing a caller can do with that is drop it.
    pub async fn attribute(&self, tenant: TenantId, observed: &ObservedIdentity) -> Attribution {
        let resource_id = match self.resolver.resolve(tenant, observed).await {
            Ok(Resolution::Matched { resource_id, .. }) => {
                self.stats.lock().expect("pipeline stats").matched += 1;
                resource_id
            }
            Ok(Resolution::Created { resource_id }) => {
                self.stats.lock().expect("pipeline stats").created += 1;
                resource_id
            }
            // A provisional resource exists and the telemetry attaches to it, so ingestion
            // continues while a human decides. Attaching to the *candidate* instead would
            // be an auto-merge by the back door — the exact thing the review threshold
            // exists to refuse.
            Ok(Resolution::Review { provisional_id, .. }) => {
                self.stats.lock().expect("pipeline stats").review += 1;
                provisional_id
            }
            Err(why) => {
                let mut stats = self.stats.lock().expect("pipeline stats");
                stats.unresolved += 1;
                // The first only. At 50k msg/s a line per message during a PostgreSQL
                // outage would fill a disk with the report of the outage.
                if stats.unresolved == 1 {
                    eprintln!(
                        "pipeline: identity resolution failed, rows will be unattributed \
                         and need a backfill: {why}"
                    );
                }
                ResourceId::nil()
            }
        };

        // Deliberately skipped for the nil resource: there is nothing to look up, and
        // asking would turn one failing query per message into two.
        let enriched = if resource_id == ResourceId::nil() {
            Enriched::default()
        } else {
            self.enrichment.of(tenant, resource_id).await
        };

        Attribution {
            tenant_id: tenant,
            resource_id,
            site_id: enriched.site_id,
            vendor: enriched.vendor,
        }
    }

    /// Forget what is cached about one resource, because it was edited, merged or split.
    ///
    /// Both caches, because both can be stale for the same event: a merge changes which
    /// resource an identifier points at *and* which site and vendor that resource has.
    pub fn invalidate(&self, tenant: TenantId, resource: ResourceId) {
        self.resolver.cache().invalidate_resource(tenant, resource);
        self.enrichment.invalidate(tenant, resource);
    }
}

/// Where finished rows go.
///
/// A plain `mpsc::Sender` rather than a method on [`Pipeline`], because the batcher is
/// its own task with its own lifetime — `batch::run` returns when this is dropped, which
/// is how a shutdown flushes rather than discards.
pub type Rows = mpsc::Sender<LogRow>;

#[cfg(test)]
mod tests {
    use uops_core::{IdentifierKind, Resolution};
    use uops_identity::MemoryIdentityStore;

    use super::*;

    #[derive(Default)]
    struct NoEnrichment;

    #[async_trait::async_trait]
    impl Enricher for NoEnrichment {
        async fn enrich(
            &self,
            _tenant: TenantId,
            _resource: ResourceId,
        ) -> Result<Option<Enriched>, String> {
            Ok(Some(Enriched {
                site_id: uops_core::SiteId::nil(),
                vendor: "cisco".to_owned(),
            }))
        }
    }

    struct Broken;

    #[async_trait::async_trait]
    impl Enricher for Broken {
        async fn enrich(
            &self,
            _tenant: TenantId,
            _resource: ResourceId,
        ) -> Result<Option<Enriched>, String> {
            Err("postgres is unreachable".to_owned())
        }
    }

    fn pipeline<E: Enricher>(enricher: E) -> Pipeline<MemoryIdentityStore, E> {
        Pipeline::new(
            Resolver::new(MemoryIdentityStore::default()),
            Enrichment::new(enricher),
        )
    }

    fn observed(host: &str) -> ObservedIdentity {
        ObservedIdentity::new("syslog").with(IdentifierKind::Hostname, host)
    }

    #[tokio::test]
    async fn the_first_message_from_a_device_creates_and_the_rest_match() {
        // The steady state, and the reason the cache exists: one query's worth of work
        // for a device's entire log.
        let pipeline = pipeline(NoEnrichment);
        let tenant = TenantId::new();

        let first = pipeline.attribute(tenant, &observed("rtr-01")).await;
        assert_eq!(pipeline.stats().created, 1);
        assert_eq!(
            first.vendor, "cisco",
            "enrichment fills what the message cannot"
        );

        for _ in 0..500 {
            let again = pipeline.attribute(tenant, &observed("rtr-01")).await;
            assert_eq!(again.resource_id, first.resource_id);
        }

        let stats = pipeline.stats();
        assert_eq!(stats.matched, 500);
        assert_eq!(stats.created, 1);
        assert_eq!(stats.unresolved, 0);

        // The acceptance criterion, measured rather than asserted in a comment.
        let hit_rate = pipeline.resolution_stats().hit_rate().expect("a rate");
        assert!(hit_rate > 0.99, "identity cache hit rate {hit_rate}");
    }

    #[tokio::test]
    async fn two_tenants_sending_the_same_hostname_get_two_resources() {
        // Every managed-service customer has a core-sw-01. Resolution is scoped to a
        // tenant at every layer, and this asserts the pipeline did not widen it.
        let pipeline = pipeline(NoEnrichment);
        let (a, b) = (TenantId::new(), TenantId::new());

        let one = pipeline.attribute(a, &observed("core-sw-01")).await;
        let two = pipeline.attribute(b, &observed("core-sw-01")).await;

        assert_ne!(one.resource_id, two.resource_id);
        assert_eq!(one.tenant_id, a);
        assert_eq!(two.tenant_id, b);
    }

    #[tokio::test]
    async fn a_message_with_nothing_to_go_on_still_gets_a_resource() {
        // Rule 1: a provisional resource rather than a dropped envelope. Somebody will
        // name it later; the telemetry is kept either way.
        let pipeline = pipeline(NoEnrichment);
        let attribution = pipeline
            .attribute(TenantId::new(), &ObservedIdentity::new("syslog"))
            .await;
        assert_ne!(attribution.resource_id, ResourceId::nil());
    }

    #[tokio::test]
    async fn a_failing_enricher_does_not_cost_the_row_its_resource() {
        // The row loses its site and its vendor and keeps everything else. Losing the
        // message instead would trade two columns for the whole record.
        let pipeline = pipeline(Broken);
        let attribution = pipeline
            .attribute(TenantId::new(), &observed("rtr-01"))
            .await;

        assert_ne!(attribution.resource_id, ResourceId::nil());
        assert_eq!(attribution.vendor, "");
        assert_eq!(pipeline.enrichment_stats().errors, 1);
    }

    #[tokio::test]
    async fn syslog_lands_on_the_device_snmp_already_found() {
        // The thesis, in one test. SNMP discovers a switch by serial and hostname;
        // syslog knows only the hostname. That hostname is already attached to that
        // resource and to nothing else, so the log lands on the polled device with
        // nobody configuring anything — which is what "one resource identity" means.
        let store = MemoryIdentityStore::default();
        let resolver = Resolver::new(store);
        let tenant = TenantId::new();

        let discovered = ObservedIdentity::new("snmp")
            .with(IdentifierKind::Serial, "FTX1840ABCD")
            .with(IdentifierKind::Hostname, "rtr-01");
        let Resolution::Created {
            resource_id: original,
        } = resolver
            .resolve(tenant, &discovered)
            .await
            .expect("resolve")
        else {
            panic!("the first sighting creates")
        };

        let pipeline = Pipeline::new(resolver, Enrichment::new(NoEnrichment));
        let attribution = pipeline.attribute(tenant, &observed("rtr-01")).await;

        assert_eq!(attribution.resource_id, original);
        assert_eq!(pipeline.stats().matched, 1);
        assert_eq!(pipeline.stats().review, 0);
    }

    #[tokio::test]
    async fn a_name_nobody_has_seen_before_is_a_question_not_an_assumption() {
        // The other half, and the reason the review band still exists. The address is
        // known and the hostname is not, so the message is asserting that this name
        // belongs to that box — which it might, or a DHCP lease might have moved the
        // address. The row attaches to the provisional, never to the candidate: doing
        // otherwise would be an auto-merge by the back door.
        let store = MemoryIdentityStore::default();
        let resolver = Resolver::new(store);
        let tenant = TenantId::new();

        let known = ObservedIdentity::new("snmp").with(IdentifierKind::MgmtIp, "10.0.0.1");
        let Resolution::Created {
            resource_id: original,
        } = resolver.resolve(tenant, &known).await.expect("resolve")
        else {
            panic!("the first sighting creates")
        };

        let pipeline = Pipeline::new(resolver, Enrichment::new(NoEnrichment));
        let from_syslog = ObservedIdentity::new("syslog")
            .with(IdentifierKind::MgmtIp, "10.0.0.1")
            .with(IdentifierKind::Hostname, "rtr-01");
        let attribution = pipeline.attribute(tenant, &from_syslog).await;

        assert_eq!(pipeline.stats().review, 1);
        assert_ne!(
            attribution.resource_id, original,
            "the row attaches to the provisional, not to the candidate"
        );
    }
}
