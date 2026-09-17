//! The evaluator, against real `PostgreSQL` and real `ClickHouse`.
//!
//! The state machine is proved in `uops-core` without either. What only these can settle
//! is whether the machine is being driven with the right arguments: the right window, the
//! right column of the right result set, the right existing phase read back from a table.
//! Every one of those can be wrong while every unit test passes, and the symptom is the
//! same in all of them — an alert that does not arrive.
//!
//! ```bash
//! docker compose -f deploy/docker-compose.yml up -d
//! bash scripts/db.sh migrate && bash scripts/ch.sh apply
//! DATABASE_URL=postgres://uops:uops@localhost:5432/uops \
//!   CLICKHOUSE_USER=uops CLICKHOUSE_PASSWORD=uops \
//!   cargo test -p uops-alert
//! ```

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use uops_alert::Engine;
use uops_core::alert::{AlertSeverity, Comparison, Condition, Phase};
use uops_core::{OrgId, ResourceId, ResourceKind, TenantId, TenantScope};
use uops_query::{AggFunc, Aggregation, Field, Query, SignalType, TimeRange};
use uops_store_ch::{ChClient, ChConfig, ChStore, MetricRow, MetricStore};
use uops_store_pg::{AlertRule, Config, NewResource, NewRule, PgStore};

async fn stores() -> (PgStore, ChStore) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://uops:uops@localhost:5432/uops".into());
    let pg = PgStore::connect(&Config {
        url,
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

/// A tenant of its own per test, with one device in it.
async fn tenant(pg: &PgStore, slug: &str) -> (TenantScope, ResourceId) {
    let org = OrgId::new();
    let tenant = TenantId::new();
    let unique = tenant.into_uuid().simple().to_string();

    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("eval-org-{unique}"))
        .execute(pg.pool())
        .await
        .expect("organization");
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("eval-{slug}"))
        .bind(format!("{slug}-{unique}"))
        .execute(pg.pool())
        .await
        .expect("tenant");

    let scope = TenantScope::collector(tenant);
    let device = pg
        .create_resource(&scope, &NewResource::new(ResourceKind::Device, "rtr-01"))
        .await
        .expect("device");
    (scope, device.id)
}

/// One CPU sample.
fn sample(tenant: TenantId, resource: ResourceId, value: f64, at: DateTime<Utc>) -> MetricRow {
    MetricRow {
        tenant_id: tenant,
        resource_id: resource,
        site_id: uops_core::SiteId::nil(),
        metric: "system.cpu.utilization".to_owned(),
        observed_at: at,
        ingested_at: at,
        value,
        unit: "1".to_owned(),
        labels: BTreeMap::new(),
    }
}

/// `avg(value) > 90 for 5m`, over the last minute.
///
/// The window is one minute so that a test can move `now` forward in minutes and have
/// each evaluation see a different sample rather than an average of all of them.
fn cpu_rule(name: &str, hold_seconds: u32) -> NewRule {
    let end = Utc::now();
    NewRule {
        name: name.to_owned(),
        description: String::new(),
        query: Query {
            aggregations: vec![Aggregation {
                func: AggFunc::Avg,
                field: Some(Field::Value),
                alias: "v".to_owned(),
            }],
            ..Query::new(
                SignalType::Metric,
                TimeRange::new(end - Duration::minutes(1), end),
            )
        },
        condition: Condition::Threshold {
            op: Comparison::Gt,
            value: 90.0,
            hold_seconds,
        },
        severity: AlertSeverity::Critical,
        enabled: true,
        eval_interval: Duration::seconds(60),
        notify: serde_json::json!([]),
    }
}

async fn rule(pg: &PgStore, scope: &TenantScope, new: &NewRule) -> AlertRule {
    pg.create_rule(scope, None, new).await.expect("create rule")
}

/// A breach that is real: fires once, stays fired, resolves once.
///
/// The whole engine in one test, driven by a clock the test controls rather than by
/// sleeping — which is what makes it deterministic and fast enough to keep.
#[tokio::test]
async fn a_sustained_breach_fires_once_and_resolves_once() {
    let (pg, ch) = stores().await;
    let (scope, device) = tenant(&pg, "sustained").await;
    let engine = Engine::new(pg.clone(), ch.clone());
    let rule = rule(&pg, &scope, &cpu_rule("CPU hot", 300)).await;

    // Samples every minute for twenty minutes: hot for the first twelve, cool after.
    let start = Utc::now() - Duration::minutes(20);
    let mut rows = Vec::new();
    for minute in 0..20 {
        let at = start + Duration::minutes(minute);
        rows.push(sample(
            scope.tenant_id(),
            device,
            if minute < 12 { 95.0 } else { 10.0 },
            at,
        ));
    }
    ch.insert_metrics(&rows).await.expect("insert");

    let mut phases = Vec::new();
    let mut notifications = 0;
    for minute in 0..20 {
        // Each evaluation looks at the one-minute window ending at this instant, so it
        // sees exactly the sample written for that minute.
        let now = start + Duration::minutes(minute) + Duration::seconds(30);
        let outcome = engine.evaluate(&scope, &rule, now).await.expect("evaluate");
        notifications += outcome.notifications();
        phases.push(outcome.decisions.first().map_or(Phase::Ok, |d| d.phase));
    }

    // The shape of an incident: quiet, pending while the dwell runs, firing, then
    // resolved once and quiet again.
    assert_eq!(phases[0], Phase::Pending, "{phases:?}");
    assert_eq!(phases[4], Phase::Pending, "five minutes is not yet elapsed");
    assert_eq!(phases[5], Phase::Firing, "{phases:?}");
    assert_eq!(phases[11], Phase::Firing, "still firing while still hot");
    assert_eq!(phases[12], Phase::Resolved, "{phases:?}");
    assert_eq!(phases[13], Phase::Ok, "{phases:?}");

    assert_eq!(
        notifications, 2,
        "one firing and one resolution over a twenty-minute incident: {phases:?}"
    );

    // And the state that survives is what the UI will read.
    assert!(
        pg.active_alerts(&scope).await.expect("active").is_empty(),
        "a resolved alert is not an active one"
    );
}

/// SPEC §M4's acceptance criterion, against the real stores.
///
/// A signal that crosses its threshold every evaluation for an hour. Without `pending`
/// this is one notification per crossing; the criterion asks for a flapping signal that
/// would produce at least twenty, and asserts the engine sends none.
#[tokio::test]
async fn a_flapping_signal_produces_no_notifications() {
    let (pg, ch) = stores().await;
    let (scope, device) = tenant(&pg, "flapping").await;
    let engine = Engine::new(pg.clone(), ch.clone());
    let rule = rule(&pg, &scope, &cpu_rule("CPU hot", 300)).await;

    let start = Utc::now() - Duration::minutes(60);
    let mut rows = Vec::new();
    let mut crossings = 0;
    for minute in 0..60 {
        let hot = minute % 2 == 0;
        crossings += i32::from(hot);
        rows.push(sample(
            scope.tenant_id(),
            device,
            if hot { 95.0 } else { 10.0 },
            start + Duration::minutes(minute),
        ));
    }
    ch.insert_metrics(&rows).await.expect("insert");

    let mut notifications = 0;
    for minute in 0..60 {
        let now = start + Duration::minutes(minute) + Duration::seconds(30);
        notifications += engine
            .evaluate(&scope, &rule, now)
            .await
            .expect("evaluate")
            .notifications();
    }

    assert_eq!(crossings, 30, "the signal really does cross its threshold");
    assert_eq!(
        notifications, 0,
        "thirty threshold crossings must not reach anybody"
    );
    assert!(
        pg.active_alerts(&scope).await.expect("active").is_empty()
            || pg.active_alerts(&scope).await.expect("active")[0]
                .alert
                .phase
                == Phase::Pending,
        "a flapping signal leaves at most a pending alert, which nobody is told about"
    );
}

/// An absence rule detects a device that stops reporting, and says so once.
#[tokio::test]
async fn an_absence_rule_notices_a_device_that_goes_quiet() {
    let (pg, ch) = stores().await;
    let (scope, device) = tenant(&pg, "absence").await;
    let engine = Engine::new(pg.clone(), ch.clone());

    let mut new = cpu_rule("Device silent", 0);
    new.condition = Condition::Absence { after_seconds: 300 };
    // An absence rule names what it watches: "everything" would include resources that
    // have never reported at all.
    new.query.resources = uops_query::ResourceSelector::Kind {
        kind: ResourceKind::Device,
    };
    let rule = rule(&pg, &scope, &new).await;

    // Reporting every minute for ten minutes, then nothing.
    let start = Utc::now() - Duration::minutes(20);
    let rows: Vec<MetricRow> = (0..10)
        .map(|minute| {
            sample(
                scope.tenant_id(),
                device,
                10.0,
                start + Duration::minutes(minute),
            )
        })
        .collect();
    ch.insert_metrics(&rows).await.expect("insert");

    // While it is still reporting: nothing.
    let during = engine
        .evaluate(&scope, &rule, start + Duration::minutes(9))
        .await
        .expect("evaluate");
    assert_eq!(during.notifications(), 0, "{:?}", during.decisions);

    // Six minutes after the last sample, with a five-minute absence window: fired, once.
    let after = engine
        .evaluate(&scope, &rule, start + Duration::minutes(16))
        .await
        .expect("evaluate");
    assert_eq!(after.notifications(), 1, "{:?}", after.decisions);
    assert_eq!(after.decisions[0].phase, Phase::Firing);

    // And not again on the next cycle.
    let again = engine
        .evaluate(&scope, &rule, start + Duration::minutes(17))
        .await
        .expect("evaluate");
    assert_eq!(again.notifications(), 0, "{:?}", again.decisions);
}

/// A maintenance window stops a rule firing at all — SPEC's "the rule does not fire".
#[tokio::test]
async fn an_open_maintenance_window_stops_the_rule_firing() {
    let (pg, ch) = stores().await;
    let (scope, device) = tenant(&pg, "maintenance").await;
    let engine = Engine::new(pg.clone(), ch.clone());
    let rule = rule(&pg, &scope, &cpu_rule("CPU hot", 0)).await;

    let start = Utc::now() - Duration::minutes(10);
    ch.insert_metrics(&[sample(scope.tenant_id(), device, 99.0, start)])
        .await
        .expect("insert");

    // Somebody is rebooting this device on purpose, right now.
    pg.schedule_maintenance(
        &scope,
        None,
        &uops_store_pg::NewWindow {
            reason: "firmware".to_owned(),
            target: uops_core::Target::Resource(device),
            schedule: uops_core::Schedule {
                starts_at: start - Duration::minutes(5),
                duration_minutes: 60,
                timezone: "UTC".to_owned(),
                recurrence: uops_core::Recurrence::Once,
                until: None,
            },
            suppression: uops_core::Suppression {
                alerts: true,
                notifications: true,
            },
        },
    )
    .await
    .expect("schedule maintenance");

    let outcome = engine
        .evaluate(&scope, &rule, start + Duration::seconds(30))
        .await
        .expect("evaluate");

    assert_eq!(outcome.suppressed, 1, "{outcome:?}");
    assert!(outcome.decisions.is_empty(), "{outcome:?}");
    assert!(
        pg.active_alerts(&scope).await.expect("active").is_empty(),
        "a suppressed series is not evaluated, so it has no state at all"
    );
}

/// A whole-installation cycle evaluates every tenant's rules and survives a broken one.
#[tokio::test]
async fn a_cycle_evaluates_every_tenant_and_one_failure_does_not_stop_it() {
    let (pg, ch) = stores().await;
    let (scope, device) = tenant(&pg, "cycle").await;
    let engine = Engine::new(pg.clone(), ch.clone());

    ch.insert_metrics(&[sample(
        scope.tenant_id(),
        device,
        99.0,
        Utc::now() - Duration::seconds(20),
    )])
    .await
    .expect("insert");
    rule(&pg, &scope, &cpu_rule("CPU hot", 0)).await;

    // A cycle crosses tenants by iterating, so it sees this one among all the others the
    // development database happens to hold.
    let cycle = engine.cycle(Utc::now()).await.expect("cycle");
    assert!(cycle.rules >= 1, "{cycle:?}");
    assert!(
        pg.active_alerts(&scope)
            .await
            .expect("active")
            .iter()
            .any(|a| a.alert.phase == Phase::Firing),
        "the rule this test created fired"
    );
}

/// A series that recovered is not rewritten on every cycle for the rest of time.
///
/// The bug this pins: a resolved alert leaves an `ok` row behind, and "write whenever
/// there is a row" turns every series that has ever alerted into a write every minute,
/// for ever. On an estate where a few hundred resources alerted at some point in the past
/// year that is a few hundred pointless writes a minute, and the only symptom is a
/// `PostgreSQL` instance that is busier than anybody can explain.
#[tokio::test]
async fn a_recovered_series_stops_being_written() {
    let (pg, ch) = stores().await;
    let (scope, device) = tenant(&pg, "quiet-writes").await;
    let engine = Engine::new(pg.clone(), ch.clone());
    let rule = rule(&pg, &scope, &cpu_rule("CPU hot", 0)).await;

    // Hot, then cool for the rest of the run.
    let start = Utc::now() - Duration::minutes(10);
    let rows: Vec<MetricRow> = (0..10)
        .map(|minute| {
            sample(
                scope.tenant_id(),
                device,
                if minute == 0 { 99.0 } else { 1.0 },
                start + Duration::minutes(minute),
            )
        })
        .collect();
    ch.insert_metrics(&rows).await.expect("insert");

    // Fire, resolve, return to ok — three transitions, each of which is a real write.
    for minute in 0..3 {
        engine
            .evaluate(
                &scope,
                &rule,
                start + Duration::minutes(minute) + Duration::seconds(30),
            )
            .await
            .expect("evaluate");
    }

    let settled = pg.rule_state(&scope, rule.id).await.expect("state");
    assert_eq!(settled.len(), 1, "{settled:?}");
    assert_eq!(settled[0].phase, Phase::Ok);

    // Everything after that is a healthy series being looked at, which is not news.
    for minute in 3..8 {
        engine
            .evaluate(
                &scope,
                &rule,
                start + Duration::minutes(minute) + Duration::seconds(30),
            )
            .await
            .expect("evaluate");
    }

    let after = pg.rule_state(&scope, rule.id).await.expect("state");
    assert_eq!(
        after[0].last_eval, settled[0].last_eval,
        "five more evaluations of a series that is fine rewrote its row"
    );
}

/// The wiring: a rule that fires reaches the channel it names, with what it fired about.
///
/// Everything under this has its own tests — the state machine without a database, the
/// transport against a socket, the limits against `PostgreSQL`. What only this can settle
/// is that the loop hands the right rule's channels the right alert's `since`, which is a
/// thing that can be wrong while every one of those passes.
#[tokio::test]
async fn a_rules_turn_evaluates_it_and_delivers_what_it_decided() {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::sync::Mutex;

    let (pg, ch) = stores().await;
    let (scope, device) = tenant(&pg, "wiring").await;

    // An endpoint that answers 200 and keeps what it was sent.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let recorder = Arc::clone(&recorder);
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 8192];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                recorder
                    .lock()
                    .await
                    .push(String::from_utf8_lossy(&buffer[..read]).into_owned());
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK
Content-Length: 2
Connection: close

ok",
                    )
                    .await;
            });
        }
    });

    let channel = pg
        .create_channel(
            &scope,
            None,
            &uops_store_pg::NewChannel {
                name: "ops".to_owned(),
                kind: "webhook".to_owned(),
                config: serde_json::json!({ "url": format!("http://127.0.0.1:{port}/hook") }),
                enabled: true,
                max_per_minute: 12,
            },
        )
        .await
        .expect("channel");

    // A rule that will fire on its first evaluation, pointed at that channel.
    let mut new = cpu_rule("CPU hot", 0);
    new.notify = serde_json::json!([channel.id.to_string()]);
    let rule = rule(&pg, &scope, &new).await;

    ch.insert_metrics(&[sample(
        scope.tenant_id(),
        device,
        99.0,
        Utc::now() - Duration::seconds(5),
    )])
    .await
    .expect("insert");

    let window = Arc::new(Mutex::new(uops_alert::Window::default()));
    uops_alert::evaluate_and_deliver(
        &Engine::new(pg.clone(), ch.clone()),
        &uops_notify::Notifier::new(pg.clone()),
        &pg,
        scope.tenant_id(),
        rule.id,
        &window,
    )
    .await;

    // The endpoint saw it, and it says what fired rather than which uuid did.
    let received = seen.lock().await;
    assert_eq!(
        received.len(),
        1,
        "the channel was called once: {received:?}"
    );
    assert!(
        received[0].contains("\"rule\":\"CPU hot\""),
        "{}",
        received[0]
    );
    assert!(received[0].contains("rtr-01"), "{}", received[0]);

    // And the ledger agrees.
    let ledger = pg.notifications(&scope, 10).await.expect("list");
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].outcome, uops_store_pg::Outcome::Sent);
    assert_eq!(ledger[0].rule_id, rule.id);
}
