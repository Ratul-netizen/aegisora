//! The control plane over `PostgreSQL` — M1.
//!
//! Resources, sites, identifiers and aliases: the rows that describe *what is being
//! monitored*, as opposed to the telemetry about it, which lives in `ClickHouse` and is
//! reached through `uops-query`.
//!
//! # Three rules this crate exists to hold
//!
//! | rule | how |
//! |---|---|
//! | No statement without a tenant | every method takes `&TenantScope`, and [`enforced`] reads this crate's own source to prove every query uses it |
//! | No SQL assembled from strings | `sqlx` macros, checked against the real schema at compile time |
//! | No `OFFSET` | keyset pagination on `UUIDv7` ids — see [`page`] |
//!
//! # Compile-time checking, and what it costs
//!
//! The `sqlx::query!` macros verify every statement against a live schema when this
//! crate is built, which is what makes a renamed column a compile error rather than a
//! runtime one. Builds use the checked-in `.sqlx/` metadata (`SQLX_OFFLINE=true`), so a
//! clone with no database still builds; regenerate it with `cargo sqlx prepare` after
//! changing a query, and CI fails if it is stale.
//!
//! # Example
//!
//! ```no_run
//! use uops_core::{ResourceKind, TenantId, TenantScope};
//! use uops_store_pg::{Config, NewResource, PgStore, ResourceFilter};
//!
//! # async fn example() -> uops_core::Result<()> {
//! let store = PgStore::connect(&Config::from_env()).await?;
//! let scope = TenantScope::system(TenantId::new());
//!
//! let device = store
//!     .create_resource(&scope, &NewResource::new(ResourceKind::Device, "rtr-01"))
//!     .await?;
//!
//! let page = store.resources(&scope, &ResourceFilter::default()).await?;
//! assert!(page.items.iter().any(|r| r.id == device.id));
//! # Ok(())
//! # }
//! ```

pub mod audit;
pub mod auth;
pub mod bootstrap;
pub mod catalog;
pub mod discovery;
mod enforced;
pub mod enrich;
pub mod error;
pub mod facts;
pub mod groups;
pub mod identity;
pub mod maintenance;
pub mod page;
pub mod pollable;
pub mod profile;
pub mod resource;
pub mod sealed;
pub mod sites;
pub mod store;

pub use audit::{AccessEntry, AuditEntry};
pub use auth::{
    ABSOLUTE_TIMEOUT, AuthenticatedSession, IDLE_TIMEOUT, TenantMembership, UserCredentials,
    UserProfile,
};
pub use bootstrap::{FirstRun, FirstRunRequest};
pub use catalog::PgCatalog;
pub use discovery::{DiscoveredChild, DiscoveryReport};
pub use enrich::PgEnricher;
pub use facts::{DeviceFacts, IdentityReport};
pub use groups::{GroupSummary, NewGroup};
pub use maintenance::{MaintenanceWindow, NewWindow};
pub use page::{Cursor, DEFAULT_PAGE, MAX_PAGE, Page};
pub use pollable::{PollableDevice, SYSOBJECTID_KEY};
pub use resource::{NewResource, ResourceFilter};
pub use sealed::PgSealedStore;
pub use sites::{Location, SiteOverview, StatusCounts};
pub use store::{Config, PgStore};
