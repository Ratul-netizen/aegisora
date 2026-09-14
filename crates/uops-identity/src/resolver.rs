//! The resolver — SPEC §M0.2.
//!
//! "The highest-leverage component in the system. Everything downstream is worthless if
//! this is wrong." When `rtr-01` appears in SNMP, syslog, `NetFlow`, LLDP, a config
//! backup and an alert, all six have to arrive at one `resource_id`.
//!
//! The *rules* are in `uops_core::identity` — noisy-OR combination, the tier-1
//! contradiction, the thresholds — where they are pure and exhaustively tested. This
//! module is what surrounds them: the cache, the ordering of storage calls, and the
//! decisions about what to write when.
//!
//! # Three rules that shape everything here
//!
//! 1. **Never block ingestion.** An envelope that cannot be resolved gets a provisional
//!    resource; it is never dropped and never made to wait for a human. Telemetry lost
//!    during an incident is the worst failure this system has.
//! 2. **Never auto-merge on weak evidence.** A wrong merge silently corrupts every
//!    downstream correlation and is hard to even notice; a review-queue item costs
//!    someone ten seconds. The thresholds are deliberately conservative.
//! 3. **Record every decision.** Six months later this is the only account of why two
//!    switches became one.
//!
//! # The constraint that shapes the writes
//!
//! `UNIQUE (tenant_id, kind, value)` on `resource_identifier` means an identifier
//! belongs to exactly one resource. So when resolution is uncertain, the identifiers
//! that already matched something **stay where they are** — only the unmatched ones go
//! onto the provisional resource. A tier-1 contradiction is the one case that moves
//! them, because it is proof the hardware was replaced and the management address now
//! belongs to the new box.

use chrono::Utc;
use uops_core::{
    ActorId, Candidate, Contradiction, DecisionId, Identifier, Match, ObservedIdentity, Outcome,
    OutcomeReason, Resolution, ResourceId, ResourceKind, Result, TenantId, classify,
    combine_confidence,
};

use crate::cache::{Cached, ResolutionCache};
use crate::store::{Decision, DecisionOutcome, IdentityStore, ReviewItem};

/// Resolution, with its cache.
pub struct Resolver<S: IdentityStore> {
    store: S,
    cache: ResolutionCache,
}

impl<S: IdentityStore> std::fmt::Debug for Resolver<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolver")
            .field("cache", &self.cache)
            .finish_non_exhaustive()
    }
}

impl<S: IdentityStore> Resolver<S> {
    #[must_use]
    pub fn new(store: S) -> Self {
        Self {
            store,
            cache: ResolutionCache::default(),
        }
    }

    #[must_use]
    pub fn with_cache(store: S, cache: ResolutionCache) -> Self {
        Self { store, cache }
    }

    #[must_use]
    pub const fn cache(&self) -> &ResolutionCache {
        &self.cache
    }

    #[must_use]
    pub const fn store(&self) -> &S {
        &self.store
    }

    /// Resolve an observation to a resource.
    pub async fn resolve(
        &self,
        tenant: TenantId,
        observed: &ObservedIdentity,
    ) -> Result<Resolution> {
        // Nothing to go on. A provisional resource rather than a dropped envelope —
        // rule 1. Somebody will name it later; the telemetry is kept either way.
        if observed.identifiers.is_empty() {
            return self
                .create_for(tenant, observed, &[], OutcomeReason::NoMatch)
                .await;
        }

        // The steady-state path, and the reason a syslog receiver can keep up.
        if let Cached::Resolved(resource_id) = self.cache.lookup(tenant, &observed.identifiers) {
            return Ok(Resolution::Matched {
                resource_id,
                confidence: combine_confidence(&confidences(&observed.identifiers)),
                matched_by: matches_of(&observed.identifiers),
            });
        }

        let hits = self.store.lookup(tenant, &observed.identifiers).await?;
        let candidates = self.assemble_candidates(tenant, observed, &hits).await?;

        let Some(best) = pick_best(&candidates) else {
            return self
                .create_for(tenant, observed, &[], OutcomeReason::NoMatch)
                .await;
        };

        match classify(&best.matched_by, best.contradiction.as_ref()) {
            Outcome::AutoMerge { confidence } => {
                self.attach_and_record(tenant, observed, best, confidence)
                    .await
            }
            Outcome::Review { confidence } => {
                self.queue_review(tenant, observed, &candidates, confidence, &hits)
                    .await
            }
            Outcome::CreateNew { reason } => self.create_for(tenant, observed, &hits, reason).await,
        }
    }

    /// Build one candidate per resource the observation touched, with its matches and
    /// any tier-1 contradiction.
    async fn assemble_candidates(
        &self,
        tenant: TenantId,
        observed: &ObservedIdentity,
        hits: &[crate::store::Hit],
    ) -> Result<Vec<Candidate>> {
        let mut by_resource: Vec<(ResourceId, Vec<Match>)> = Vec::new();
        for hit in hits {
            let entry = by_resource
                .iter_mut()
                .find(|(id, _)| *id == hit.resource_id);
            let m = Match {
                kind: hit.identifier.kind,
                value: hit.identifier.value.clone(),
                confidence: hit.identifier.kind.base_confidence(),
            };
            match entry {
                Some((_, matches)) => matches.push(m),
                None => by_resource.push((hit.resource_id, vec![m])),
            }
        }

        let mut candidates = Vec::with_capacity(by_resource.len());
        for (resource_id, matched_by) in by_resource {
            // The contradiction cannot be seen from the observation alone: it is the
            // observed serial disagreeing with the *stored* one.
            let existing = self.store.identifiers_of(tenant, resource_id).await?;
            let contradiction = tier_one_contradiction(&observed.identifiers, &existing);

            candidates.push(Candidate {
                resource_id,
                confidence: combine_confidence(
                    &matched_by.iter().map(|m| m.confidence).collect::<Vec<_>>(),
                ),
                matched_by,
                contradiction,
            });
        }
        Ok(candidates)
    }

    async fn attach_and_record(
        &self,
        tenant: TenantId,
        observed: &ObservedIdentity,
        best: &Candidate,
        confidence: f32,
    ) -> Result<Resolution> {
        // Conflicts are ignored: the identifiers that matched are already attached, and
        // the new ones join. This is the path that learns a device's serial the first
        // time SNMP sees it, having previously known only its syslog hostname.
        self.store
            .attach_identifiers(
                tenant,
                best.resource_id,
                &observed.identifiers,
                &observed.source,
            )
            .await?;

        self.record(
            tenant,
            Some(best.resource_id),
            DecisionOutcome::AutoMerge,
            confidence,
            best.matched_by.clone(),
            observed,
            None,
        )
        .await?;

        self.cache
            .remember(tenant, &observed.identifiers, best.resource_id);

        Ok(Resolution::Matched {
            resource_id: best.resource_id,
            confidence,
            matched_by: best.matched_by.clone(),
        })
    }

    /// Plausible but not certain: create a provisional resource so ingestion continues,
    /// and put the question in front of a human.
    async fn queue_review(
        &self,
        tenant: TenantId,
        observed: &ObservedIdentity,
        candidates: &[Candidate],
        confidence: f32,
        hits: &[crate::store::Hit],
    ) -> Result<Resolution> {
        // The same device keeps sending. Without this, an observation stuck in the
        // review band would mint a provisional resource and a queue item per message —
        // thousands an hour, for one question nobody has answered yet.
        if let Some(existing) = self
            .store
            .find_pending_review(tenant, &observed.identifiers)
            .await?
        {
            return Ok(Resolution::Review {
                provisional_id: existing.provisional_id,
                candidates: candidates.to_vec(),
            });
        }

        let provisional = self
            .store
            .create_resource(tenant, guess_kind(observed), &guess_name(observed))
            .await?;

        // ONLY the identifiers that matched nothing. The ones that matched belong to the
        // candidate resource and must stay there — UNIQUE (tenant_id, kind, value) makes
        // that a constraint rather than a preference, and moving them would silently
        // enact the merge this review exists to ask about.
        let unmatched = unmatched(&observed.identifiers, hits);
        if !unmatched.is_empty() {
            self.store
                .attach_identifiers(tenant, provisional, &unmatched, &observed.source)
                .await?;
        }

        let best = candidates.first().cloned();
        self.record(
            tenant,
            Some(provisional),
            DecisionOutcome::Review,
            confidence,
            best.map(|c| c.matched_by).unwrap_or_default(),
            observed,
            None,
        )
        .await?;

        Ok(Resolution::Review {
            provisional_id: provisional,
            candidates: candidates.to_vec(),
        })
    }

    async fn create_for(
        &self,
        tenant: TenantId,
        observed: &ObservedIdentity,
        hits: &[crate::store::Hit],
        reason: OutcomeReason,
    ) -> Result<Resolution> {
        let resource_id = self
            .store
            .create_resource(tenant, guess_kind(observed), &guess_name(observed))
            .await?;

        if reason == OutcomeReason::TierOneContradiction {
            // The box was physically replaced. Every identifier observed now describes
            // the new hardware — including the management address that still points at
            // the predecessor — so these move rather than being skipped.
            self.store
                .reassign_identifiers(tenant, resource_id, &observed.identifiers, &observed.source)
                .await?;
            for hit in hits {
                self.cache.invalidate_resource(tenant, hit.resource_id);
            }
        } else {
            let unmatched = unmatched(&observed.identifiers, hits);
            if !unmatched.is_empty() {
                self.store
                    .attach_identifiers(tenant, resource_id, &unmatched, &observed.source)
                    .await?;
            }
        }

        self.record(
            tenant,
            Some(resource_id),
            DecisionOutcome::New,
            0.0,
            Vec::new(),
            observed,
            None,
        )
        .await?;

        Ok(Resolution::Created { resource_id })
    }

    /// Merge `historical` into `surviving`. Reversible by [`Resolver::split`].
    ///
    /// The decision records the identifiers that were moved, which is the pre-merge
    /// partition — without it a split would have to guess which identifiers came from
    /// which side.
    pub async fn merge(
        &self,
        tenant: TenantId,
        historical: ResourceId,
        surviving: ResourceId,
        actor: ActorId,
        reason: &str,
    ) -> Result<Decision> {
        if historical == surviving {
            return Err(uops_core::Error::Invalid(
                "a resource cannot be merged into itself".into(),
            ));
        }

        let moved = self.store.identifiers_of(tenant, historical).await?;
        let decision = Decision {
            id: DecisionId::new(),
            tenant_id: tenant,
            resource_id: Some(surviving),
            outcome: DecisionOutcome::ManualMerge,
            confidence: 1.0,
            matched_by: Vec::new(),
            observed: moved,
            source: reason.to_owned(),
            actor_id: Some(actor),
            decided_at: Utc::now(),
        };

        self.store
            .merge(tenant, historical, surviving, &decision)
            .await?;

        // Both sides: entries pointing at the merged-away resource are now wrong, and
        // entries pointing at the survivor may be missing the identifiers it gained.
        self.cache.invalidate_resource(tenant, historical);
        self.cache.invalidate_resource(tenant, surviving);

        Ok(decision)
    }

    /// Split `identifiers` off `from` onto a new resource, undoing a merge.
    pub async fn split(
        &self,
        tenant: TenantId,
        from: ResourceId,
        identifiers: &[Identifier],
        actor: ActorId,
        reason: &str,
    ) -> Result<(ResourceId, Decision)> {
        if identifiers.is_empty() {
            return Err(uops_core::Error::Invalid(
                "a split needs the identifiers to move".into(),
            ));
        }

        let decision = Decision {
            id: DecisionId::new(),
            tenant_id: tenant,
            resource_id: Some(from),
            outcome: DecisionOutcome::ManualSplit,
            confidence: 1.0,
            matched_by: Vec::new(),
            observed: identifiers.to_vec(),
            source: reason.to_owned(),
            actor_id: Some(actor),
            decided_at: Utc::now(),
        };

        let new_id = self
            .store
            .split(tenant, from, identifiers, &decision)
            .await?;

        self.cache.invalidate_resource(tenant, from);
        self.cache.invalidate_resource(tenant, new_id);

        Ok((new_id, decision))
    }

    /// The review queue, newest first.
    pub async fn reviews(&self, tenant: TenantId, limit: i64) -> Result<Vec<ReviewItem>> {
        self.store
            .pending_reviews(tenant, limit.clamp(1, 500))
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        tenant: TenantId,
        resource_id: Option<ResourceId>,
        outcome: DecisionOutcome,
        confidence: f32,
        matched_by: Vec<Match>,
        observed: &ObservedIdentity,
        actor: Option<ActorId>,
    ) -> Result<()> {
        self.store
            .record_decision(&Decision {
                id: DecisionId::new(),
                tenant_id: tenant,
                resource_id,
                outcome,
                confidence,
                matched_by,
                observed: observed.identifiers.clone(),
                source: observed.source.clone(),
                actor_id: actor,
                decided_at: Utc::now(),
            })
            .await
    }
}

fn confidences(identifiers: &[Identifier]) -> Vec<f32> {
    identifiers
        .iter()
        .map(|i| i.kind.base_confidence())
        .collect()
}

fn matches_of(identifiers: &[Identifier]) -> Vec<Match> {
    identifiers
        .iter()
        .map(|i| Match {
            kind: i.kind,
            value: i.value.clone(),
            confidence: i.kind.base_confidence(),
        })
        .collect()
}

/// Observed identifiers that pointed at nothing.
fn unmatched(observed: &[Identifier], hits: &[crate::store::Hit]) -> Vec<Identifier> {
    observed
        .iter()
        .filter(|i| {
            !hits
                .iter()
                .any(|h| h.identifier.kind == i.kind && h.identifier.value == i.value)
        })
        .cloned()
        .collect()
}

/// A tier-1 identifier observed with a different value than the one on record.
///
/// Only tier 1: a changed hostname is Tuesday, a changed serial is a different machine.
fn tier_one_contradiction(
    observed: &[Identifier],
    existing: &[Identifier],
) -> Option<Contradiction> {
    observed
        .iter()
        .filter(|o| o.kind.is_tier_one())
        .find_map(|o| {
            existing
                .iter()
                .find(|e| e.kind == o.kind && e.value != o.value)
                .map(|e| Contradiction {
                    kind: o.kind,
                    observed: o.value.clone(),
                    existing: e.value.clone(),
                })
        })
}

/// Strongest evidence wins; a contradiction loses to anything without one.
fn pick_best(candidates: &[Candidate]) -> Option<&Candidate> {
    candidates.iter().max_by(|a, b| {
        a.contradiction
            .is_none()
            .cmp(&b.contradiction.is_none())
            .then(a.confidence.total_cmp(&b.confidence))
    })
}

/// A starting name for a resource nobody has named yet. A human renames it, and
/// `display_name` means discovery will not overwrite them when they do.
fn guess_name(observed: &ObservedIdentity) -> String {
    use uops_core::IdentifierKind as K;
    for kind in [K::Hostname, K::Serial, K::MgmtIp, K::OtelHostId] {
        if let Some(i) = observed.identifiers.iter().find(|i| i.kind == kind) {
            return i.value.clone();
        }
    }
    observed.identifiers.first().map_or_else(
        || format!("unidentified-{}", observed.source),
        |i| i.value.clone(),
    )
}

/// A guess, corrected by a human or by discovery. `service_name` without a host is the
/// one case that is clearly not a device.
fn guess_kind(observed: &ObservedIdentity) -> ResourceKind {
    use uops_core::IdentifierKind as K;
    let has = |k: K| observed.identifiers.iter().any(|i| i.kind == k);

    if has(K::OtelHostId) {
        ResourceKind::Host
    } else if has(K::ServiceName) && !has(K::MgmtIp) && !has(K::Hostname) {
        ResourceKind::Service
    } else {
        ResourceKind::Device
    }
}
