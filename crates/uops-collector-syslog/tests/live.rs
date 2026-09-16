//! The whole path, against real infrastructure.
//!
//! A datagram on a real socket, through the real receiver, the real resolver against real
//! `PostgreSQL`, and a real insert into real `ClickHouse` — then read back with a query.
//!
//! This is the test the daemon exists to pass. Every stage has its own unit tests and
//! every one of them passes with the stages wired together wrongly: a resolver that
//! resolves, a normalizer that normalizes and a batcher that batches prove nothing about
//! whether a message sent to port 1514 becomes a row somebody can find.
//!
//! Skipped, loudly, when the infrastructure is not there — the same way the SNMP tests
//! skip without their agent. A test that silently passes because it did nothing is worse
//! than one that is not there.

use std::net::SocketAddr;
use std::time::Duration;

use uops_collector_syslog::config::{Config, Listener};
use uops_collector_syslog::run;
use uops_store_ch::{ChClient, ChStore, TelemetryStore};
use uops_store_pg::{Config as PgConfig, PgStore};

macro_rules! infra_or_skip {
    ($what:expr) => {
        match $what {
            Ok(v) => v,
            Err(e) => {
                println!(
                    "SKIPPED: the infrastructure is not reachable ({e}). Start it with \
                     `docker compose -f deploy/docker-compose.yml up -d`"
                );
                return;
            }
        }
    };
}

fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://uops:uops@localhost:5432/uops".into())
}

async fn store() -> Result<PgStore, String> {
    PgStore::connect(&PgConfig {
        url: database_url(),
        ..PgConfig::default()
    })
    .await
    .map_err(|e| e.to_string())
}

/// A tenant with a slug the listener file can name.
async fn tenant(store: &PgStore, slug: &str) -> (uops_core::TenantId, String) {
    let org = uuid::Uuid::now_v7();
    let id = uops_core::TenantId::new();
    let slug = format!("{slug}-{}", id.into_uuid().simple());

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org)
        .bind(format!("syslogd-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(id.into_uuid())
        .bind(org)
        .bind(format!("syslogd-{slug}"))
        .bind(&slug)
        .execute(store.pool())
        .await
        .expect("tenant");

    (id, slug)
}

/// A config with one UDP listener on an ephemeral port.
fn config(slug: &str, udp: SocketAddr) -> Config {
    Config {
        listeners: vec![Listener {
            tenant: slug.to_owned(),
            udp: Some(udp),
            tcp: None,
            vendor: "cisco".to_owned(),
        }],
        postgres: PgConfig {
            url: database_url(),
            ..PgConfig::default()
        },
        clickhouse: uops_store_ch::ChConfig::from_env(),
        // Small, so the test does not allocate a 200 000-row channel twice per case.
        queue: 4_096,
        workers: 2,
    }
}

/// Wait for a row, rather than sleeping a fixed time and hoping.
///
/// The batcher's deadline is a second and `ClickHouse` needs a moment to make the part
/// visible, so the honest thing is to poll for a bounded time and fail with what was
/// actually found.
async fn wait_for_logs(
    telemetry: &ChStore,
    tenant: uops_core::TenantId,
    contains: &str,
) -> Vec<String> {
    for _ in 0..60 {
        let found = logs(telemetry, tenant, contains).await;
        if !found.is_empty() {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Vec::new()
}

async fn logs(telemetry: &ChStore, tenant: uops_core::TenantId, contains: &str) -> Vec<String> {
    let sql = format!(
        "SELECT body FROM logs WHERE tenant_id = '{}' AND position(body, '{}') > 0 \
         ORDER BY observed_at FORMAT TabSeparated",
        tenant.into_uuid(),
        contains.replace('\'', "''"),
    );
    telemetry
        .client()
        .run(&sql, &[])
        .await
        .map(|raw| {
            raw.body
                .lines()
                .filter(|l| !l.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Bind an ephemeral UDP port and give it back, so the daemon can take it.
///
/// A race in principle. In practice the window is microseconds and the alternative is a
/// hard-coded port that collides with whatever else the machine is running — which is a
/// flake that happens to somebody else, on their machine, with no explanation.
fn free_port() -> SocketAddr {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("an ephemeral port");
    socket.local_addr().expect("its address")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_datagram_becomes_a_row_somebody_can_find() {
    let store = infra_or_skip!(store().await);
    let telemetry = ChStore::new(ChClient::new(uops_store_ch::ChConfig::from_env()));
    infra_or_skip!(telemetry.health().await.map_err(|e| e.to_string()));

    let (tenant_id, slug) = tenant(&store, "live").await;
    let address = free_port();
    let config = config(&slug, address);
    let bound = run::resolve_tenants(&store, &config)
        .await
        .expect("resolve the slug");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
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

    // The daemon binds inside `serve`, so give it a moment before sending.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let marker = format!("live-{}", uuid::Uuid::now_v7().simple());
    let sender = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("a sender");
    sender
        .send_to(
            format!("<34>Oct 11 22:14:15 rtr-live sshd[1234]: {marker}").as_bytes(),
            address,
        )
        .await
        .expect("send");

    let found = wait_for_logs(&telemetry, tenant_id, &marker).await;
    assert!(
        !found.is_empty(),
        "a datagram sent to {address} must become a row in ClickHouse"
    );
    assert!(found[0].contains(&marker), "{found:?}");

    // And the device it came from is now a resource, because nothing was registered in
    // advance. Rule 1 of SPEC M0.2: never block ingestion — an unknown sender gets a
    // provisional resource, not a dropped message.
    let resources = store
        .resources(
            &uops_core::TenantScope::system(tenant_id),
            &uops_store_pg::ResourceFilter::default(),
        )
        .await
        .expect("resources");
    assert!(
        !resources.items.is_empty(),
        "an unknown sender must become a resource, not a dropped message"
    );

    let _ = stop_tx.send(());
    serving.await.expect("join").expect("serve");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_tenants_listening_on_two_ports_do_not_mix() {
    // The whole tenant-attribution decision, tested rather than asserted in a comment.
    // A message's tenant comes from where it arrived, so two listeners must produce two
    // tenants' rows from two identical messages.
    let store = infra_or_skip!(store().await);
    let telemetry = ChStore::new(ChClient::new(uops_store_ch::ChConfig::from_env()));
    infra_or_skip!(telemetry.health().await.map_err(|e| e.to_string()));

    let (first_id, first_slug) = tenant(&store, "mix-a").await;
    let (second_id, second_slug) = tenant(&store, "mix-b").await;
    let (first_addr, second_addr) = (free_port(), free_port());

    let mut config = config(&first_slug, first_addr);
    config.listeners.push(Listener {
        tenant: second_slug,
        udp: Some(second_addr),
        tcp: None,
        vendor: String::new(),
    });
    let bound = run::resolve_tenants(&store, &config)
        .await
        .expect("resolve both slugs");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
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

    // The same hostname, from the same sender address, to two ports. Only the port
    // differs, which is the point: it is the one thing the sender cannot influence.
    let marker = format!("mix-{}", uuid::Uuid::now_v7().simple());
    let message = format!("<34>Oct 11 22:14:15 core-sw-01 app: {marker}");
    let sender = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("a sender");
    for address in [first_addr, second_addr] {
        sender
            .send_to(message.as_bytes(), address)
            .await
            .expect("send");
    }

    assert!(
        !wait_for_logs(&telemetry, first_id, &marker)
            .await
            .is_empty(),
        "the first tenant must have its copy"
    );
    assert!(
        !wait_for_logs(&telemetry, second_id, &marker)
            .await
            .is_empty(),
        "and the second must have its own"
    );

    // Two resources, one per tenant, because `core-sw-01` is two different devices as
    // far as this product is concerned — and every managed-service customer has one.
    for tenant_id in [first_id, second_id] {
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
            "each tenant resolves its own core-sw-01"
        );
    }

    let _ = stop_tx.send(());
    serving.await.expect("join").expect("serve");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_message_is_stored_with_the_reason() {
    // SPEC is explicit and it is the rule the whole crate is shaped by: parse failures
    // are never dropped. The messages a device emits while it is failing are the ones
    // most likely to be malformed, and they are the ones somebody is looking for.
    let store = infra_or_skip!(store().await);
    let telemetry = ChStore::new(ChClient::new(uops_store_ch::ChConfig::from_env()));
    infra_or_skip!(telemetry.health().await.map_err(|e| e.to_string()));

    let (tenant_id, slug) = tenant(&store, "bad").await;
    let address = free_port();
    let config = config(&slug, address);
    let bound = run::resolve_tenants(&store, &config)
        .await
        .expect("resolve");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
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

    let marker = format!("broken-{}", uuid::Uuid::now_v7().simple());
    let sender = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("a sender");
    // No priority at all, which is what `logger -n` and a good deal of firmware emits.
    sender
        .send_to(format!("this has no priority {marker}").as_bytes(), address)
        .await
        .expect("send");

    let found = wait_for_logs(&telemetry, tenant_id, &marker).await;
    assert!(
        !found.is_empty(),
        "a malformed message must still be stored"
    );

    let sql = format!(
        "SELECT attributes['parse.error'] FROM logs \
          WHERE tenant_id = '{}' AND position(body, '{marker}') > 0 FORMAT TabSeparated",
        tenant_id.into_uuid(),
    );
    let reason = telemetry.client().run(&sql, &[]).await.expect("query").body;
    assert!(
        reason.contains("priority"),
        "parse.error must say what could not be read: {reason:?}"
    );

    let _ = stop_tx.send(());
    serving.await.expect("join").expect("serve");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shutdown_writes_what_is_still_in_flight() {
    // The reason the shutdown drains rather than abandoning. A batch is up to 10 000 rows
    // or a second, so a daemon that returned without flushing would lose up to a full
    // batch of somebody's logs on every deploy — routinely, and with nothing to show for
    // it afterwards.
    let store = infra_or_skip!(store().await);
    let telemetry = ChStore::new(ChClient::new(uops_store_ch::ChConfig::from_env()));
    infra_or_skip!(telemetry.health().await.map_err(|e| e.to_string()));

    let (tenant_id, slug) = tenant(&store, "drain").await;
    let address = free_port();
    let config = config(&slug, address);
    let bound = run::resolve_tenants(&store, &config)
        .await
        .expect("resolve");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
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

    let marker = format!("drain-{}", uuid::Uuid::now_v7().simple());
    let sender = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("a sender");
    sender
        .send_to(
            format!("<34>Oct 11 22:14:15 rtr-drain app: {marker}").as_bytes(),
            address,
        )
        .await
        .expect("send");

    // Long enough for the message to be received and normalized, short enough that the
    // batcher's one-second deadline has not fired. The row is in the buffer, not in
    // ClickHouse.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        logs(&telemetry, tenant_id, &marker).await.is_empty(),
        "the premise: the row has not been inserted yet"
    );

    let _ = stop_tx.send(());
    serving.await.expect("join").expect("serve");

    // And after the shutdown it is there, without anybody having waited for a deadline.
    assert!(
        !wait_for_logs(&telemetry, tenant_id, &marker)
            .await
            .is_empty(),
        "a shutdown must write what is buffered"
    );
}
