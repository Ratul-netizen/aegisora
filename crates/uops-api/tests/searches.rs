//! Saved searches, through HTTP.
//!
//! The assertion that matters is the last one: what comes back out of a saved search is
//! posted straight to `POST /api/v1/query` and answers. That is SPEC §M3's "saved
//! searches are stored `Query` ASTs" checked rather than asserted — and it is the same
//! property M4 needs when an alert rule is built from one with no edits.
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
        .bind(format!("search-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");

    let tenant = TenantId::new();
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("search-{slug}"))
        .bind(format!("{slug}-{}", tenant.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");

    let email = format!("{slug}-{}@example.com", tenant.into_uuid().simple());
    let hash = password::hash(&Secret::new("pw".to_owned())).unwrap();
    let user = store
        .create_user(org, &email, "Search Test", &hash)
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

/// The real `ClickHouse`, because the last test in this file runs a saved search rather
/// than only reading it back.
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

    /// What the access log says about this tenant, newest first.
    async fn access_log(&self) -> Vec<(String, Option<i64>)> {
        self.store
            .access_entries(self.tenant, 50)
            .await
            .unwrap()
            .into_iter()
            .map(|e| (e.target, e.row_count))
            .collect()
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

/// A search over the last fifteen minutes, as the Explorer would send it.
fn search(name: &str, term: &str) -> serde_json::Value {
    let end = chrono::Utc::now();
    let start = end - chrono::Duration::minutes(15);
    serde_json::json!({
        "name": name,
        "description": "what an operator wrote down",
        "query": {
            "signal": "log",
            "time": { "start": start.to_rfc3339(), "end": end.to_rfc3339() },
            "resources": { "type": "all" },
            "filter": {
                "op": "text",
                "field": { "field": "body" },
                "mode": "any_token",
                "terms": [term]
            },
            "limit": 100
        }
    })
}

#[tokio::test]
async fn a_search_is_saved_listed_fetched_replaced_and_deleted() {
    let f = fixture("crud", Role::Operator).await;

    let (status, saved) = f
        .call(f.send("POST", "/api/v1/searches", &search("Timeouts", "timeout")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    assert_eq!(saved["signal"], "log", "{saved}");
    let id = saved["id"].as_str().expect("an id").to_owned();

    let (status, list) = f.call(f.get("/api/v1/searches")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().expect("a list").len(), 1, "{list}");

    let (status, one) = f.call(f.get(&format!("/api/v1/searches/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["name"], "Timeouts", "{one}");

    let (status, replaced) = f
        .call(f.send(
            "PUT",
            &format!("/api/v1/searches/{id}"),
            &search("Timeouts and resets", "reset"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["name"], "Timeouts and resets");
    assert_eq!(
        replaced["id"], saved["id"],
        "replacing keeps the search's identity"
    );

    let (status, _) = f
        .call(f.send(
            "DELETE",
            &format!("/api/v1/searches/{id}"),
            &serde_json::Value::Null,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = f.call(f.get(&format!("/api/v1/searches/{id}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The property SPEC §M3 states and M4 depends on: what comes back is the query itself.
///
/// Not a reconstruction of one — the bytes are posted to `/api/v1/query` unchanged, with
/// only the window replaced, exactly as the Explorer does when an operator opens a saved
/// search. If this passes, "a saved search converts to an alert rule with no edits" is a
/// copy rather than a translation.
#[tokio::test]
async fn a_saved_search_runs_as_the_query_it_was_saved_from() {
    let f = fixture("runs", Role::Operator).await;

    let (status, saved) = f
        .call(f.send("POST", "/api/v1/searches", &search("Runnable", "timeout")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");

    let (status, result) = f
        .call(f.send("POST", "/api/v1/query", &saved["query"]))
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["table"], "logs", "{result}");
}

#[tokio::test]
async fn a_query_the_compiler_refuses_is_not_saved() {
    // Refused at the moment somebody clicks Save, with the compiler's wording — rather
    // than stored, listed, and found to be broken by whoever opens it during an
    // incident. `trace` is in the AST and has no table until M8.
    let f = fixture("refused", Role::Operator).await;

    let mut body = search("Traces", "anything");
    body["query"]["signal"] = serde_json::Value::String("trace".into());

    let (status, problem) = f.call(f.send("POST", "/api/v1/searches", &body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(problem["type"], "invalid-input");

    let (_, list) = f.call(f.get("/api/v1/searches")).await;
    assert_eq!(list.as_array().expect("a list").len(), 0, "{list}");
}

#[tokio::test]
async fn a_viewer_may_read_searches_and_may_not_write_them() {
    // A saved search is the team's question, not a personal bookmark: a viewer who could
    // overwrite one could change what a colleague sees during an incident, with nothing
    // on screen to say so. Running any query they like is still theirs.
    let f = fixture("viewer", Role::Viewer).await;

    let (status, _) = f.call(f.get("/api/v1/searches")).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = f
        .call(f.send("POST", "/api/v1/searches", &search("Mine", "timeout")))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_audit_row_names_the_search_and_not_what_it_searches_for() {
    // The terms are the customer's hostnames and error strings. An audit row carrying
    // them would put a second copy of the data in the audit table, which is the opposite
    // of what an audit table is for — the same reason a telemetry read records a
    // fingerprint rather than the query.
    let f = fixture("audit", Role::Operator).await;
    let (_, saved) = f
        .call(f.send(
            "POST",
            "/api/v1/searches",
            &search("Audited", "a-secret-hostname"),
        ))
        .await;

    let actions = f.audit_log().await;
    assert!(
        actions
            .iter()
            .any(|(action, target)| action == "searches.save"
                && target == &format!("search:{}", saved["id"].as_str().unwrap())),
        "{actions:?}"
    );

    let written = f
        .store
        .audit_entries(f.tenant, 50)
        .await
        .unwrap()
        .into_iter()
        .fold(String::new(), |mut acc, e| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{:?}{:?}", e.before, e.after);
            acc
        });
    assert!(
        !written.contains("a-secret-hostname"),
        "the audit log must not carry the search terms: {written}"
    );

    // And reading is recorded too, which is the half nobody notices missing until an
    // auditor asks.
    f.call(f.get("/api/v1/searches")).await;
    let reads = f.access_log().await;
    assert!(
        reads.iter().any(|(target, _)| target == "searches.list"),
        "{reads:?}"
    );
}
