//! Alert rules and alerts, through HTTP.
//!
//! The assertion that matters is `a_saved_search_becomes_an_alert_rule_with_no_edits`.
//! SPEC §M4 lists it as an acceptance criterion, and it is the one that says whether the
//! decision to build a single `Query` AST in M0 actually paid: the rule is created by
//! posting back the exact bytes the saved-search route returned, with a condition
//! attached and nothing edited.
//!
//! ```bash
//! DATABASE_URL=postgres://uops:uops@localhost:5432/uops //!   CLICKHOUSE_USER=uops CLICKHOUSE_PASSWORD=uops //!   cargo test -p uops-api --test searches
//! ```

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use uops_api::{AppState, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE, TENANT_HEADER};
use uops_core::{OrgId, Role, Secret, TenantId};
use uops_secrets::password;
use uops_store_pg::{Config, PgStore};

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

struct Fixture {
    store: PgStore,
    tenant: TenantId,
    session: String,
    csrf: String,
}

async fn fixture(slug: &str, role: Role) -> Fixture {
    let store = store().await;
    let org = OrgId::new();
    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("alert-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");

    let tenant = TenantId::new();
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("alert-{slug}"))
        .bind(format!("{slug}-{}", tenant.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");

    let email = format!("{slug}-{}@example.com", tenant.into_uuid().simple());
    let hash = password::hash(&Secret::new("pw".to_owned())).unwrap();
    let user = store
        .create_user(org, &email, "Alert Test", &hash)
        .await
        .expect("user");
    store
        .grant_role(user, tenant, role, None)
        .await
        .expect("role");

    let (session, csrf) = sign_in(&store, &email).await;
    Fixture {
        store,
        tenant,
        session,
        csrf,
    }
}

/// The real `ClickHouse`. A rule's query is compiled before it is stored, and the store
/// that would run it is part of the state the router is built with.
fn telemetry() -> uops_store_ch::ChStore {
    uops_store_ch::ChStore::new(uops_store_ch::ChClient::new(uops_store_ch::ChConfig {
        user: std::env::var("CLICKHOUSE_USER").unwrap_or_else(|_| "uops".into()),
        password: std::env::var("CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "uops".into()),
        ..uops_store_ch::ChConfig::from_env()
    }))
}

fn app(store: &PgStore) -> Router {
    uops_api::router(AppState::new(store.clone(), telemetry()))
}

async fn sign_in(store: &PgStore, email: &str) -> (String, String) {
    let response = app(store)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "email": email, "password": "pw" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let mut session = String::new();
    let mut csrf = String::new();
    for value in response.headers().get_all(header::SET_COOKIE) {
        let text = value.to_str().unwrap();
        let (pair, _) = text.split_once("; ").unwrap();
        let (name, v) = pair.split_once('=').unwrap();
        if name == SESSION_COOKIE {
            v.clone_into(&mut session);
        } else if name == CSRF_COOKIE {
            v.clone_into(&mut csrf);
        }
    }
    (session, csrf)
}

impl Fixture {
    fn get(&self, path: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .header(header::COOKIE, format!("{SESSION_COOKIE}={}", self.session))
            .header(TENANT_HEADER, self.tenant.to_string())
            .body(Body::empty())
            .unwrap()
    }

    fn send(&self, method: &str, path: &str, body: &serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(
                header::COOKIE,
                format!(
                    "{SESSION_COOKIE}={}; {CSRF_COOKIE}={}",
                    self.session, self.csrf
                ),
            )
            .header(TENANT_HEADER, self.tenant.to_string())
            .header(CSRF_HEADER, &self.csrf)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn call(&self, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = app(&self.store).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn audit_log(&self) -> Vec<(String, String)> {
        self.store
            .audit_entries(self.tenant, 50)
            .await
            .unwrap()
            .into_iter()
            .map(|e| (e.action, e.target))
            .collect()
    }
}

/// `avg(value) > 90 for 5m` over metrics — SPEC's own example rule.
fn rule(name: &str) -> serde_json::Value {
    let end = chrono::Utc::now();
    let start = end - chrono::Duration::minutes(5);
    serde_json::json!({
        "name": name,
        "query": {
            "signal": "metric",
            "time": { "start": start.to_rfc3339(), "end": end.to_rfc3339() },
            "resources": { "type": "all" },
            "limit": 100
        },
        "condition": {
            "kind": "threshold",
            "op": "gt",
            "value": 90.0,
            "hold_seconds": 300
        },
        "severity": "critical"
    })
}

#[tokio::test]
async fn a_rule_is_created_listed_fetched_replaced_and_deleted() {
    let f = fixture("crud", Role::Operator).await;

    let (status, created) = f
        .call(f.send("POST", "/api/v1/alerts/rules", &rule("CPU hot")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["kind"], "threshold", "{created}");
    assert_eq!(
        created["enabled"], true,
        "a new rule is live unless it says otherwise"
    );
    assert_eq!(created["eval_interval_seconds"], 60);
    let id = created["id"].as_str().expect("an id").to_owned();

    let (status, list) = f.call(f.get("/api/v1/alerts/rules")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().expect("a list").len(), 1, "{list}");

    let mut edited = rule("CPU very hot");
    edited["condition"]["value"] = serde_json::json!(95.0);
    let (status, replaced) = f
        .call(f.send("PUT", &format!("/api/v1/alerts/rules/{id}"), &edited))
        .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["condition"]["value"], 95.0);
    assert_eq!(
        replaced["id"], created["id"],
        "replacing keeps the rule's identity"
    );

    let (status, _) = f
        .call(f.send(
            "DELETE",
            &format!("/api/v1/alerts/rules/{id}"),
            &serde_json::Value::Null,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = f.call(f.get(&format!("/api/v1/alerts/rules/{id}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// SPEC §M4's acceptance criterion, checked rather than asserted.
///
/// The rule is built from the bytes `GET /api/v1/searches/{id}` returned — no
/// translation, no field mapping, no second query language. That is the return on the
/// decision made in M0 to have exactly one AST, and it is only visible here, where both
/// routes are in the same process.
#[tokio::test]
async fn a_saved_search_becomes_an_alert_rule_with_no_edits() {
    let f = fixture("from-search", Role::Operator).await;

    let end = chrono::Utc::now();
    let start = end - chrono::Duration::minutes(15);
    let search = serde_json::json!({
        "name": "Interface errors",
        "query": {
            "signal": "log",
            "time": { "start": start.to_rfc3339(), "end": end.to_rfc3339() },
            "resources": { "type": "all" },
            "filter": {
                "op": "text",
                "field": { "field": "body" },
                "mode": "any_token",
                "terms": ["crc"]
            },
            "limit": 100
        }
    });

    let (status, saved) = f.call(f.send("POST", "/api/v1/searches", &search)).await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");

    // The whole conversion: take the query, add a condition and a severity.
    let from_search = serde_json::json!({
        "name": "Interface errors",
        "query": saved["query"],
        "condition": { "kind": "threshold", "op": "gt", "value": 0.0, "hold_seconds": 300 },
        "severity": "warning"
    });

    let (status, created) = f
        .call(f.send("POST", "/api/v1/alerts/rules", &from_search))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(
        created["query"], saved["query"],
        "the rule holds the search's query byte for byte"
    );
}

#[tokio::test]
async fn a_rule_whose_query_cannot_be_answered_is_refused() {
    // A rule is evaluated by a background task with nobody watching. Stored, this one
    // would produce a log line a minute and an alert that never fires — found during the
    // incident it should have caught.
    let f = fixture("refused", Role::Operator).await;

    let mut broken = rule("Broken");
    broken["query"]["filter"] = serde_json::json!({
        "op": "text",
        "field": { "field": "body" },
        "mode": "any_token",
        "terms": ["metrics have no body"]
    });

    let (status, problem) = f
        .call(f.send("POST", "/api/v1/alerts/rules", &broken))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["type"], "invalid-input");
}

#[tokio::test]
async fn an_absence_rule_needs_no_threshold_to_be_written() {
    // The tagged condition is what makes this expressible without two columns that mean
    // nothing. A flat (op, value, for) triple would have this rule carrying `> 0`.
    let f = fixture("absence", Role::Operator).await;

    let mut absence = rule("Device silent");
    absence["condition"] = serde_json::json!({ "kind": "absence", "after_seconds": 300 });

    let (status, created) = f
        .call(f.send("POST", "/api/v1/alerts/rules", &absence))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["kind"], "absence");
    assert_eq!(created["condition"]["after_seconds"], 300);
    assert!(
        created["condition"].get("value").is_none(),
        "an absence rule has no threshold to show: {created}"
    );
}

#[tokio::test]
async fn an_evaluation_interval_outside_its_bounds_is_refused_rather_than_clamped() {
    // A rule silently evaluating sixty times less often than asked is worse than a
    // refusal: it looks correct in the UI and misses the thing it was written for.
    let f = fixture("interval", Role::Operator).await;

    let mut eager = rule("Too eager");
    eager["eval_interval_seconds"] = serde_json::json!(1);

    let (status, problem) = f.call(f.send("POST", "/api/v1/alerts/rules", &eager)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
}

#[tokio::test]
async fn disabling_a_rule_is_one_request_and_leaves_a_row() {
    // Silencing a rule is what somebody does at 3am. Making them round-trip the whole
    // rule to flip a boolean is how a tired person overwrites a threshold by accident —
    // and six weeks later "who turned this off" is the only question anybody asks.
    let f = fixture("disable", Role::Operator).await;
    let (_, created) = f
        .call(f.send("POST", "/api/v1/alerts/rules", &rule("CPU hot")))
        .await;
    let id = created["id"].as_str().expect("an id").to_owned();

    let (status, off) = f
        .call(f.send(
            "PATCH",
            &format!("/api/v1/alerts/rules/{id}/enabled"),
            &serde_json::json!({ "enabled": false }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{off}");
    assert_eq!(off["enabled"], false);
    // Everything else survived the flip.
    assert_eq!(off["condition"], created["condition"]);

    let actions = f.audit_log().await;
    assert!(
        actions
            .iter()
            .any(|(action, target)| action == "alerts.rule.disable"
                && target == &format!("alert_rule:{id}")),
        "{actions:?}"
    );
}

#[tokio::test]
async fn a_viewer_may_read_rules_and_alerts_and_may_not_change_them() {
    let f = fixture("viewer", Role::Viewer).await;

    let (status, _) = f.call(f.get("/api/v1/alerts/rules")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = f.call(f.get("/api/v1/alerts")).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = f
        .call(f.send("POST", "/api/v1/alerts/rules", &rule("Mine")))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Acknowledgement decides that nobody else needs to be woken up, which is an
    // operational act rather than an annotation.
    let (status, _) = f
        .call(f.send(
            "POST",
            &format!("/api/v1/alerts/{}/ack", uuid::Uuid::now_v7()),
            &serde_json::Value::Null,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// The alert list, and what an acknowledgement does to it.
///
/// The state rows are written through the store rather than by an evaluator, because the
/// evaluator does not exist yet — what is under test here is the API's view of state, and
/// the machine that produces it is tested in `uops-core` without a database.
#[tokio::test]
async fn an_acknowledged_alert_is_still_firing_and_still_listed() {
    let f = fixture("ack", Role::Operator).await;
    let scope = uops_core::TenantScope::collector(f.tenant);

    let (_, created) = f
        .call(f.send("POST", "/api/v1/alerts/rules", &rule("CPU hot")))
        .await;
    let rule_id: uuid::Uuid = created["id"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("uuid");

    let device = f
        .store
        .create_resource(
            &scope,
            &uops_store_pg::NewResource::new(uops_core::ResourceKind::Device, "rtr-01"),
        )
        .await
        .expect("device");

    f.store
        .record_evaluation(
            &scope,
            &uops_store_pg::Evaluated {
                rule_id,
                resource_id: device.id,
                dedup_key: uops_core::alert::dedup_key(rule_id, device.id, []),
                phase: uops_core::alert::Phase::Firing,
                since: chrono::Utc::now(),
                at: chrono::Utc::now(),
                value: Some(97.5),
            },
        )
        .await
        .expect("fire");

    let (status, alerts) = f.call(f.get("/api/v1/alerts")).await;
    assert_eq!(status, StatusCode::OK, "{alerts}");
    let listed = alerts.as_array().expect("a list");
    assert_eq!(listed.len(), 1, "{alerts}");
    assert_eq!(listed[0]["state"], "firing");
    assert_eq!(listed[0]["last_value"], 97.5);
    assert!(listed[0].get("acked_at").is_none(), "{alerts}");

    let alert_id = listed[0]["id"].as_str().expect("an id").to_owned();
    let (status, acked) = f
        .call(f.send(
            "POST",
            &format!("/api/v1/alerts/{alert_id}/ack"),
            &serde_json::Value::Null,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{acked}");
    assert_eq!(
        acked["state"], "firing",
        "an ack silences, it does not resolve"
    );
    assert!(acked["acked_at"].is_string(), "{acked}");

    // And it is still in the list. An acknowledgement that hid the alert would mean the
    // next person to look at the screen concludes the problem went away.
    let (_, after) = f.call(f.get("/api/v1/alerts")).await;
    assert_eq!(after.as_array().expect("a list").len(), 1, "{after}");
}
