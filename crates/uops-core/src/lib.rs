//! Core types for the unified observability platform.
//!
//! Every other crate imports this one. It holds the four things SPEC §M0 says must
//! exist before any collector is written:
//!
//! | module | what |
//! |---|---|
//! | [`ids`] | Typed `UUIDv7` identifiers — tenant isolation enforced by the type system |
//! | [`resource`] | The resource model and relationship edges |
//! | [`identity`] | Identity resolution rules: confidence, noisy-OR, contradiction |
//! | [`envelope`] | The one telemetry shape every signal arrives in |
//! | [`secret`] | [`Secret<T>`], which cannot be logged or serialised |
//! | [`scope`] | [`TenantScope`], required to construct any query |
//!
//! # The one rule
//!
//! A missing tenant filter must be a **compile error**, not a code-review miss. Query
//! and repository functions take a [`TenantScope`], and a `TenantScope` can only be
//! produced from an authenticated request context. See [`scope`].

pub mod attr;
pub mod envelope;
pub mod error;
pub mod identity;
pub mod ids;
pub mod maintenance;
pub mod resource;
pub mod scope;
pub mod secret;
pub mod tags;

pub use attr::{AttrMap, AttrValue, semconv};
pub use envelope::{
    EventRecord, LogRecord, MetricKind, MetricPoint, Severity, Signal, Source, SourceKind,
    StateRecord, TelemetryEnvelope,
};
pub use error::{Error, Result};
pub use identity::{
    AUTO_MERGE_THRESHOLD, Candidate, Contradiction, Identifier, IdentifierKind, Match,
    ObservedIdentity, Outcome, OutcomeReason, REVIEW_FLOOR, Resolution, classify,
    combine_confidence,
};
pub use ids::{
    ActorId, CredentialRef, DecisionId, OrgId, ResourceGroupId, ResourceId, SessionId, SiteId,
    TenantId,
};
pub use maintenance::{Recurrence, Schedule, Suppression, Target, WindowError};
pub use resource::{
    Relationship, RelationshipKind, Resource, ResourceGroup, ResourceKind, ResourceStatus,
};
pub use scope::{Role, TenantScope};
pub use secret::{AuthProtocol, CredentialMaterial, PrivProtocol, Secret};
pub use tags::{TagError, Tags};
