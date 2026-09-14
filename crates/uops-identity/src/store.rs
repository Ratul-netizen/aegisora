//! What resolution needs from storage.
//!
//! Deliberately narrow. The resolver decides; the store remembers. Splitting them this
//! way is what lets the rules — which are the highest-leverage code in the product — be
//! tested exhaustively against an in-memory store, with the `PostgreSQL` implementation
//! tested separately for the thing only it can get wrong: whether the SQL agrees with
//! the schema.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uops_core::{
    ActorId, DecisionId, Identifier, Match, ResourceId, ResourceKind, Result, TenantId,
};

/// What was decided, recorded for every resolution that changes anything.
///
/// SPEC §M0.2: "This is what makes identity mistakes debuggable instead of mystifying."
/// Six months after two switches silently became one, this row is the only account of
/// why.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    pub id: DecisionId,
    pub tenant_id: TenantId,
    /// `None` when the decision created a resource rather than matching one.
    pub resource_id: Option<ResourceId>,
    pub outcome: DecisionOutcome,
    pub confidence: f32,
    /// The identifiers that matched, with their individual confidences.
    pub matched_by: Vec<Match>,
    /// Everything that was presented, including what did not match. A decision cannot
    /// be re-argued from the matches alone — the absence of a serial is often the
    /// reason a merge was only a review.
    pub observed: Vec<Identifier>,
    pub source: String,
    /// Set only for decisions a human made.
    pub actor_id: Option<ActorId>,
    pub decided_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionOutcome {
    AutoMerge,
    Review,
    New,
    ManualMerge,
    ManualSplit,
}

impl DecisionOutcome {
    /// The string the `identity_decision.outcome` CHECK constraint accepts.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AutoMerge => "auto_merge",
            Self::Review => "review",
            Self::New => "new",
            Self::ManualMerge => "manual_merge",
            Self::ManualSplit => "manual_split",
        }
    }
}

/// An identifier that pointed at a resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub resource_id: ResourceId,
    pub identifier: Identifier,
}

/// A pending review item, for `GET /api/v1/identity/review`.
#[derive(Clone, Debug)]
pub struct ReviewItem {
    pub decision: Decision,
    /// The provisional resource created so that ingestion was not blocked.
    pub provisional_id: ResourceId,
}

/// Persistence for identity resolution.
#[async_trait]
pub trait IdentityStore: Send + Sync {
    /// Which resources these identifiers already point at.
    ///
    /// One round trip for the whole set, not one per identifier: resolution runs on the
    /// ingestion path, and the difference between one query and nine is the difference
    /// between a pipeline that keeps up and one that does not.
    async fn lookup(&self, tenant: TenantId, identifiers: &[Identifier]) -> Result<Vec<Hit>>;

    /// Every identifier already recorded for a resource.
    ///
    /// Needed to detect a tier-1 contradiction: the observed serial disagreeing with the
    /// stored one is the proof that this is different hardware, and it cannot be seen
    /// from the observation alone.
    async fn identifiers_of(
        &self,
        tenant: TenantId,
        resource: ResourceId,
    ) -> Result<Vec<Identifier>>;

    /// Create a resource for telemetry that did not resolve to an existing one.
    ///
    /// Named apart from the repository's `create_resource` on purpose: that one is a
    /// deliberate act by an operator with a name and a site, this one is the resolver
    /// saying "something is sending telemetry and I cannot yet say what it is". A
    /// single name for both invites calling the wrong one.
    async fn create_provisional(
        &self,
        tenant: TenantId,
        kind: ResourceKind,
        name: &str,
    ) -> Result<ResourceId>;

    /// Attach identifiers to a resource, ignoring ones already present.
    ///
    /// Idempotent because ingestion repeats: the same host sends the same hostname
    /// thousands of times a minute, and every one of those must not be an error.
    async fn attach_identifiers(
        &self,
        tenant: TenantId,
        resource: ResourceId,
        identifiers: &[Identifier],
        source: &str,
    ) -> Result<()>;

    /// Attach identifiers, taking them from whichever resource holds them.
    ///
    /// The dangerous sibling of [`IdentityStore::attach_identifiers`], and used for
    /// exactly one thing: a tier-1 contradiction, where the serial proves the hardware
    /// was replaced and the management address now describes the new box. Anywhere else
    /// this would silently enact a merge nobody approved.
    async fn reassign_identifiers(
        &self,
        tenant: TenantId,
        resource: ResourceId,
        identifiers: &[Identifier],
        source: &str,
    ) -> Result<()>;

    async fn record_decision(&self, decision: &Decision) -> Result<()>;

    /// Point `historical` at `surviving`: write the alias, move the identifiers.
    ///
    /// Telemetry already written under `historical` is never rewritten — the alias is
    /// what makes a merge O(1) rather than a re-ingest, and `uops-query` expands through
    /// it on every read.
    async fn merge(
        &self,
        tenant: TenantId,
        historical: ResourceId,
        surviving: ResourceId,
        decision: &Decision,
    ) -> Result<()>;

    /// Undo a merge: move `identifiers` onto a new resource and drop the alias.
    async fn split(
        &self,
        tenant: TenantId,
        from: ResourceId,
        identifiers: &[Identifier],
        decision: &Decision,
    ) -> Result<ResourceId>;

    /// Pending review items, newest first.
    async fn pending_reviews(&self, tenant: TenantId, limit: i64) -> Result<Vec<ReviewItem>>;

    /// An unresolved review already asking this exact question.
    ///
    /// Without this, a device whose evidence lands in the review band produces a review
    /// item and a provisional resource **per message** — and a device sends thousands an
    /// hour. The first implementation of this resolver did exactly that.
    ///
    /// The question is identified by what was observed, not by the message: same
    /// identifier set, same tenant, still pending. SPEC §M0.2 describes the outcome
    /// bands without saying what happens on the second identical observation, and this
    /// is that answer — reuse the provisional resource, so telemetry keeps landing
    /// somewhere stable while a human decides.
    async fn find_pending_review(
        &self,
        tenant: TenantId,
        observed: &[Identifier],
    ) -> Result<Option<ReviewItem>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_strings_match_the_check_constraint() {
        // migrations/0004_identity.sql constrains this column to exactly these five.
        // A mismatch is a runtime insert failure on the ingestion path, which is the
        // worst place to discover a typo.
        let allowed = [
            "auto_merge",
            "review",
            "new",
            "manual_merge",
            "manual_split",
        ];
        for outcome in [
            DecisionOutcome::AutoMerge,
            DecisionOutcome::Review,
            DecisionOutcome::New,
            DecisionOutcome::ManualMerge,
            DecisionOutcome::ManualSplit,
        ] {
            assert!(
                allowed.contains(&outcome.as_str()),
                "{} is not in the CHECK constraint",
                outcome.as_str()
            );
        }
    }
}
