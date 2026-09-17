//! The storage traits, and `ClickHouse` behind them — SPEC §M0.6.
//!
//! This is where the two halves M0 built separately finally meet: `uops-query` has
//! compiled telemetry SQL since M0 and `ch-migrations` has created the schema since the
//! migration runner, and until now nothing had ever executed one against the other.
//!
//! # Two deviations from the SPEC signatures, both deliberate
//!
//! **`query` takes the resolved resources.** SPEC writes `query(&self, q, scope)`.
//! Expanding a `ResourceSelector` is a `PostgreSQL` round trip through `resource_alias`,
//! and doing it inside a `ClickHouse` store would put the control plane in the middle of
//! a telemetry read. The caller resolves once and passes the result — which is also what
//! keeps `uops-query`'s golden tests free of a database.
//!
//! **`tail` returns a page, not a stream.** SPEC sketches the live tail as a stream, and
//! the signature here is deliberately the same shape as `query`: one window in, one
//! collected `ResultSet` out. `MergeTree` has no change feed — anything calling itself a
//! stream would be this poll with a connection held open in front of it, and a held-open
//! connection is the part that on-premise proxies drop at sixty seconds. What makes the
//! result a stream rather than a repeated search is the *window*, which is half-open on
//! `ingested_at`, so consecutive polls partition the rows exactly once each. See
//! [`uops_query::follow`].

use async_trait::async_trait;
use serde::Serialize;
use uops_core::TenantScope;
use uops_query::{Compiled, Query, ResolvedResources, compile, compile_tail};

use crate::client::ChClient;
use crate::error::{Error, Result};
use crate::rows::{LogRow, MetricRow, ResultSet, StateRow};

/// Reading telemetry.
#[async_trait]
pub trait TelemetryStore: Send + Sync {
    /// Run a compiled query.
    async fn query(
        &self,
        query: &Query,
        scope: &TenantScope,
        resources: &ResolvedResources,
    ) -> Result<ResultSet>;

    /// Run one poll of a live tail.
    ///
    /// A separate method rather than a flag on `query`, because it is a different
    /// physical query: ordered by time rather than by resource, which is what the
    /// `p_by_time` projection exists to serve — W1 measured that ordering at 2 303 ms
    /// and 33.8M rows without it, and 72 ms and 254K rows with it. The caller builds the
    /// window with [`uops_query::follow`]; `compile_tail` refuses anything that would
    /// stop the projection matching.
    async fn tail(
        &self,
        query: &Query,
        scope: &TenantScope,
        resources: &ResolvedResources,
    ) -> Result<ResultSet>;

    /// Whether the store is reachable, and what it is.
    async fn health(&self) -> Result<StoreHealth>;
}

/// Writing logs.
#[async_trait]
pub trait LogStore: TelemetryStore {
    async fn insert_logs(&self, rows: &[LogRow]) -> Result<()>;
}

/// Writing metrics.
#[async_trait]
pub trait MetricStore: TelemetryStore {
    async fn insert_metrics(&self, rows: &[MetricRow]) -> Result<()>;
}

/// Writing state transitions.
#[async_trait]
pub trait StateStore: TelemetryStore {
    async fn insert_states(&self, rows: &[StateRow]) -> Result<()>;
}

/// What `/api/v1/health` reports about telemetry storage.
#[derive(Clone, Debug, Serialize)]
pub struct StoreHealth {
    pub reachable: bool,
    /// The server's own version string. On-premise support asks this constantly, and
    /// the text index's syntax moved between versions — so knowing which one is
    /// actually running is not idle curiosity.
    pub version: String,
}

/// Telemetry over `ClickHouse`.
#[derive(Clone, Debug)]
pub struct ChStore {
    client: ChClient,
}

impl ChStore {
    #[must_use]
    pub const fn new(client: ChClient) -> Self {
        Self { client }
    }

    #[must_use]
    pub const fn client(&self) -> &ChClient {
        &self.client
    }

    /// Insert rows as `JSONEachRow`.
    ///
    /// One request per batch, never per row: the pipeline batches before it gets here,
    /// and a row-at-a-time insert into a `MergeTree` produces one part per row, which is
    /// the classic way to bring a `ClickHouse` cluster to its knees.
    async fn insert<T: Serialize + Sync>(&self, table: &str, rows: &[T]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }

        let mut body = String::new();
        for row in rows {
            let line = serde_json::to_string(row).map_err(|e| Error::Request(e.to_string()))?;
            body.push_str(&line);
            body.push('\n');
        }

        // The table name comes from this file, never from a caller.
        let sql = format!("INSERT INTO {table} FORMAT JSONEachRow\n{body}");
        self.client.run(&sql, &[]).await?;
        Ok(())
    }

    /// Run whatever the compiler produced.
    ///
    /// Shared by `query` and `tail` so the two entry points cannot drift in how they
    /// bind parameters or read a response — the difference between them is which
    /// statement was compiled, and nothing else.
    async fn execute(&self, compiled: Compiled) -> Result<ResultSet> {
        // JSONCompact: column names once in `meta`, values as arrays. The row-object
        // format repeats every column name on every row, which at ten thousand rows is
        // a large amount of bandwidth spent restating the schema.
        let sql = format!("{} FORMAT JSONCompact", compiled.sql.text());
        let params: Vec<(&str, String)> = compiled
            .sql
            .params()
            .iter()
            .map(|(name, p)| (name.as_str(), p.value.clone()))
            .collect();

        let raw = self.client.run(&sql, &params).await?;
        ResultSet::parse(&raw.body, compiled.table, compiled.warnings, raw.summary)
    }
}

#[async_trait]
impl TelemetryStore for ChStore {
    async fn query(
        &self,
        query: &Query,
        scope: &TenantScope,
        resources: &ResolvedResources,
    ) -> Result<ResultSet> {
        // The tenant predicate is written by the compiler, from the scope. Nothing in
        // this file can produce SQL, which is what makes that guarantee hold all the
        // way to the wire.
        self.execute(compile(query, scope, resources)?).await
    }

    async fn tail(
        &self,
        query: &Query,
        scope: &TenantScope,
        resources: &ResolvedResources,
    ) -> Result<ResultSet> {
        // The same guarantee by the same route: a different entry point into the same
        // compiler, not a second place that writes SQL.
        self.execute(compile_tail(query, scope, resources)?).await
    }

    async fn health(&self) -> Result<StoreHealth> {
        let raw = self.client.run("SELECT version()", &[]).await?;
        Ok(StoreHealth {
            reachable: true,
            version: raw.body.trim().to_owned(),
        })
    }
}

#[async_trait]
impl LogStore for ChStore {
    async fn insert_logs(&self, rows: &[LogRow]) -> Result<()> {
        self.insert("logs", rows).await
    }
}

#[async_trait]
impl MetricStore for ChStore {
    async fn insert_metrics(&self, rows: &[MetricRow]) -> Result<()> {
        self.insert("metrics", rows).await
    }
}

#[async_trait]
impl StateStore for ChStore {
    async fn insert_states(&self, rows: &[StateRow]) -> Result<()> {
        self.insert("states", rows).await
    }
}

/// A fingerprint of what a query asked for, for the access log.
///
/// The *shape* and never the parameters: a fingerprint carrying the customer's hostnames
/// and search terms would put a second copy of the data in the audit table, which is the
/// opposite of what an audit table is for. Signal, table and the presence of a filter
/// are what an auditor needs to recognise a pattern of access.
#[must_use]
pub fn fingerprint(query: &Query, table: &str) -> String {
    format!(
        "{}:{table}{}{}",
        query.signal.as_str(),
        if query.filter.is_some() {
            "+filter"
        } else {
            ""
        },
        if query.is_aggregate() { "+agg" } else { "" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use uops_query::{SignalType, TimeRange};

    fn query() -> Query {
        Query::new(
            SignalType::Log,
            TimeRange::new(
                chrono::DateTime::from_timestamp(0, 0).unwrap(),
                chrono::DateTime::from_timestamp(3_600, 0).unwrap(),
            ),
        )
    }

    #[test]
    fn a_fingerprint_describes_the_shape_and_nothing_else() {
        // What an auditor needs is "this account ran tenant-wide log searches all
        // night", not a transcript of what they searched for.
        let plain = fingerprint(&query(), "logs");
        assert_eq!(plain, "log:logs");

        let filtered = query().with_filter(uops_query::Expr::Text {
            field: uops_query::Field::Body,
            mode: uops_query::TextMode::AnyToken,
            terms: vec!["secret-hostname".into()],
        });
        let printed = fingerprint(&filtered, "logs");

        assert_eq!(printed, "log:logs+filter");
        assert!(
            !printed.contains("secret-hostname"),
            "a fingerprint must not carry the customer's data: {printed}"
        );
    }

    #[test]
    fn an_aggregate_is_distinguishable_from_a_scan() {
        let mut agg = query();
        agg.aggregations = vec![uops_query::Aggregation {
            func: uops_query::AggFunc::Count,
            field: None,
            alias: "c".into(),
        }];
        assert_eq!(
            fingerprint(&agg, "logs_counts_5m"),
            "log:logs_counts_5m+agg"
        );
    }
}
