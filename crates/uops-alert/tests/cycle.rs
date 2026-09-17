//! The scale measurement — SPEC §M4: *"1 000 rules evaluate within one 60 s cycle."*
//!
//! Ignored by default, because it seeds a thousand rules and runs a real cycle against
//! both databases: a minute of wall time and a few thousand round trips is not something
//! to put in front of every `cargo test`.
//!
//! ```bash
//! DATABASE_URL=postgres://uops:uops@localhost:5432/uops \
//!   CLICKHOUSE_USER=uops CLICKHOUSE_PASSWORD=uops \
//!   cargo test -p uops-alert --test cycle -- --ignored --nocapture
//! ```
//!
//! # What it measures, and what that number is worth
//!
//! The path the engine actually takes: `evaluate_and_deliver` per rule, dispatched with
//! the same [`uops_alert::IN_FLIGHT`] bound the run loop uses. Not `Engine::cycle`, which
//! evaluates sequentially — that would measure a design nothing runs.
//!
//! It is one machine's number, on a development database with a single-node `ClickHouse`,
//! and it is a floor rather than a promise: a real installation has more resources per
//! rule and more data per resource. What it is good for is the shape — whether a thousand
//! rules is minutes or seconds, and which of the two stores the time is in.

use std::sync::Arc;
use std::time::Instant;

use chrono::{Duration, Utc};
use tokio::sync::{Mutex, Semaphore};
use uops_alert::{Engine, IN_FLIGHT, Window, evaluate_and_deliver};
use uops_core::alert::{AlertSeverity, Comparison, Condition};
use uops_core::{OrgId, ResourceId, ResourceKind, TenantId, TenantScope};
use uops_notify::Notifier;
use uops_query::{AggFunc, Aggregation, Field, Query, SignalType, TimeRange};
use uops_store_ch::{ChClient, ChConfig, ChStore, MetricRow, MetricStore};
use uops_store_pg::{Config, NewResource, NewRule, PgStore};

/// How many rules SPEC names.
const RULES: usize = 1_000;

/// How many devices they are spread over.
///
/// Fifty, so every evaluation reads a result set with fifty series in it rather than one.
/// A thousand rules over one device would measure a thousand empty queries, which is the
/// number nobody needs.
const DEVICES: usize = 50;

/// The budget the criterion names.
const CYCLE: std::time::Duration = std::time::Duration::from_secs(60);

async fn stores() -> (PgStore, ChStore) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://uops:uops@localhost:5432/uops".into());
    let pg = PgStore::connect(&Config {
        url,
        // The run loop dispatches IN_FLIGHT evaluations at once and each one takes two or
        // three connections' worth of work in sequence. The default pool is smaller than
        // that, and measuring a queue for connections rather than the work itself is the
        // classic way to produce a number that means nothing.
        max_connections: 32,
        ..Config::default()
    })
    .await
    .expect("connect to PostgreSQL");

    let ch = ChStore::new(ChClient::new(ChConfig {
        user: std::env::var("CLICKHOUSE_USER").unwrap_or_else(|_| "uops".into()),
        password: std::env::var("CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "uops".into()),
        ..ChConfig::from_env()
    }));

    (pg, ch)
}

/// A tenant with `DEVICES` devices reporting, and `RULES` rules over them.
///
/// Split out so the measurement below is only the thing being measured.
async fn seed(pg: &PgStore, ch: &ChStore) -> (TenantId, Vec<uuid::Uuid>) {
    let org = OrgId::new();
    let tenant = TenantId::new();
    let unique = tenant.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("cycle-org-{unique}"))
        .execute(pg.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind("cycle")
        .bind(format!("cycle-{unique}"))
        .execute(pg.pool())
        .await
        .expect("tenant");

    let scope = TenantScope::collector(tenant);

    let mut devices = Vec::with_capacity(DEVICES);
    for n in 0..DEVICES {
        devices.push(
            pg.create_resource(
                &scope,
                &NewResource::new(ResourceKind::Device, format!("rtr-{n}")),
            )
            .await
            .expect("device")
            .id,
        );
    }

    // Ten minutes of samples, one a minute per device: enough that every evaluation reads
    // real rows rather than an empty window.
    let start = Utc::now() - Duration::minutes(10);
    let mut rows = Vec::with_capacity(DEVICES * 10);
    for (n, device) in devices.iter().enumerate() {
        for minute in 0..10 {
            rows.push(sample(
                tenant,
                *device,
                // A handful of devices are genuinely hot, so the cycle does the work of
                // firing and writing state rather than only of reading.
                if n % 10 == 0 { 95.0 } else { 20.0 },
                start + Duration::minutes(minute),
            ));
        }
    }
    ch.insert_metrics(&rows).await.expect("insert metrics");

    let mut rule_ids = Vec::with_capacity(RULES);
    for n in 0..RULES {
        rule_ids.push(
            pg.create_rule(&scope, None, &cpu_rule(n))
                .await
                .expect("rule")
                .id,
        );
    }

    (tenant, rule_ids)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "seeds 1 000 rules and runs a real cycle; run it deliberately"]
async fn a_thousand_rules_evaluate_within_one_cycle() {
    let (pg, ch) = stores().await;

    let seeding = Instant::now();
    let (tenant, rule_ids) = seed(&pg, &ch).await;
    let seeded = seeding.elapsed();

    // ---- the cycle --------------------------------------------------------
    //
    // The run loop's own shape: every rule dispatched, IN_FLIGHT of them in flight.
    let engine = Engine::new(pg.clone(), ch.clone());
    let notifier = Notifier::new(pg.clone());
    let window = Arc::new(Mutex::new(Window::default()));
    let permits = Arc::new(Semaphore::new(IN_FLIGHT));
    let latencies: Arc<Mutex<Vec<u128>>> = Arc::new(Mutex::new(Vec::with_capacity(RULES)));

    let cycle = Instant::now();
    let mut tasks = Vec::with_capacity(RULES);
    for rule_id in rule_ids {
        let engine = engine.clone();
        let notifier = notifier.clone();
        let pg = pg.clone();
        let window = Arc::clone(&window);
        let permits = Arc::clone(&permits);
        let latencies = Arc::clone(&latencies);

        tasks.push(tokio::spawn(async move {
            let Ok(_permit) = permits.acquire().await else {
                return;
            };
            let one = Instant::now();
            evaluate_and_deliver(&engine, &notifier, &pg, tenant, rule_id, &window).await;
            latencies.lock().await.push(one.elapsed().as_millis());
        }));
    }
    for task in tasks {
        let _ = task.await;
    }
    let elapsed = cycle.elapsed();

    // ---- what it says -----------------------------------------------------
    let mut latencies = latencies.lock().await.clone();
    latencies.sort_unstable();
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[latencies.len() * 95 / 100];
    let worst = latencies.last().copied().unwrap_or(0);

    println!(
        "\n{RULES} rules over {DEVICES} devices\n  \
         seeded in     {:.1}s\n  \
         cycle         {:.2}s  (budget {:.0}s, {} in flight)\n  \
         per rule      p50 {p50} ms · p95 {p95} ms · worst {worst} ms\n  \
         throughput    {:.0} rules/s\n",
        seeded.as_secs_f64(),
        elapsed.as_secs_f64(),
        CYCLE.as_secs_f64(),
        IN_FLIGHT,
        f64::from(u32::try_from(RULES).unwrap_or(u32::MAX)) / elapsed.as_secs_f64(),
    );

    let evaluated = window.lock().await.evaluated();
    assert_eq!(evaluated, RULES, "every rule was evaluated");
    assert!(
        elapsed < CYCLE,
        "SPEC §M4: 1 000 rules must evaluate inside one 60 s cycle; this took {:.2}s",
        elapsed.as_secs_f64()
    );
}

fn sample(
    tenant: TenantId,
    resource: ResourceId,
    value: f64,
    at: chrono::DateTime<Utc>,
) -> MetricRow {
    MetricRow {
        tenant_id: tenant,
        resource_id: resource,
        site_id: uops_core::SiteId::nil(),
        metric: "system.cpu.utilization".to_owned(),
        observed_at: at,
        ingested_at: at,
        value,
        unit: "1".to_owned(),
        labels: std::collections::BTreeMap::new(),
    }
}

/// `avg(value) > 90 for 5m`, over the last five minutes — SPEC's own example, a thousand
/// times with different names.
fn cpu_rule(n: usize) -> NewRule {
    let end = Utc::now();
    NewRule {
        name: format!("CPU hot {n}"),
        description: String::new(),
        query: Query {
            aggregations: vec![Aggregation {
                func: AggFunc::Avg,
                field: Some(Field::Value),
                alias: "v".to_owned(),
            }],
            ..Query::new(
                SignalType::Metric,
                TimeRange::new(end - Duration::minutes(5), end),
            )
        },
        condition: Condition::Threshold {
            op: Comparison::Gt,
            value: 90.0,
            hold_seconds: 0,
        },
        severity: AlertSeverity::Warning,
        enabled: true,
        eval_interval: Duration::seconds(60),
        // No channels: this measures evaluation, and a thousand rules firing at a webhook
        // would measure the rate limiter refusing them.
        notify: serde_json::json!([]),
    }
}
