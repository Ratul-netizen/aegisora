//! Delivery, against a real `PostgreSQL` and a real socket.
//!
//! The receiver is eighty lines of `TcpListener` rather than a mock, for the same reason
//! the SNMP tests use a simulator: what is under test is that a notification leaves this
//! process as an HTTP request somebody else's endpoint would accept, and a mock of our
//! own client proves only that our own client was called.

use std::sync::Arc;

use chrono::Utc;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use uops_core::alert::{AlertSeverity, Phase};
use uops_core::{OrgId, ResourceId, ResourceKind, TenantId, TenantScope};
use uops_notify::{Notification, Notifier};
use uops_store_pg::{Config, NewChannel, NewResource, Outcome, PgStore};

async fn store() -> PgStore {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://uops:uops@localhost:5432/uops".into());
    PgStore::connect(&Config {
        url,
        ..Config::default()
    })
    .await
    .expect("connect")
}

async fn tenant(store: &PgStore, slug: &str) -> (TenantScope, ResourceId) {
    let org = OrgId::new();
    let tenant = TenantId::new();
    let unique = tenant.into_uuid().simple().to_string();

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("deliver-org-{unique}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("deliver-{slug}"))
        .bind(format!("{slug}-{unique}"))
        .execute(store.pool())
        .await
        .expect("tenant");

    let scope = TenantScope::collector(tenant);
    let device = store
        .create_resource(&scope, &NewResource::new(ResourceKind::Device, "rtr-01"))
        .await
        .expect("device");
    (scope, device.id)
}

/// What one request looked like to the endpoint.
#[derive(Clone, Debug, Default)]
struct Received {
    headers: String,
    body: String,
}

/// An endpoint that answers `status` and remembers what it was sent.
async fn endpoint(status: u16) -> (String, Arc<Mutex<Vec<Received>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen: Arc<Mutex<Vec<Received>>> = Arc::new(Mutex::new(Vec::new()));

    let recorder = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let recorder = Arc::clone(&recorder);
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut buffer = [0_u8; 4096];

                // Read until the headers are complete, then until the body is.
                loop {
                    let Ok(n) = socket.read(&mut buffer).await else {
                        return;
                    };
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buffer[..n]);

                    let text = String::from_utf8_lossy(&raw).into_owned();
                    let Some(split) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let head = &text[..split];
                    let body = &text[split + 4..];
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().ok())?
                        })
                        .unwrap_or(0);

                    if body.len() >= length {
                        recorder.lock().await.push(Received {
                            headers: head.to_owned(),
                            body: body.to_owned(),
                        });
                        break;
                    }
                }

                let reason = if status < 300 { "OK" } else { "Bad" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: 7\r\nConnection: close\r\n\r\nthanks!"
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });

    (format!("http://127.0.0.1:{port}/hook"), seen)
}

fn notification(resource: ResourceId, phase: Phase) -> Notification {
    Notification {
        phase,
        severity: AlertSeverity::Critical,
        rule: "CPU hot".to_owned(),
        rule_id: uuid::Uuid::now_v7(),
        resource_id: resource,
        resource: "rtr-01".to_owned(),
        dedup_key: format!("rule/{resource}"),
        value: Some(94.5),
        since: Utc::now(),
        at: Utc::now(),
    }
}

#[tokio::test]
async fn an_alert_reaches_a_webhook_as_json_and_is_recorded_as_sent() {
    let pg = store().await;
    let (scope, device) = tenant(&pg, "sent").await;
    let (url, seen) = endpoint(200).await;

    let channel = pg
        .create_channel(
            &scope,
            None,
            &NewChannel {
                name: "ops".to_owned(),
                kind: "webhook".to_owned(),
                config: serde_json::json!({ "url": url, "headers": { "X-Token": "abc" } }),
                enabled: true,
                max_per_minute: 12,
            },
        )
        .await
        .expect("channel");

    let notifier = Notifier::new(pg.clone());
    let delivered = notifier
        .deliver(
            &scope,
            &serde_json::json!([channel.id.to_string()]),
            &notification(device, Phase::Firing),
        )
        .await
        .expect("deliver");

    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].outcome, Outcome::Sent, "{delivered:?}");

    let received = seen.lock().await;
    assert_eq!(received.len(), 1, "the endpoint was called exactly once");
    // Lowercased on the wire, as HTTP/1.1 field names are — the assertion matches what a
    // receiver actually sees rather than what the config was typed as.
    assert!(
        received[0].headers.to_lowercase().contains("x-token: abc"),
        "the channel's headers are sent: {}",
        received[0].headers
    );

    let body: serde_json::Value = serde_json::from_str(&received[0].body).expect("json");
    assert_eq!(body["phase"], "firing");
    assert_eq!(body["severity"], "critical");
    assert_eq!(body["resource"], "rtr-01", "a name, not a uuid");
    assert!(
        body["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("rtr-01"),
        "{body}"
    );

    // And the row says it happened.
    let recorded = pg.notifications(&scope, 10).await.expect("list");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].outcome, Outcome::Sent);
    assert_eq!(recorded[0].phase, "firing");
}

#[tokio::test]
async fn an_endpoint_that_refuses_is_recorded_with_what_it_said() {
    // An operator debugging a webhook needs the status code, not "delivery failed".
    let pg = store().await;
    let (scope, device) = tenant(&pg, "refused").await;
    let (url, _seen) = endpoint(503).await;

    let channel = pg
        .create_channel(
            &scope,
            None,
            &NewChannel {
                name: "ops".to_owned(),
                kind: "webhook".to_owned(),
                config: serde_json::json!({ "url": url }),
                enabled: true,
                max_per_minute: 12,
            },
        )
        .await
        .expect("channel");

    let delivered = Notifier::new(pg.clone())
        .deliver(
            &scope,
            &serde_json::json!([channel.id.to_string()]),
            &notification(device, Phase::Firing),
        )
        .await
        .expect("deliver");

    assert_eq!(delivered[0].outcome, Outcome::Failed);
    assert!(
        delivered[0].detail.contains("503"),
        "{}",
        delivered[0].detail
    );

    let recorded = pg.notifications(&scope, 10).await.expect("list");
    assert_eq!(recorded[0].outcome, Outcome::Failed);
    assert!(recorded[0].detail.contains("503"), "{}", recorded[0].detail);
}

#[tokio::test]
async fn a_rate_limited_alert_is_recorded_and_never_reaches_the_endpoint() {
    // The limit is in front of the transport, not behind it: a channel that has spent its
    // allowance must not open a connection at all, or the endpoint is still being hit
    // 5 000 times and only the log is quieter.
    let pg = store().await;
    let (scope, device) = tenant(&pg, "limited").await;
    let (url, seen) = endpoint(200).await;

    let channel = pg
        .create_channel(
            &scope,
            None,
            &NewChannel {
                name: "ops".to_owned(),
                kind: "webhook".to_owned(),
                config: serde_json::json!({ "url": url }),
                enabled: true,
                max_per_minute: 2,
            },
        )
        .await
        .expect("channel");

    let notifier = Notifier::new(pg.clone());
    let channels = serde_json::json!([channel.id.to_string()]);
    let mut outcomes = Vec::new();
    for _ in 0..5 {
        outcomes.extend(
            notifier
                .deliver(&scope, &channels, &notification(device, Phase::Firing))
                .await
                .expect("deliver")
                .into_iter()
                .map(|d| d.outcome),
        );
    }

    assert_eq!(outcomes.iter().filter(|o| **o == Outcome::Sent).count(), 2);
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == Outcome::RateLimited)
            .count(),
        3
    );
    assert_eq!(
        seen.lock().await.len(),
        2,
        "the endpoint saw only what the limit allowed"
    );
}

#[tokio::test]
async fn a_channel_deleted_after_the_rule_was_written_is_reported_rather_than_ignored() {
    // The "why was nobody paged" case that leaves no row, because a delivery record
    // belongs to a channel and there is no channel. It has to reach the caller instead.
    let pg = store().await;
    let (scope, device) = tenant(&pg, "gone").await;

    let delivered = Notifier::new(pg.clone())
        .deliver(
            &scope,
            &serde_json::json!([uuid::Uuid::now_v7().to_string()]),
            &notification(device, Phase::Firing),
        )
        .await
        .expect("deliver");

    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].outcome, Outcome::Failed);
    assert!(
        delivered[0].detail.contains("deleted"),
        "{}",
        delivered[0].detail
    );
}

#[tokio::test]
async fn an_unreachable_endpoint_fails_without_taking_the_cycle_with_it() {
    let pg = store().await;
    let (scope, device) = tenant(&pg, "unreachable").await;

    // Port 1 on loopback: nothing listens, and the connection is refused immediately
    // rather than hanging, so this test costs a millisecond rather than the timeout.
    let channel = pg
        .create_channel(
            &scope,
            None,
            &NewChannel {
                name: "ops".to_owned(),
                kind: "webhook".to_owned(),
                config: serde_json::json!({ "url": "http://127.0.0.1:1/hook" }),
                enabled: true,
                max_per_minute: 12,
            },
        )
        .await
        .expect("channel");

    let delivered = Notifier::new(pg.clone())
        .deliver(
            &scope,
            &serde_json::json!([channel.id.to_string()]),
            &notification(device, Phase::Firing),
        )
        .await
        .expect("deliver");

    assert_eq!(delivered[0].outcome, Outcome::Failed);
    assert!(
        delivered[0].detail.contains("could not be reached"),
        "{}",
        delivered[0].detail
    );
}
