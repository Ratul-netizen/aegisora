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

/// The five-minute boundary `secs_ago` seconds in the past.
///
/// # Why these fixtures are not at a fixed instant
///
/// They used to be, at `1_700_000_000` — 2023-11-14. Every table these tests write to
/// carries a retention TTL (`logs` and the pre-aggregates 365 days, `metrics` 30), so by
/// the time this was found the fixtures were nearly three years past retention: the rows
/// were inserted and then removed by a background TTL merge. Whether a test passed
/// depended on whether that merge had run against its part yet, which made
/// `the_explorer_histogram_is_served_from_the_pre_aggregate` fail about one run in
/// fifteen with an empty result. A `SELECT` at the time of the diagnosis found 335 rows
/// still in `logs` for that window and **zero** in `logs_counts_5m` — the aggregate had
/// already been swept.
///
/// Anchored to now instead, and truncated to a five-minute boundary so
/// `toStartOfFiveMinute` puts a fixture in a predictable bucket rather than one that
/// depends on when the suite ran. Backdated far enough that the whole window is in the
/// past: a row in the future is not a retention problem but it is not a measurement
/// either.
fn bucket_aligned(secs_ago: i64) -> chrono::DateTime<Utc> {
    let secs = Utc::now().timestamp() - secs_ago;
    Utc.timestamp_opt(secs - secs.rem_euclid(300), 0)
        .single()
        .expect("a truncated unix timestamp is a valid instant")
}

/// A window that contains the fixtures below, and nothing else.
///
/// One hour, two hours back. Tests share this `ClickHouse` and each uses its own
/// `TenantId`, so two runs whose windows differ still cannot see each other's rows.
fn window() -> TimeRange {
    let start = window_start();
    TimeRange::new(start, start + Duration::hours(1))
}

/// Where the window begins.
///
/// Computed once per process. Every call must return the same instant, because the
/// fixtures are written relative to it and then queried relative to it — if a run
/// crosses a five-minute boundary between the `INSERT` and the `SELECT`, a recomputed
/// start lands in the next bucket and the query looks in the wrong place. That is how
/// three of these tests failed on the run right after the window stopped being a
/// constant.
fn window_start() -> chrono::DateTime<Utc> {
    static START: std::sync::OnceLock<chrono::DateTime<Utc>> = std::sync::OnceLock::new();
    *START.get_or_init(|| bucket_aligned(2 * 3600))
}

/// The shortest retention any table these fixtures write to has.
///
/// `metrics` is 30 days; `logs`, `events` and the pre-aggregates are 365 or more. The
/// shortest one is what binds, because a fixture outside it is removed from that table
/// and left in the others — which is precisely the half-present state that made this
/// hard to see. Keep it in step with `ch-migrations/`.
const SHORTEST_RETENTION_DAYS: i64 = 30;

#[test]
fn the_fixtures_are_inside_every_retention_window() {
    // The guard on the bug, checked by the test suite on itself rather than by a CI
    // mutation, because the failure it prevents is not deterministic: TTL is applied by
    // a background merge on ClickHouse's own schedule, so an expired fixture passes
    // until the moment a merge lands between the INSERT and the SELECT. That is a test
    // that fails once a fortnight for a reason nobody can reproduce.
    //
    // This assertion has no such timing in it. It fails immediately, on every run, the
    // day somebody writes a fixed timestamp here again or shortens a TTL past it.
    let age = Utc::now() - window_start();
    assert!(
        age < Duration::days(SHORTEST_RETENTION_DAYS),
        "the fixtures are {} days old and the shortest retention is {SHORTEST_RETENTION_DAYS};          ClickHouse will delete them on its next TTL merge, which makes every test in this          file pass or fail depending on when that merge runs",
        age.num_days()
    );
    assert!(
        age > Duration::zero(),
        "the fixtures are in the future, which is not a measurement"
    );
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

/// One counter reading on a named interface.
fn counter_row(
    tenant: TenantId,
    resource: ResourceId,
    interface: &str,
    minute: i64,
    value: f64,
) -> MetricRow {
    let mut labels = BTreeMap::new();
    labels.insert("network.interface.index".to_owned(), interface.to_owned());
    MetricRow {
        tenant_id: tenant,
        resource_id: resource,
        site_id: SiteId::nil(),
        metric: "network.io.receive".to_owned(),
        observed_at: window().start + Duration::minutes(minute),
        ingested_at: window().start,
        value,
        unit: "By".to_owned(),
        labels,
    }
}

/// Ask for the per-bucket rate of `network.io.receive`, as a dashboard panel would.
fn rate_query(bucket_seconds: u32) -> Query {
    let mut q = Query::new(SignalType::Metric, window());
    q.filter = Some(Expr::Compare {
        field: Field::Metric,
        cmp: uops_query::CompareOp::Eq,
        value: uops_query::Value::Str("network.io.receive".into()),
    });
    q.aggregations = vec![
        Aggregation {
            func: AggFunc::Avg,
            field: Some(Field::Rate),
            alias: "bps".into(),
        },
        Aggregation {
            func: AggFunc::Min,
            field: Some(Field::Rate),
            alias: "low".into(),
        },
    ];
    q.group_by = vec![Field::TimeBucket {
        seconds: bucket_seconds,
    }];
    q.limit = 100;
    q
}

#[tokio::test]
async fn a_counter_wrap_produces_no_negative_rate() {
    // SPEC section M2's fourth acceptance criterion, in the place it has to hold: "a
    // 32-bit counter wrap produces no negative rate in any query". `uops_poll::counter`
    // states the rule in Rust and is thoroughly tested; nothing executes that Rust on a
    // query path, so this is the assertion that the criterion is actually met.
    //
    // A 32-bit counter near its ceiling, wrapping between two consecutive polls — which
    // on a 1 Gbps link with a 32-bit ifInOctets happens about every 34 seconds.
    let store = store();
    let tenant = TenantId::new();
    let resource = ResourceId::new();
    let scope = scope_for(tenant);

    let ceiling = f64::from(u32::MAX);
    let rows = vec![
        counter_row(tenant, resource, "1", 0, ceiling - 3_000.0),
        counter_row(tenant, resource, "1", 1, ceiling - 1_000.0), // +2 000, forwards
        counter_row(tenant, resource, "1", 2, 1_000.0),           // wrapped
        counter_row(tenant, resource, "1", 3, 3_000.0),           // +2 000, forwards
    ];
    store.insert_metrics(&rows).await.unwrap();

    // One bucket per sample, so each pair is visible on its own.
    let result = store
        .query(
            &rate_query(60),
            &scope,
            &ResolvedResources::whole_tenant(&scope),
        )
        .await
        .unwrap();

    let rates: Vec<Option<f64>> = result.rows.iter().map(|row| number(&row[1])).collect();
    assert!(
        rates.iter().flatten().all(|r| *r >= 0.0),
        "a wrap produced a negative rate: {rates:?}"
    );

    // And the minimum, which is where a negative would show up first.
    let lows: Vec<Option<f64>> = result.rows.iter().map(|row| number(&row[2])).collect();
    assert!(
        lows.iter().flatten().all(|r| *r >= 0.0),
        "a wrap produced a negative minimum: {lows:?}"
    );

    // Not vacuous: the forward pairs must have produced real numbers. 2 000 bytes over
    // 60 seconds is 33.3 B/s.
    let real: Vec<f64> = rates.iter().flatten().copied().collect();
    assert_eq!(
        real.len(),
        2,
        "two of the three pairs step forwards and must each yield a rate: {rates:?}"
    );
    for r in real {
        assert!(
            (r - 2_000.0 / 60.0).abs() < 0.001,
            "expected 33.3 B/s, got {r}"
        );
    }
}

#[tokio::test]
async fn a_wrap_is_a_gap_rather_than_a_repaired_number() {
    // The other half of the criterion, and the one a "fix" would break. The obvious
    // repair is 2^32 - previous + current; it is right exactly once, and on a 10 Gbps
    // link a 32-bit byte counter goes round seventeen times a minute, so it would report
    // one seventeenth of the traffic with total confidence.
    //
    // So the wrapped pair must yield *nothing* — not a large number and not a small one.
    let store = store();
    let tenant = TenantId::new();
    let resource = ResourceId::new();
    let scope = scope_for(tenant);

    let ceiling = f64::from(u32::MAX);
    store
        .insert_metrics(&[
            counter_row(tenant, resource, "1", 0, ceiling - 1_000.0),
            counter_row(tenant, resource, "1", 1, 1_000.0),
        ])
        .await
        .unwrap();

    let result = store
        .query(
            &rate_query(60),
            &scope,
            &ResolvedResources::whole_tenant(&scope),
        )
        .await
        .unwrap();

    let real: Vec<f64> = result
        .rows
        .iter()
        .filter_map(|row| number(&row[1]))
        .collect();
    assert!(
        real.is_empty(),
        "the only pair in this series wrapped; nothing should have been reported: {real:?}"
    );
}

#[tokio::test]
async fn two_interfaces_counters_are_not_differenced_against_each_other() {
    // A series is one resource, one metric, one set of labels. Partition by less and
    // interface 2 gets subtracted from interface 1 — which produces a number rather than
    // an error, and a plausible-looking one.
    let store = store();
    let tenant = TenantId::new();
    let resource = ResourceId::new();
    let scope = scope_for(tenant);

    // Interface 1 climbs slowly, interface 2 is far ahead and climbs at the same rate.
    // Interleaved in time, so a query ignoring the labels would see the sequence
    // 1 000, 900 000, 2 000, 901 000 and report enormous alternating rates.
    store
        .insert_metrics(&[
            counter_row(tenant, resource, "1", 0, 1_000.0),
            counter_row(tenant, resource, "2", 0, 900_000.0),
            counter_row(tenant, resource, "1", 1, 2_000.0),
            counter_row(tenant, resource, "2", 1, 901_000.0),
        ])
        .await
        .unwrap();

    let result = store
        .query(
            &rate_query(3600),
            &scope,
            &ResolvedResources::whole_tenant(&scope),
        )
        .await
        .unwrap();

    // Both interfaces moved 1 000 bytes in 60 seconds: 16.67 B/s each, so the average
    // over the two is the same number.
    let rates: Vec<f64> = result
        .rows
        .iter()
        .filter_map(|row| number(&row[1]))
        .collect();
    assert_eq!(rates.len(), 1, "one bucket: {rates:?}");
    assert!(
        (rates[0] - 1_000.0 / 60.0).abs() < 0.001,
        "expected 16.67 B/s per interface; got {} — the series were mixed",
        rates[0]
    );
}

#[tokio::test]
async fn the_first_sample_of_a_series_has_no_rate() {
    // `lagInFrame` returns the column default when there is no previous row — zero for a
    // Float64, the epoch for a DateTime64 — and the arithmetic would accept both,
    // yielding a plausible rate over fifty-six years. A single sample is not a rate.
    let store = store();
    let tenant = TenantId::new();
    let resource = ResourceId::new();
    let scope = scope_for(tenant);

    store
        .insert_metrics(&[counter_row(tenant, resource, "1", 0, 5_000.0)])
        .await
        .unwrap();

    let result = store
        .query(
            &rate_query(3600),
            &scope,
            &ResolvedResources::whole_tenant(&scope),
        )
        .await
        .unwrap();

    let real: Vec<f64> = result
        .rows
        .iter()
        .filter_map(|row| number(&row[1]))
        .collect();
    assert!(real.is_empty(), "one sample is not a rate: {real:?}");
}

/// The property that makes the tail a stream rather than a repeated search.
///
/// Two consecutive polls over `[t0, t1)` and `[t1, t2)` must between them deliver every
/// row once and no row twice. The window is half-open on `ingested_at`, so a row that
/// lands exactly on a boundary belongs to the later poll and only to it — which is the
/// one case that is impossible to get right by eye and trivial to get wrong.
#[tokio::test]
async fn consecutive_tail_polls_deliver_every_row_exactly_once() {
    let store = store();
    let tenant = TenantId::new();
    let resource = ResourceId::new();
    let scope = scope_for(tenant);

    let t0 = window().start;
    let t1 = t0 + Duration::seconds(10);
    let t2 = t1 + Duration::seconds(10);

    // Two in the first poll's window, one exactly on the boundary, one in the second's.
    let rows = [
        ingested_at_row(tenant, resource, "first", t0),
        ingested_at_row(tenant, resource, "second", t0 + Duration::seconds(5)),
        ingested_at_row(tenant, resource, "on the boundary", t1),
        ingested_at_row(tenant, resource, "third", t1 + Duration::seconds(5)),
    ];
    store.insert_logs(&rows).await.unwrap();

    let base = Query::new(SignalType::Log, window());
    let first = bodies(&store, &scope, &uops_query::follow(&base, t0, t1)).await;
    let second = bodies(&store, &scope, &uops_query::follow(&base, t1, t2)).await;

    assert_eq!(first, ["second", "first"], "half-open at the end");
    assert_eq!(
        second,
        ["third", "on the boundary"],
        "a row ingested exactly at the watermark belongs to the next poll"
    );
}

/// The reason the window is on `ingested_at` and not on `observed_at`.
///
/// A row is written with a timestamp two minutes older than the moment it reached
/// storage — a WAL segment replayed after a `ClickHouse` restart, or a device whose
/// clock is slow. Windowing on `observed_at` would have the tail skip past it while it
/// was still in flight and never show it. This is the regression test for that, and it
/// fails against the obvious implementation.
#[tokio::test]
async fn a_row_that_arrives_late_still_reaches_the_tail() {
    let store = store();
    let tenant = TenantId::new();
    let resource = ResourceId::new();
    let scope = scope_for(tenant);

    let t0 = window().start + Duration::seconds(600);
    let t1 = t0 + Duration::seconds(10);

    let mut late = ingested_at_row(tenant, resource, "replayed from the WAL", t0);
    late.observed_at = t0 - Duration::minutes(2);
    store.insert_logs(&[late]).await.unwrap();

    let base = Query::new(SignalType::Log, window());
    assert_eq!(
        bodies(&store, &scope, &uops_query::follow(&base, t0, t1)).await,
        ["replayed from the WAL"]
    );
}

/// A tenant cannot tail another tenant's ingest, for the same reason it cannot query it:
/// the predicate is the compiler's, from the scope, on this path too.
#[tokio::test]
async fn a_tail_cannot_see_another_tenants_telemetry() {
    let store = store();
    let mine = TenantId::new();
    let theirs = TenantId::new();
    let resource = ResourceId::new();

    let t0 = window().start + Duration::seconds(1200);
    let t1 = t0 + Duration::seconds(10);

    store
        .insert_logs(&[ingested_at_row(theirs, resource, "not yours", t0)])
        .await
        .unwrap();

    let base = Query::new(SignalType::Log, window());
    let seen = bodies(&store, &scope_for(mine), &uops_query::follow(&base, t0, t1)).await;
    assert!(seen.is_empty(), "{seen:?}");
}

/// A log row that reached storage at `ingested_at`, stamped at the same instant unless a
/// test says otherwise.
fn ingested_at_row(
    tenant: TenantId,
    resource: ResourceId,
    body: &str,
    ingested_at: chrono::DateTime<Utc>,
) -> LogRow {
    LogRow {
        observed_at: ingested_at,
        ingested_at,
        ..log_row(tenant, resource, body, 0)
    }
}

/// One poll of the tail, as bodies in the order it delivered them.
async fn bodies(store: &ChStore, scope: &TenantScope, q: &Query) -> Vec<String> {
    let result = store
        .tail(q, scope, &ResolvedResources::whole_tenant(scope))
        .await
        .unwrap();
    (0..result.len())
        .map(|i| {
            result
                .value(i, "body")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}
