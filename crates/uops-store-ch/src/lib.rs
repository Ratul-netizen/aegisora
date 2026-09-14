//! Telemetry over `ClickHouse` — M1.
//!
//! The half of the query layer that was missing. `uops-query` has compiled telemetry SQL
//! since M0 and `ch-migrations` has created the schema since the migration runner; until
//! this crate existed, nothing had ever executed one against the other.
//!
//! | module | what |
//! |---|---|
//! | [`client`] | the HTTP protocol: one POST per statement, parameters bound |
//! | [`store`] | the storage traits, and `ClickHouse` behind them |
//! | [`rows`] | what goes in, and what comes back |
//!
//! # The guarantee this crate must not break
//!
//! Nothing here writes SQL. Every statement comes from `uops_query::compile`, which
//! takes a `&TenantScope` and writes the tenant predicate itself. The one exception is
//! the `INSERT INTO <table>` prefix, whose table name is a literal in this crate and
//! never a caller's string.
//!
//! # Example
//!
//! ```no_run
//! use uops_core::{TenantId, TenantScope};
//! use uops_query::{Query, ResolvedResources, SignalType, TimeRange};
//! use uops_store_ch::{ChClient, ChConfig, ChStore, TelemetryStore};
//!
//! # async fn example() -> Result<(), uops_store_ch::Error> {
//! let store = ChStore::new(ChClient::new(ChConfig::from_env()));
//! let scope = TenantScope::system(TenantId::new());
//!
//! let query = Query::new(SignalType::Log, TimeRange::last(chrono::Duration::hours(1)));
//! let result = store
//!     .query(&query, &scope, &ResolvedResources::whole_tenant(&scope))
//!     .await?;
//!
//! println!("{} rows from {}, {} read", result.len(), result.table, result.rows_read);
//! # Ok(())
//! # }
//! ```

pub mod client;
pub mod error;
pub mod rows;
pub mod store;

pub use client::{ChClient, ChConfig, Summary};
pub use error::{Error, Result};
pub use rows::{Column, LogRow, MetricRow, ResultSet};
pub use store::{ChStore, LogStore, MetricStore, StoreHealth, TelemetryStore, fingerprint};
