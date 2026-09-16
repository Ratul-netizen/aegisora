//! The whole path, against real infrastructure.
//!
//! A protobuf body on a real socket, through the real resolver against real `PostgreSQL`,
//! into real `ClickHouse` — then read back with a query.
//!
//! The conversion has its own tests in `uops-otlp` and every one of them passes with the
//! stages wired together wrongly. What this proves is that an `otlphttp` exporter pointed
//! at this port produces rows somebody can find.

use std::net::SocketAddr;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::metrics::v1::{
    Gauge, Histogram, HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics,
    metric, number_data_point,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message as _;
use uops_collector_otlp::config::{Config, Listener};
use uops_collector_otlp::run;
use uops_store_ch::{ChClient, ChStore, TelemetryStore};
use uops_store_pg::{Config as PgConfig, PgStore};

macro_rules! infra_or_skip {
    ($what:expr) => {
        match $what {
            Ok(v) => v,
            Err(e) => {
                println!("SKIPPED: the infrastructure is not reachable ({e})");
                return;
            }
        }
    };
}

fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://uops:uops@localhost:5432/uops".into())
}

async fn tenant(store: &PgStore, slug: &str) -> (uops_core::TenantId, String) {
    let org = uuid::Uuid::now_v7();
    let id = uops_core::TenantId::new();
    let slug = format!("{slug}-{}", id.into_uuid().simple());

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org)
        .bind(format!("otlp-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(id.into_uuid())
        .bind(org)
        .bind(format!("otlp-{slug}"))
        .bind(&slug)
        .execute(store.pool())
        .await
        .expect("tenant");

    (id, slug)
}

/// Now, in the nanoseconds OTLP carries.
///
/// **Not a fixed instant.** `metrics` has a 30-day `DELETE` TTL and `logs` a 365-day one,
/// so a fixture dated 2023 is a row `ClickHouse` removes at the next merge — and the merge
/// is background, so the test either fails or passes depending on timing.
///
/// This is the third time this project has been caught by it. The first was the query
/// layer's golden fixtures; the second was a histogram against a pre-aggregated rollup;
/// this was a gauge that never appeared while the log beside it did, because the log's
/// TTL is twelve times longer and the merge had not reached it yet. A fixed timestamp in
/// a test against a table with retention is a test with an expiry date on it.
fn now_nanos() -> u64 {
    u64::try_from(
        chrono::Utc::now()
            .timestamp_nanos_opt()
            .expect("a representable instant"),
    )
    .expect("after 1970")
}

fn free_port() -> SocketAddr {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").expect("an ephemeral port");
    socket.local_addr().expect("its address")
}

fn config(slug: &str, bind: SocketAddr) -> Config {
    Config {
        listeners: vec![Listener {
            tenant: slug.to_owned(),
            bind,
            vendor: String::new(),
        }],
        postgres: PgConfig {
            url: database_url(),
            ..PgConfig::default()
        },
        clickhouse: uops_store_ch::ChConfig::from_env(),
        spill: None,
        queue: 4_096,
        max_body: 4 * 1024 * 1024,
    }
}

fn string(v: &str) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::StringValue(v.to_owned())),
    }
}

fn attribute(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(string(value)),
        ..KeyValue::default()
    }
}

/// What the `OTel` Collector's `resourcedetection` processor produces.
fn resource(host: &str) -> Resource {
    Resource {
        attributes: vec![
            attribute(uops_core::semconv::HOST_NAME, host),
            attribute(uops_core::semconv::HOST_ID, &format!("machine-id-{host}")),
            attribute(uops_core::semconv::SERVICE_NAME, "checkout"),
        ],
        ..Resource::default()
    }
}

/// POST a protobuf body the way `otlphttp` does, without an HTTP client dependency.
///
/// Hand-written because the alternative is pulling reqwest in as a dev dependency for
/// four requests, and this is the whole of HTTP/1.1 that matters here: a request line,
/// two headers, a length and some bytes.
async fn post(address: SocketAddr, path: &str, body: Vec<u8>) -> (u16, Vec<u8>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut socket = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-protobuf\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await.expect("head");
    socket.write_all(&body).await.expect("body");
    socket.flush().await.expect("flush");

    let mut raw = Vec::new();
    socket.read_to_end(&mut raw).await.expect("read");

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a header terminator");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("a status line");

    (status, raw[split + 4..].to_vec())
}

struct Running {
    address: SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<Result<(), String>>,
}

async fn start(store: &PgStore, telemetry: &ChStore, slug: &str) -> Running {
    let address = free_port();
    let config = config(slug, address);
    let bound = run::resolve_tenants(store, &config)
        .await
        .expect("resolve the slug");

    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn({
        let store = store.clone();
        let telemetry = telemetry.clone();
        async move {
            run::serve(store, telemetry, &config, bound, async move {
                let _ = stop_rx.await;
            })
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(250)).await;

    Running {
        address,
        stop,
        serving,
    }
}

async fn query(telemetry: &ChStore, sql: &str) -> String {
    telemetry
        .client()
        .run(sql, &[])
        .await
        .map(|raw| raw.body)
        .unwrap_or_default()
}

async fn wait_for(telemetry: &ChStore, sql: &str) -> String {
    for _ in 0..60 {
        let found = query(telemetry, sql).await;
        if !found.trim().is_empty() {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    String::new()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_otlp_log_export_becomes_a_row_somebody_can_find() {
    let store = infra_or_skip!(
        PgStore::connect(&PgConfig {
            url: database_url(),
            ..PgConfig::default()
        })
        .await
        .map_err(|e| e.to_string())
    );
    let telemetry = ChStore::new(ChClient::new(uops_store_ch::ChConfig::from_env()));
    infra_or_skip!(telemetry.health().await.map_err(|e| e.to_string()));

    let (tenant_id, slug) = tenant(&store, "logs").await;
    let running = start(&store, &telemetry, &slug).await;

    let marker = format!("otlp-{}", uuid::Uuid::now_v7().simple());
    let request = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(resource("app-01")),
            scope_logs: vec![ScopeLogs {
                scope: Some(InstrumentationScope {
                    name: "test".to_owned(),
                    ..InstrumentationScope::default()
                }),
                log_records: vec![LogRecord {
                    time_unix_nano: now_nanos(),
                    severity_number: 17,
                    severity_text: "ERROR".to_owned(),
                    body: Some(string(&marker)),
                    attributes: vec![attribute("db.system", "postgresql")],
                    ..LogRecord::default()
                }],
                ..ScopeLogs::default()
            }],
            ..ResourceLogs::default()
        }],
    };

    let (status, body) = post(running.address, "/v1/logs", request.encode_to_vec()).await;
    assert_eq!(status, 200, "an export must be accepted");

    // A successful export carries no partial_success at all. One populated with zeros is
    // a message some collectors log as a warning every batch.
    let response = ExportLogsServiceResponse::decode(&body[..]).expect("a valid response");
    assert!(
        response.partial_success.is_none(),
        "nothing was rejected: {:?}",
        response.partial_success
    );

    let sql = format!(
        "SELECT body, severity FROM logs WHERE tenant_id = '{}' AND body = '{marker}' \
         FORMAT TabSeparated",
        tenant_id.into_uuid()
    );
    let found = wait_for(&telemetry, &sql).await;
    assert!(
        found.contains(&marker),
        "the row must be queryable: {found:?}"
    );
    assert!(
        found.contains("error"),
        "the severity must survive: {found:?}"
    );

    // And the emitter became a resource, because nothing was registered in advance.
    let resources = store
        .resources(
            &uops_core::TenantScope::system(tenant_id),
            &uops_store_pg::ResourceFilter::default(),
        )
        .await
        .expect("resources");
    assert_eq!(
        resources.items.len(),
        1,
        "an unknown emitter becomes one resource, not none and not several"
    );

    let _ = running.stop.send(());
    running.serving.await.expect("join").expect("serve");
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_are_stored_and_what_cannot_be_is_reported() {
    // Both halves at once, because the interesting property is that they travel together:
    // a request carrying a gauge and a histogram stores the gauge and *says* the histogram
    // was rejected. A receiver that answered 200 {} would be lying by omission.
    let store = infra_or_skip!(
        PgStore::connect(&PgConfig {
            url: database_url(),
            ..PgConfig::default()
        })
        .await
        .map_err(|e| e.to_string())
    );
    let telemetry = ChStore::new(ChClient::new(uops_store_ch::ChConfig::from_env()));
    infra_or_skip!(telemetry.health().await.map_err(|e| e.to_string()));

    let (tenant_id, slug) = tenant(&store, "metrics").await;
    let running = start(&store, &telemetry, &slug).await;

    let metric_name = format!("test.gauge.{}", uuid::Uuid::now_v7().simple());
    let request = ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(resource("host-02")),
            scope_metrics: vec![ScopeMetrics {
                metrics: vec![
                    Metric {
                        name: metric_name.clone(),
                        unit: "1".to_owned(),
                        data: Some(metric::Data::Gauge(Gauge {
                            data_points: vec![NumberDataPoint {
                                time_unix_nano: now_nanos(),
                                value: Some(number_data_point::Value::AsDouble(0.42)),
                                attributes: vec![attribute("cpu", "0")],
                                ..NumberDataPoint::default()
                            }],
                        })),
                        ..Metric::default()
                    },
                    Metric {
                        name: "http.server.duration".to_owned(),
                        data: Some(metric::Data::Histogram(Histogram {
                            data_points: vec![HistogramDataPoint::default()],
                            ..Histogram::default()
                        })),
                        ..Metric::default()
                    },
                ],
                ..ScopeMetrics::default()
            }],
            ..ResourceMetrics::default()
        }],
    };

    let (status, body) = post(running.address, "/v1/metrics", request.encode_to_vec()).await;
    assert_eq!(status, 200);

    let response = ExportMetricsServiceResponse::decode(&body[..]).expect("a valid response");
    let partial = response
        .partial_success
        .expect("the histogram must be reported as rejected");
    assert_eq!(partial.rejected_data_points, 1);
    assert!(
        partial.error_message.contains("histogram"),
        "the reason must name what was not stored: {:?}",
        partial.error_message
    );

    let sql = format!(
        "SELECT metric, value FROM metrics WHERE tenant_id = '{}' AND metric = '{metric_name}' \
         FORMAT TabSeparated",
        tenant_id.into_uuid()
    );
    let found = wait_for(&telemetry, &sql).await;
    assert!(
        found.contains(&metric_name) && found.contains("0.42"),
        "the gauge must be queryable: {found:?}"
    );

    let _ = running.stop.send(());
    running.serving.await.expect("join").expect("serve");
}

#[tokio::test(flavor = "multi_thread")]
async fn traces_are_accepted_and_said_to_be_discarded() {
    // SPEC: accept and drop with a counter, so instrumented apps do not error. An
    // exporter that got a 404 would retry, back off and log an error every batch forever
    // — "traces are not stored yet" and "the endpoint is broken" must not look the same.
    let store = infra_or_skip!(
        PgStore::connect(&PgConfig {
            url: database_url(),
            ..PgConfig::default()
        })
        .await
        .map_err(|e| e.to_string())
    );
    let telemetry = ChStore::new(ChClient::new(uops_store_ch::ChConfig::from_env()));
    infra_or_skip!(telemetry.health().await.map_err(|e| e.to_string()));

    let (_tenant_id, slug) = tenant(&store, "traces").await;
    let running = start(&store, &telemetry, &slug).await;

    let request = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(resource("app-03")),
            scope_spans: vec![ScopeSpans {
                spans: vec![Span::default(), Span::default()],
                ..ScopeSpans::default()
            }],
            ..ResourceSpans::default()
        }],
    };

    let (status, body) = post(running.address, "/v1/traces", request.encode_to_vec()).await;
    assert_eq!(status, 200, "an exporter must not see an error");

    let response = ExportTraceServiceResponse::decode(&body[..]).expect("a valid response");
    let partial = response
        .partial_success
        .expect("spans that were discarded must be reported as such");
    assert_eq!(partial.rejected_spans, 2, "spans are counted, not requests");
    assert!(
        partial.error_message.contains("not stored"),
        "{:?}",
        partial.error_message
    );

    let _ = running.stop.send(());
    running.serving.await.expect("join").expect("serve");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_that_is_not_protobuf_is_the_senders_problem() {
    // 400, not 500: retrying it will produce the same result, and an exporter that backs
    // off on a 400 is wasting its own time.
    let store = infra_or_skip!(
        PgStore::connect(&PgConfig {
            url: database_url(),
            ..PgConfig::default()
        })
        .await
        .map_err(|e| e.to_string())
    );
    let telemetry = ChStore::new(ChClient::new(uops_store_ch::ChConfig::from_env()));
    infra_or_skip!(telemetry.health().await.map_err(|e| e.to_string()));

    let (_tenant_id, slug) = tenant(&store, "bad").await;
    let running = start(&store, &telemetry, &slug).await;

    // Valid protobuf framing is forgiving, so this is bytes that cannot be a message:
    // field 1 with a length longer than the body.
    let (status, _) = post(
        running.address,
        "/v1/logs",
        vec![0x0a, 0xff, 0xff, 0xff, 0x7f, 0x01],
    )
    .await;
    assert_eq!(status, 400, "a malformed body is the sender's problem");

    let _ = running.stop.send(());
    running.serving.await.expect("join").expect("serve");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_tenants_on_two_ports_do_not_mix() {
    // The tenant-attribution decision, tested rather than asserted in a comment. The
    // request bodies are identical; only the port differs, which is the one thing the
    // sender cannot influence.
    let store = infra_or_skip!(
        PgStore::connect(&PgConfig {
            url: database_url(),
            ..PgConfig::default()
        })
        .await
        .map_err(|e| e.to_string())
    );
    let telemetry = ChStore::new(ChClient::new(uops_store_ch::ChConfig::from_env()));
    infra_or_skip!(telemetry.health().await.map_err(|e| e.to_string()));

    let (first_id, first_slug) = tenant(&store, "mix-a").await;
    let (second_id, second_slug) = tenant(&store, "mix-b").await;

    let (first_addr, second_addr) = (free_port(), free_port());
    let mut config = config(&first_slug, first_addr);
    config.listeners.push(Listener {
        tenant: second_slug,
        bind: second_addr,
        vendor: String::new(),
    });
    let bound = run::resolve_tenants(&store, &config)
        .await
        .expect("resolve both slugs");

    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn({
        let store = store.clone();
        let telemetry = telemetry.clone();
        let config = config.clone();
        async move {
            run::serve(store, telemetry, &config, bound, async move {
                let _ = stop_rx.await;
            })
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(250)).await;

    let marker = format!("mix-{}", uuid::Uuid::now_v7().simple());
    let request = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(resource("shared-name")),
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord {
                    time_unix_nano: now_nanos(),
                    severity_number: 9,
                    body: Some(string(&marker)),
                    ..LogRecord::default()
                }],
                ..ScopeLogs::default()
            }],
            ..ResourceLogs::default()
        }],
    };
    let encoded = request.encode_to_vec();

    for address in [first_addr, second_addr] {
        let (status, _) = post(address, "/v1/logs", encoded.clone()).await;
        assert_eq!(status, 200);
    }

    for tenant_id in [first_id, second_id] {
        let sql = format!(
            "SELECT body FROM logs WHERE tenant_id = '{}' AND body = '{marker}' \
             FORMAT TabSeparated",
            tenant_id.into_uuid()
        );
        assert!(
            wait_for(&telemetry, &sql).await.contains(&marker),
            "each tenant must have its own copy"
        );

        // And its own resource. `shared-name` and its machine id are the same bytes in
        // both requests, and every managed-service customer has an `app-01`.
        let resources = store
            .resources(
                &uops_core::TenantScope::system(tenant_id),
                &uops_store_pg::ResourceFilter::default(),
            )
            .await
            .expect("resources");
        assert_eq!(resources.items.len(), 1);
    }

    let _ = stop.send(());
    serving.await.expect("join").expect("serve");
}
