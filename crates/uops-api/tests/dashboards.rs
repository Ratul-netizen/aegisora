//! Dashboards, through HTTP.
//!
//! A panel is a `Query` AST and a picture to draw it as — the third thing in this product
//! built on that one type, after the Explorer and alert rules. What these settle is that
//! a dashboard which cannot be drawn is refused while somebody is still looking at the
//! form, rather than becoming an error box on a wall display next to nineteen panels that
//! work.
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
        .bind(format!("dash-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");

    let tenant = TenantId::new();
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("dash-{slug}"))
        .bind(format!("{slug}-{}", tenant.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");

    let email = format!("{slug}-{}@example.com", tenant.into_uuid().simple());
    let hash = password::hash(&Secret::new("pw".to_owned())).unwrap();
    let user = store
        .create_user(org, &email, "Dashboard Test", &hash)
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
}

/// A panel over metrics, drawn as a line.
fn panel(id: &str, title: &str) -> serde_json::Value {
    let end = chrono::Utc::now();
    let start = end - chrono::Duration::hours(1);
    serde_json::json!({
        "id": id,
        "title": title,
        "query": {
            "signal": "metric",
            "time": { "start": start.to_rfc3339(), "end": end.to_rfc3339() },
            "resources": { "type": "all" },
            "aggregations": [{ "func": "avg", "field": { "field": "value" }, "alias": "v" }],
            "group_by": [{ "field": "time_bucket", "seconds": 300 }],
            "limit": 500
        },
        "viz": { "kind": "time_series", "unit": "%" },
        "width": 6,
        "height": 2
    })
}

#[tokio::test]
async fn a_dashboard_is_created_listed_fetched_replaced_and_deleted() {
    let f = fixture("crud", Role::Operator).await;

    let body = serde_json::json!({
        "name": "Core routers",
        "description": "the spine",
        "panels": [panel("p1", "CPU"), panel("p2", "Memory")]
    });

    let (status, created) = f.call(f.send("POST", "/api/v1/dashboards", &body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["panels"].as_array().expect("panels").len(), 2);
    let id = created["id"].as_str().expect("an id").to_owned();

    let (status, list) = f.call(f.get("/api/v1/dashboards")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().expect("a list").len(), 1, "{list}");

    // Adding, moving and removing a panel are all a PUT of the whole document.
    let mut smaller = body.clone();
    smaller["panels"] = serde_json::json!([panel("p2", "Memory")]);
    let (status, replaced) = f
        .call(f.send("PUT", &format!("/api/v1/dashboards/{id}"), &smaller))
        .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["panels"].as_array().expect("panels").len(), 1);
    assert_eq!(replaced["panels"][0]["id"], "p2");
    assert_eq!(
        replaced["id"], created["id"],
        "replacing keeps its identity"
    );

    let (status, _) = f
        .call(f.send(
            "DELETE",
            &format!("/api/v1/dashboards/{id}"),
            &serde_json::Value::Null,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = f.call(f.get(&format!("/api/v1/dashboards/{id}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_panel_whose_query_cannot_be_answered_is_refused() {
    // Stored, this is an error box on a wall display that nobody is looking at closely,
    // next to nineteen panels that work. The refusal names the panel, because a dashboard
    // with twenty of them needs to say which one.
    let f = fixture("refused", Role::Operator).await;

    let mut broken = panel("p1", "Nonsense");
    broken["query"]["filter"] = serde_json::json!({
        "op": "text",
        "field": { "field": "body" },
        "mode": "any_token",
        "terms": ["metrics have no body"]
    });

    let (status, problem) = f
        .call(f.send(
            "POST",
            "/api/v1/dashboards",
            &serde_json::json!({ "name": "Broken", "panels": [broken] }),
        ))
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("Nonsense"),
        "the refusal has to say which panel: {problem}"
    );
}

#[tokio::test]
async fn the_alert_panel_is_the_one_with_no_query() {
    // It reads the alert list, which is the control plane rather than telemetry. A query
    // attached to one is somebody expecting it to filter that list — a misunderstanding
    // worth correcting rather than silently ignoring.
    let f = fixture("alerts-panel", Role::Operator).await;

    let (status, created) = f
        .call(f.send(
            "POST",
            "/api/v1/dashboards",
            &serde_json::json!({
                "name": "Operations",
                "panels": [{
                    "id": "a1",
                    "title": "What is firing",
                    "viz": { "kind": "alerts" },
                    "width": 12,
                    "height": 3
                }]
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let mut confused = panel("a2", "Also firing");
    confused["viz"] = serde_json::json!({ "kind": "alerts" });
    let (status, problem) = f
        .call(f.send(
            "POST",
            "/api/v1/dashboards",
            &serde_json::json!({ "name": "Confused", "panels": [confused] }),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
}

#[tokio::test]
async fn a_drawing_panel_with_no_query_is_refused() {
    let f = fixture("no-query", Role::Operator).await;

    let (status, problem) = f
        .call(f.send(
            "POST",
            "/api/v1/dashboards",
            &serde_json::json!({
                "name": "Empty",
                "panels": [{
                    "id": "p1",
                    "title": "Nothing",
                    "viz": { "kind": "stat" }
                }]
            }),
        ))
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("draw nothing"),
        "{problem}"
    );
}

#[tokio::test]
async fn every_panel_kind_round_trips_with_its_own_options() {
    // A gauge's bounds are part of the panel, not derived from the data: a gauge whose
    // range moves with what it is showing always reads half full.
    let f = fixture("kinds", Role::Operator).await;

    let mut stat = panel("p1", "Now");
    stat["viz"] = serde_json::json!({ "kind": "stat", "unit": "ms", "decimals": 1 });
    let mut gauge = panel("p2", "Utilisation");
    gauge["viz"] = serde_json::json!({ "kind": "gauge", "min": 0.0, "max": 100.0, "unit": "%" });
    let mut table = panel("p3", "Rows");
    table["viz"] = serde_json::json!({ "kind": "table" });

    let (status, created) = f
        .call(f.send(
            "POST",
            "/api/v1/dashboards",
            &serde_json::json!({ "name": "Every kind", "panels": [stat, gauge, table] }),
        ))
        .await;

    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["panels"][0]["viz"]["decimals"], 1);
    assert_eq!(created["panels"][1]["viz"]["max"], 100.0);
    assert_eq!(created["panels"][2]["viz"]["kind"], "table");
}

#[tokio::test]
async fn a_dashboard_larger_than_a_screenful_is_refused() {
    // Every panel is a telemetry query, so the ceiling is also the limit on what one page
    // load asks of ClickHouse.
    let f = fixture("too-big", Role::Operator).await;

    let panels: Vec<serde_json::Value> = (0..41)
        .map(|n| panel(&format!("p{n}"), &format!("Panel {n}")))
        .collect();

    let (status, problem) = f
        .call(f.send(
            "POST",
            "/api/v1/dashboards",
            &serde_json::json!({ "name": "Everything", "panels": panels }),
        ))
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
}

#[tokio::test]
async fn a_viewer_may_read_dashboards_and_may_not_change_them() {
    let f = fixture("viewer", Role::Viewer).await;

    let (status, _) = f.call(f.get("/api/v1/dashboards")).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = f
        .call(f.send(
            "POST",
            "/api/v1/dashboards",
            &serde_json::json!({ "name": "Mine", "panels": [] }),
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
