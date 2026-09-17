//! The dashboard load measurement — SPEC §M4: *"dashboard with 20 panels loads p95 < 3 s
//! over 30 days of data (rollups exercised)."*
//!
//! Ignored by default: it writes a month of metrics and then loads a dashboard ten times.
//!
//! ```bash
//! DATABASE_URL=postgres://uops:uops@localhost:5432/uops \
//!   CLICKHOUSE_USER=uops CLICKHOUSE_PASSWORD=uops \
//!   cargo test -p uops-api --test dashboard_load -- --ignored --nocapture
//! ```
//!
//! # What "loads" means here
//!
//! Twenty `POST /api/v1/query` requests through the real router — session, CSRF, tenant
//! resolution, the compiler, `ClickHouse` — issued together, the way a browser does,
//! and timed until the last one answers. That is what a person waits for. Timing one panel
//! and multiplying by twenty would measure a page nobody loads.
//!
//! # "Rollups exercised"
//!
//! The criterion says so explicitly, and it is not automatic: a thirty-day window only
//! reaches `metrics_5m` because the panel asks for wide buckets, which is what
//! `forWindow` in the web app does when the range changes. This test builds its panels the
//! same way — the bucket suits the window — and asserts the answer came from the
//! pre-aggregate rather than from the raw table, because a thirty-day scan of raw points
//! that happened to be fast enough would pass the timing and prove nothing.

use std::collections::BTreeMap;
use std::time::Instant;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{DateTime, Duration, Utc};
use tower::ServiceExt as _;
use uops_api::{AppState, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE, TENANT_HEADER};
use uops_core::{OrgId, ResourceId, ResourceKind, Role, Secret, SiteId, TenantId, TenantScope};
use uops_secrets::password;
use uops_store_ch::{ChClient, ChConfig, ChStore, MetricRow, MetricStore};
use uops_store_pg::{Config, NewResource, PgStore};

/// SPEC's panel count.
const PANELS: usize = 20;
/// How many times the page is loaded.
const LOADS: usize = 10;
/// The budget.
const BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

/// Devices reporting for the month.
const DEVICES: usize = 20;
/// How often each reports. Five minutes, which is what a poller does by default and what
/// makes a month's data a realistic size rather than a synthetic one.
const EVERY_MINUTES: i64 = 5;

async fn stores() -> (PgStore, ChStore) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://uops:uops@localhost:5432/uops".into());
    let pg = PgStore::connect(&Config {
        url,
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

/// A tenant with a month of metrics in it, and a viewer who can read them.
///
/// Split out so the measurement below is only the thing being measured.
async fn seed(pg: &PgStore, ch: &ChStore) -> (TenantId, String, Vec<ResourceId>, usize) {
    let org = OrgId::new();
    let tenant = TenantId::new();
    let unique = tenant.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("load-org-{unique}"))
        .execute(pg.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind("load")
        .bind(format!("load-{unique}"))
        .execute(pg.pool())
        .await
        .expect("tenant");

    let email = format!("load-{unique}@example.com");
    let hash = password::hash(&Secret::new("pw".to_owned())).expect("hash");
    let user = pg
        .create_user(org, &email, "Load Test", &hash)
        .await
        .expect("user");
    pg.grant_role(user, tenant, Role::Viewer, None)
        .await
        .expect("role");

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

    let end = Utc::now();
    let start = end - Duration::days(30);
    let mut written = 0_usize;

    // In batches, because ClickHouse wants large infrequent inserts and a month of
    // five-minute samples for twenty devices is 172 800 rows.
    let mut batch: Vec<MetricRow> = Vec::with_capacity(20_000);
    let mut at = start;
    while at < end {
        for device in &devices {
            batch.push(sample(tenant, *device, at));
        }
        if batch.len() >= 20_000 {
            written += batch.len();
            ch.insert_metrics(&batch).await.expect("insert");
            batch.clear();
        }
        at += Duration::minutes(EVERY_MINUTES);
    }
    if !batch.is_empty() {
        written += batch.len();
        ch.insert_metrics(&batch).await.expect("insert");
    }

    (tenant, email, devices, written)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "writes a month of metrics and loads a dashboard ten times; run it deliberately"]
async fn twenty_panels_over_thirty_days_load_inside_the_budget() {
    let (pg, ch) = stores().await;

    let seeding = Instant::now();
    let (tenant, email, devices, written) = seed(&pg, &ch).await;
    let seeded = seeding.elapsed();

    // The window every panel asks about.
    let end = Utc::now();
    let start = end - Duration::days(30);

    let app = uops_api::router(AppState::new(pg.clone(), ch.clone()));
    let (session, csrf) = sign_in(&pg, &ch, &email).await;

    // The bucket a thirty-day window asks for — the same choice `forWindow` makes in the
    // browser. Twelve hours: 60 buckets over the month, which is what a chart 600 pixels
    // wide can actually show.
    let bucket = 43_200;
    let mut loads = Vec::with_capacity(LOADS);
    let mut table = String::new();

    for _ in 0..LOADS {
        let page = Instant::now();

        let mut requests = Vec::with_capacity(PANELS);
        for panel in 0..PANELS {
            let app = app.clone();
            let request = query_request(
                &session,
                &csrf,
                tenant,
                &panel_query(start, end, bucket, &devices, panel),
            );
            requests.push(tokio::spawn(async move {
                let response = app.oneshot(request).await.expect("response");
                let status = response.status();
                let bytes = axum::body::to_bytes(response.into_body(), 1 << 24)
                    .await
                    .expect("body");
                (status, bytes)
            }));
        }

        for request in requests {
            let (status, bytes) = request.await.expect("panel");
            assert_eq!(
                status,
                StatusCode::OK,
                "{}",
                String::from_utf8_lossy(&bytes)
            );
            let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
            table = body["table"].as_str().unwrap_or_default().to_owned();
        }

        loads.push(page.elapsed());
    }

    loads.sort_unstable();
    let p50 = loads[LOADS / 2];
    let p95 = loads[LOADS * 95 / 100];
    let worst = loads.last().copied().unwrap_or_default();

    println!(
        "\n{PANELS} panels over 30 days · {written} rows from {DEVICES} devices\n  \
         seeded in     {:.1}s\n  \
         page load     p50 {:.2}s · p95 {:.2}s · worst {:.2}s  (budget {:.0}s)\n  \
         answered by   {table}\n",
        seeded.as_secs_f64(),
        p50.as_secs_f64(),
        p95.as_secs_f64(),
        worst.as_secs_f64(),
        BUDGET.as_secs_f64(),
    );

    // "Rollups exercised" is half the criterion: a thirty-day scan of raw points that
    // happened to be fast enough would pass the timing and prove nothing.
    assert_eq!(
        table, "metrics_5m",
        "a thirty-day window must be answered from the pre-aggregate"
    );
    assert!(
        p95 < BUDGET,
        "SPEC §M4: twenty panels over thirty days must load p95 under 3 s; this was {:.2}s",
        p95.as_secs_f64()
    );
}

fn sample(tenant: TenantId, resource: ResourceId, at: DateTime<Utc>) -> MetricRow {
    // A slow sine rather than a constant, so the rollup's averages differ per bucket and
    // the pre-aggregate is doing real work rather than compressing one repeated value.
    // Minutes since the epoch, through a width the mantissa holds exactly. A unix
    // second fits in 32 bits until 2106, and the sine only needs a slowly varying angle.
    let minutes = f64::from(u32::try_from(at.timestamp()).unwrap_or(0)) / 60.0;
    MetricRow {
        tenant_id: tenant,
        resource_id: resource,
        site_id: SiteId::nil(),
        metric: "system.cpu.utilization".to_owned(),
        observed_at: at,
        ingested_at: at,
        value: 50.0 + 40.0 * (minutes / 120.0).sin(),
        unit: "1".to_owned(),
        labels: BTreeMap::new(),
    }
}

/// One panel's query: the average over twelve-hour buckets, for one device.
///
/// Per device rather than tenant-wide, so twenty panels are twenty different questions —
/// a dashboard of twenty identical panels would measure a cache.
fn panel_query(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    bucket: u32,
    devices: &[ResourceId],
    panel: usize,
) -> serde_json::Value {
    let device = devices[panel % devices.len()];
    serde_json::json!({
        "signal": "metric",
        "time": { "start": start.to_rfc3339(), "end": end.to_rfc3339() },
        "resources": { "type": "ids", "ids": [device] },
        "aggregations": [{ "func": "avg", "field": { "field": "value" }, "alias": "v" }],
        "group_by": [{ "field": "time_bucket", "seconds": bucket }],
        "order_by": [{ "key": { "by": "field", "field": { "field": "time_bucket", "seconds": bucket } } }],
        "limit": 500
    })
}

fn query_request(
    session: &str,
    csrf: &str,
    tenant: TenantId,
    body: &serde_json::Value,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/query")
        .header(
            header::COOKIE,
            format!("{SESSION_COOKIE}={session}; {CSRF_COOKIE}={csrf}"),
        )
        .header(TENANT_HEADER, tenant.to_string())
        .header(CSRF_HEADER, csrf)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

async fn sign_in(pg: &PgStore, ch: &ChStore, email: &str) -> (String, String) {
    let app: Router = uops_api::router(AppState::new(pg.clone(), ch.clone()));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "email": email, "password": "pw" }).to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("login");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let mut session = String::new();
    let mut csrf = String::new();
    for value in response.headers().get_all(header::SET_COOKIE) {
        let text = value.to_str().expect("cookie");
        let (pair, _) = text.split_once("; ").expect("cookie attributes");
        let (name, v) = pair.split_once('=').expect("cookie value");
        if name == SESSION_COOKIE {
            v.clone_into(&mut session);
        } else if name == CSRF_COOKIE {
            v.clone_into(&mut csrf);
        }
    }
    (session, csrf)
}
