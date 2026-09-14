//! Identity resolution — SPEC §M0.2, the service around the rules.
//!
//! One `resource_id` for every name a device answers to. When `rtr-01` appears in
//! SNMP, syslog, `NetFlow`, LLDP, a config backup and an alert, all six must resolve to
//! the same resource, or every correlation the product performs is built on sand.
//!
//! | where | what |
//! |---|---|
//! | `uops_core::identity` | the rules: noisy-OR, tier-1 contradiction, thresholds. Pure, exhaustively tested |
//! | [`resolver`] | the service: cache, call ordering, what gets written when |
//! | [`cache`] | `(tenant, kind, value) → resource_id`, so 50k msg/s does not become 50k queries |
//! | [`store`] | the narrow persistence interface, implemented over `PostgreSQL` in `uops-store-pg` |
//!
//! # Example
//!
//! ```
//! use uops_core::{IdentifierKind, ObservedIdentity, Resolution, TenantId};
//! use uops_identity::{MemoryIdentityStore, Resolver};
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! let resolver = Resolver::new(MemoryIdentityStore::default());
//! let tenant = TenantId::new();
//!
//! // SNMP reports a serial. Nothing matches it, so a resource is created.
//! let poll = ObservedIdentity::new("snmp").with(IdentifierKind::Serial, "FTX1840ABCD");
//! let Resolution::Created { resource_id } = resolver.resolve(tenant, &poll).await.unwrap()
//! else {
//!     panic!("the first sighting creates")
//! };
//!
//! // A trap from the same device carries that serial and a hostname nobody has seen.
//! // The serial is tier-1 — globally unique by specification — so this is the same box,
//! // and the resource learns its hostname.
//! let trap = ObservedIdentity::new("snmp_trap")
//!     .with(IdentifierKind::Serial, "FTX1840ABCD")
//!     .with(IdentifierKind::Hostname, "rtr-01");
//!
//! assert!(matches!(
//!     resolver.resolve(tenant, &trap).await.unwrap(),
//!     Resolution::Matched { resource_id: r, .. } if r == resource_id
//! ));
//! # }
//! ```
//!
//! # What a weak match does instead
//!
//! Had those two sources shared only a hostname (0.65), the second would have produced
//! a [`Resolution::Review`](uops_core::Resolution::Review): a provisional resource so
//! ingestion continues, and a question for a human. That is deliberate — a hostname can
//! be inherited by a replacement box, a wrong auto-merge silently corrupts every
//! correlation downstream, and a queue item costs someone ten seconds.
//!
//! The practical consequence, worth knowing before it surprises anyone: **two sources
//! discovering the same device usually produce one review item**, unless they share a
//! tier-1 identifier or enough weaker ones to clear 0.95. The queue is deduplicated by
//! observation, so it is one item per device, not one per message.

pub mod cache;
pub mod memory;
pub mod resolver;
pub mod store;

pub use cache::{CacheStats, Cached, ResolutionCache};
pub use memory::MemoryIdentityStore;
pub use resolver::Resolver;
pub use store::{Decision, DecisionOutcome, Hit, IdentityStore, ReviewItem};
