//! Telemetry, end to end, against a real `ClickHouse`.
//!
//! The seam these tests exist for: `uops-query` compiles SQL, `ch-migrations` creates
//! the schema, and until this crate nothing had ever run one against the other. Both
//! sides have had their own tests pass for weeks while disagreeing — that is exactly how
//! `searchAll()` survived as long as it did.
//!
//! ```bash
//! docker compose -f deploy/docker-compose.yml up -d
//! bash scripts/ch.sh apply
//! cargo test -p uops-store-ch
//! ```

use std::collections::BTreeMap;

use chrono::{Duration, TimeZone, Utc};
use uops_core::{ResourceId, SiteId, TenantId, TenantScope};
use uops_query::{
    AggFunc, Aggregation, Expr, Field, Query, ResolvedResources, SignalType, TextMode, TimeRange,
};
use uops_store_ch::{
    ChClient, ChConfig, ChStore, LogRow, LogStore, MetricRow, MetricStore, TelemetryStore,
};

fn store() -> ChStore {
    ChStore::new(ChClient::new(ChConfig {
        user: std::env::var("CLICKHOUSE_USER").unwrap_or_else(|_| "uops".into()),
        password: std::env::var("CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "uops".into()),
        ..ChConfig::from_env()
    }))
}

/// A window that contains the fixtures below, and nothing else.
fn window() -> TimeRange {
    TimeRange::new(
        Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        Utc.timestamp_opt(1_700_003_600, 0).unwrap(),
    )
}

fn log_row(tenant: TenantId, resource: ResourceId, body: &str, offset_secs: i64) -> LogRow {
    let at = window().start + Duration::seconds(offset_secs);
    let mut attributes = BTreeMap::new();
    attributes.insert("host.name".to_owned(), "rtr-01".to_owned());
    attributes.insert("service.name".to_owned(), "bgpd".to_owned());

    LogRow {
        tenant_id: tenant,
        resource_id: resource,
        site_id: SiteId::nil(),
        observed_at: at,
        ingested_at: at,
        source_kind: "syslog".to_owned(),
        source_vendor: "cisco".to_owned(),
        severity: "error".to_owned(),
        facility: 23,
        body: body.to_owned(),
        attributes,
        trace_id: String::new(),
        span_id: String::new(),
    }
}

/// Each test gets its own tenant, so they neither collide nor need cleaning up — and
/// "another tenant cannot see this" is asserted against data that genuinely exists.
fn scope_for(tenant: TenantId) -> TenantScope {
    TenantScope::system(tenant)
}

/// A number from a result cell, however `ClickHouse` chose to encode it.
///
/// 64-bit integers arrive quoted when `output_format_json_quote_64bit_integers` is on,
/// which is the default, and unquoted otherwise — and which of the two you get depends
/// on the aggregate's result type. A test that assumes one silently reads every value
/// as absent, which looks exactly like an empty table.
fn number(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

#[tokio::test]
async fn health_reports_the_version_that_is_actually_running() {
    // On-premise support asks this constantly, and it is not idle curiosity: the text
    // index's function names moved between versions, which this project has already
    // been caught by once.
    let health = store().health().await.unwrap();
    assert!(health.reachable);
    assert!(
        health.version.starts_with("26."),
        "the schema is pinned to 26.8; found {}",
        health.version
    );
}

#[tokio::test]
async fn rows_go_in_and_come_back_out() {
    let store = store();
    let tenant = TenantId::new();
    let resource = ResourceId::new();
    let scope = scope_for(tenant);

    store
        .insert_logs(&[
            log_row(
                tenant,
                resource,
                "%LINK-3-UPDOWN: changed state to down",
                10,
            ),
            log_row(tenant, resource, "%LINK-3-UPDOWN: changed state to up", 20),
        ])
        .await
        .unwrap();

    let result = store
        .query(
            &Query::new(SignalType::Log, window()),
            &scope,
            &ResolvedResources::whole_tenant(&scope),
        )
        .await
        .unwrap();

    assert_eq!(result.len(), 2, "{:?}", result.rows);
    assert_eq!(result.table, "logs");
    assert!(
        result
            .value(0, "body")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("UPDOWN"),
        "{:?}",
        result.rows
    );
    // The column types come back too — a client guessing from the JSON gets
    // DateTime64 wrong, because it arrives as a string.
    assert_eq!(
        result
            .columns
            .iter()
            .find(|c| c.name == "observed_at")
            .unwrap()
            .ty,
        "DateTime64(3, 'UTC')"
    );
}

#[tokio::test]
async fn a_query_cannot_see_another_tenants_telemetry() {
    // The guarantee the whole stack is arranged around, asserted at the last place it
    // could still be lost. The predicate is written by the compiler from the scope —
    // nothing in uops-store-ch can produce SQL.
    let store = store();
    let mine = TenantId::new();
    let theirs = TenantId::new();
    let resource = ResourceId::new();

    store
        .insert_logs(&[
            log_row(mine, resource, "mine", 30),
            log_row(theirs, resource, "theirs", 30),
        ])
        .await
        .unwrap();

    let scope = scope_for(mine);
    let result = store
        .query(
            &Query::new(SignalType::Log, window()),
            &scope,
            &ResolvedResources::whole_tenant(&scope),
        )
        .await
        .unwrap();

    assert_eq!(result.len(), 1);
    assert_eq!(result.value(0, "body").unwrap(), "mine");
}

#[tokio::test]
async fn a_resolved_resource_set_narrows_the_read() {
    // What PgCatalog produces, reaching the predicate it was compiled into. The two
    // halves were built a commit apart and never ran together until now.
    let store = store();
    let tenant = TenantId::new();
    let wanted = ResourceId::new();
    let other = ResourceId::new();
    let scope = scope_for(tenant);

    store
        .insert_logs(&[
            log_row(tenant, wanted, "from the device we asked about", 40),
            log_row(tenant, other, "from a different device", 40),
        ])
        .await
        .unwrap();

    let resolved = ResolvedResources::already_resolved(&scope, vec![wanted]);
    let result = store
        .query(&Query::new(SignalType::Log, window()), &scope, &resolved)
        .await
        .unwrap();

    assert_eq!(result.len(), 1);
    assert_eq!(
        result.value(0, "resource_id").unwrap(),
        &serde_json::Value::String(wanted.to_string())
    );
}

#[tokio::test]
async fn token_search_reaches_the_text_index() {
    // The function names the compiler emits, against the index the DDL creates. This is
    // the exact pair that was wrong for weeks — searchAll() does not exist — and it was
    // only ever going to be caught by running one against the other.
    let store = store();
    let tenant = TenantId::new();
    let resource = ResourceId::new();
    let scope = scope_for(tenant);

    store
        .insert_logs(&[
            log_row(
                tenant,
                resource,
                "%LINK-3-UPDOWN: changed state to down",
                50,
            ),
            log_row(
                tenant,
                resource,
                "%SYS-5-CONFIG_I: configured from console",
                51,
            ),
        ])
        .await
        .unwrap();

    let hit = Query::new(SignalType::Log, window()).with_filter(Expr::Text {
        field: Field::Body,
        mode: TextMode::AllToken,
        terms: vec!["changed".into(), "down".into()],
    });
    let result = store
        .query(&hit, &scope, &ResolvedResources::whole_tenant(&scope))
        .await
        .unwrap();
    assert_eq!(result.len(), 1, "{:?}", result.rows);

    let miss = Query::new(SignalType::Log, window()).with_filter(Expr::Text {
        field: Field::Body,
        mode: TextMode::AnyToken,
        terms: vec!["zzqx".into()],
    });
    assert!(
        store
            .query(&miss, &scope, &ResolvedResources::whole_tenant(&scope))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_materialised_attribute_is_grouped_as_a_real_column() {
    // W1's most expensive finding: GROUP BY attributes['host.name'] was the slowest
    // query in the suite. The compiler rewrites it onto host_name, which only works if
    // the DDL actually declares that column — and only this proves it does.
    let store = store();
    let tenant = TenantId::new();
    let scope = scope_for(tenant);

    store
        .insert_logs(&[log_row(tenant, ResourceId::new(), "grouped", 60)])
        .await
        .unwrap();

    let mut grouped = Query::new(SignalType::Log, window());
    grouped.aggregations = vec![Aggregation {
        func: AggFunc::Count,
        field: None,
        alias: "n".into(),
    }];
    grouped.group_by = vec![Field::Attr {
        key: "host.name".into(),
    }];

    let result = store
        .query(&grouped, &scope, &ResolvedResources::whole_tenant(&scope))
        .await
        .unwrap();

    assert_eq!(result.len(), 1);
    assert_eq!(result.rows[0][0], "rtr-01", "{:?}", result.rows);
}

#[tokio::test]
async fn the_explorer_histogram_is_served_from_the_pre_aggregate() {
    // W1 FIX 2, end to end: the planner routes here, the materialised view populates it
    // on insert, and countMerge reads it back. Three separate pieces, first time
    // together.
    let store = store();
    let tenant = TenantId::new();
    let scope = scope_for(tenant);

    store
        .insert_logs(&[
            log_row(tenant, ResourceId::new(), "one", 70),
            log_row(tenant, ResourceId::new(), "two", 80),
        ])
        .await
        .unwrap();

    let mut histogram = Query::new(SignalType::Log, window());
    histogram.aggregations = vec![Aggregation {
        func: AggFunc::Count,
        field: None,
        alias: "c".into(),
    }];
    histogram.group_by = vec![Field::TimeBucket { seconds: 300 }];

    let result = store
        .query(&histogram, &scope, &ResolvedResources::whole_tenant(&scope))
        .await
        .unwrap();

    assert_eq!(
        result.table, "logs_counts_5m",
        "the histogram must not fall back to the base table"
    );
    let total: f64 = result.rows.iter().filter_map(|row| number(&row[1])).sum();
    assert!(
        (total - 2.0).abs() < f64::EPSILON,
        "the pre-aggregate must account for both rows: {:?}",
        result.rows
    );
}

#[tokio::test]
async fn a_slow_query_still_runs_and_says_it_was_slow() {
    // Substring search reads the whole tenant — W1 measured 1 928 ms at 100M rows. It
    // still works, and the warning travels with the result so the UI can say so before
    // somebody waits.
    let store = store();
    let tenant = TenantId::new();
    let scope = scope_for(tenant);

    store
        .insert_logs(&[log_row(tenant, ResourceId::new(), "interface reset", 90)])
        .await
        .unwrap();

    let query = Query::new(SignalType::Log, window()).with_filter(Expr::Text {
        field: Field::Body,
        mode: TextMode::Substring,
        terms: vec!["terface res".into()],
    });

    let result = store
        .query(&query, &scope, &ResolvedResources::whole_tenant(&scope))
        .await
        .unwrap();

    assert_eq!(result.len(), 1, "a substring must still match mid-token");
    assert!(
        result.warnings.iter().any(|w| matches!(
            w.warning,
            uops_query::QueryWarning::NotIndexAccelerated { .. }
        )),
        "{:?}",
        result.warnings
    );
}

#[tokio::test]
async fn the_server_reports_how_much_it_read() {
    // The number W1 was written around. Latency says a query was slow; rows read says
    // whether it pruned or scanned the tenant — and it is what the access log records.
    let store = store();
    let tenant = TenantId::new();
    let scope = scope_for(tenant);

    store
        .insert_logs(&[log_row(tenant, ResourceId::new(), "counted", 100)])
        .await
        .unwrap();

    let result = store
        .query(
            &Query::new(SignalType::Log, window()),
            &scope,
            &ResolvedResources::whole_tenant(&scope),
        )
        .await
        .unwrap();

    assert!(result.rows_read > 0, "the summary header was not parsed");
}

#[tokio::test]
async fn metrics_go_in_and_the_rollup_answers_a_long_window() {
    // raw → 5m → 1h, and the planner choosing between them by span. The chain is built
    // by materialised views on insert, so this exercises all three at once.
    let store = store();
    let tenant = TenantId::new();
    let resource = ResourceId::new();
    let scope = scope_for(tenant);

    let rows: Vec<MetricRow> = (0..12)
        .map(|i| MetricRow {
            tenant_id: tenant,
            resource_id: resource,
            site_id: SiteId::nil(),
            metric: "system.cpu.utilization".to_owned(),
            observed_at: window().start + Duration::minutes(i * 5),
            ingested_at: window().start,
            #[allow(clippy::cast_precision_loss)]
            value: (i + 1) as f64,
            unit: "1".to_owned(),
            labels: BTreeMap::new(),
        })
        .collect();
    store.insert_metrics(&rows).await.unwrap();

    // A window longer than raw retention: the planner must reach for a rollup.
    let mut long = Query::new(
        SignalType::Metric,
        TimeRange::new(window().start, window().start + Duration::days(60)),
    );
    long.aggregations = vec![Aggregation {
        func: AggFunc::Avg,
        field: Some(Field::Value),
        alias: "avg_cpu".into(),
    }];

    let result = store
        .query(&long, &scope, &ResolvedResources::whole_tenant(&scope))
        .await
        .unwrap();

    assert_eq!(result.table, "metrics_1h");
    assert!(
        result
            .warnings
            .iter()
            .any(|w| matches!(w.warning, uops_query::QueryWarning::Downsampled { .. })),
        "downsampling must never be silent: {:?}",
        result.warnings
    );

    // The mean of 1..=12 is 6.5, and it must survive two levels of state merging —
    // five-minute states re-aggregated into an hourly one. An average of averages
    // would also give 6.5 here (every bucket holds one point), so the weighting is
    // proven separately in scripts/ch.sh smoke, where the buckets are uneven.
    let avg = number(&result.rows[0][0]).expect("an average came back");
    assert!(
        (avg - 6.5).abs() < 0.001,
        "got {avg} from {:?}",
        result.rows
    );
}

#[tokio::test]
async fn a_query_the_compiler_refuses_never_reaches_the_server() {
    // A trace query is declared in the AST and unimplemented until M8. It must fail as
    // a caller error rather than as a ClickHouse exception about a missing table.
    let store = store();
    let tenant = TenantId::new();
    let scope = scope_for(tenant);

    let err = store
        .query(
            &Query::new(SignalType::Trace, window()),
            &scope,
            &ResolvedResources::whole_tenant(&scope),
        )
        .await
        .unwrap_err();

    let mapped: uops_core::Error = err.into();
    assert_eq!(mapped.status_code(), 400, "{mapped}");
}
