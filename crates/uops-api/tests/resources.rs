//! Resource routes and the audit trail they leave.
//!
//! Two things are under test and they are separable. One is that the verbs work and
//! that a role gate sits in front of each. The other is the one SPEC calls out and
//! nobody notices missing until an auditor asks: that a **read** leaves a row behind,
//! not only a write.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use uops_api::{AppState, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE, TENANT_HEADER};
use uops_core::{OrgId, ResourceId, Role, Secret, TenantId};
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
        .bind(format!("res-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");

    let tenant = TenantId::new();
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("res-{slug}"))
        .bind(format!("{slug}-{}", tenant.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");

    let email = format!("{slug}-{}@example.com", tenant.into_uuid().simple());
    let hash = password::hash(&Secret::new("pw".to_owned())).unwrap();
    let user = store
        .create_user(org, &email, "Res Test", &hash)
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

/// A telemetry store for tests that never query one.
///
/// Constructing it opens no connection — the HTTP client is lazy — so a test that only
/// exercises the control plane costs nothing for holding it. Required rather than
/// optional in `AppState` because an API that cannot answer a query is a different
/// product, not a degraded one.
fn telemetry() -> uops_store_ch::ChStore {
    uops_store_ch::ChStore::new(uops_store_ch::ChClient::new(
        uops_store_ch::ChConfig::from_env(),
    ))
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

#[tokio::test]
async fn a_resource_can_be_created_listed_and_fetched() {
    let f = fixture("crud", Role::Operator).await;

    let (status, created) = f
        .call(f.send(
            "POST",
            "/api/v1/resources",
            &serde_json::json!({ "kind": "device", "name": "rtr-01", "vendor": "cisco" }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, list) = f.call(f.get("/api/v1/resources")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["items"][0]["name"], "rtr-01");
    assert!(list["next"].is_null(), "one page, one item");

    let (status, one) = f.call(f.get(&format!("/api/v1/resources/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["vendor"], "cisco");
}

#[tokio::test]
async fn a_viewer_can_read_but_not_create() {
    let f = fixture("viewer", Role::Viewer).await;

    let (status, _) = f.call(f.get("/api/v1/resources")).await;
    assert_eq!(status, StatusCode::OK);

    let (status, problem) = f
        .call(f.send(
            "POST",
            "/api/v1/resources",
            &serde_json::json!({ "kind": "device", "name": "nope" }),
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        problem["detail"].as_str().unwrap().contains("operator"),
        "the refusal must name the role needed: {problem}"
    );
}

#[tokio::test]
async fn a_mutation_without_the_csrf_header_is_refused() {
    let f = fixture("csrf", Role::Operator).await;

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/resources")
        .header(
            header::COOKIE,
            format!("{SESSION_COOKIE}={}; {CSRF_COOKIE}={}", f.session, f.csrf),
        )
        .header(TENANT_HEADER, f.tenant.to_string())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "kind": "device", "name": "x" }).to_string(),
        ))
        .unwrap();

    let (status, _) = f.call(request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn deleting_a_resource_retires_it_rather_than_removing_it() {
    // A hard delete would orphan every row of telemetry already written under this
    // resource_id, and history that resolves to nothing is worse than a retired row.
    let f = fixture("retire", Role::Operator).await;

    let (_, created) = f
        .call(f.send(
            "POST",
            "/api/v1/resources",
            &serde_json::json!({ "kind": "device", "name": "old-switch" }),
        ))
        .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, retired) = f
        .call(f.send(
            "DELETE",
            &format!("/api/v1/resources/{id}"),
            &serde_json::Value::Null,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retired["status"], "decommissioned");

    // Still readable, which is the point.
    let (status, _) = f.call(f.get(&format!("/api/v1/resources/{id}"))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn an_unknown_resource_is_not_found() {
    let f = fixture("missing", Role::Viewer).await;
    let (status, _) = f
        .call(f.get(&format!("/api/v1/resources/{}", ResourceId::new())))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reading_a_resource_leaves_a_row_in_the_access_log() {
    // SPEC §M0.8, and the reason it is called out separately: defence and
    // law-enforcement buyers audit who SAW what, not only who changed it. Every other
    // system logs the write; this is the one that is usually missing.
    let f = fixture("access", Role::Operator).await;

    let (_, created) = f
        .call(f.send(
            "POST",
            "/api/v1/resources",
            &serde_json::json!({ "kind": "device", "name": "watched" }),
        ))
        .await;
    let id = created["id"].as_str().unwrap().to_owned();

    f.call(f.get(&format!("/api/v1/resources/{id}"))).await;
    f.call(f.get("/api/v1/resources")).await;

    let log = f.access_log().await;
    assert!(
        log.iter()
            .any(|(target, _)| target == &format!("resource:{id}")),
        "reading one resource must be recorded: {log:?}"
    );
    let listing = log
        .iter()
        .find(|(target, _)| target == "resources")
        .expect("listing must be recorded");
    assert_eq!(
        listing.1,
        Some(1),
        "how much came back is part of the record — a listing and an export are \
         different events"
    );
}

#[tokio::test]
async fn a_mutation_leaves_an_audit_row_with_before_and_after() {
    let f = fixture("audit", Role::Operator).await;

    let (_, created) = f
        .call(f.send(
            "POST",
            "/api/v1/resources",
            &serde_json::json!({ "kind": "device", "name": "changing" }),
        ))
        .await;
    let id = created["id"].as_str().unwrap().to_owned();

    f.call(f.send(
        "PATCH",
        &format!("/api/v1/resources/{id}/status"),
        &serde_json::json!({ "status": "maintenance" }),
    ))
    .await;

    let log = f.audit_log().await;
    assert!(
        log.contains(&("resource.create".to_owned(), format!("resource:{id}"))),
        "{log:?}"
    );
    assert!(
        log.contains(&("resource.status".to_owned(), format!("resource:{id}"))),
        "{log:?}"
    );

    let entries = f.store.audit_entries(f.tenant, 50).await.unwrap();
    let change = entries
        .iter()
        .find(|e| e.action == "resource.status")
        .unwrap();
    assert_eq!(
        change.before.as_ref().unwrap()["status"],
        "unknown",
        "the audit row must carry what it was, not only what it became"
    );
    assert_eq!(change.after.as_ref().unwrap()["status"], "maintenance");
    assert!(change.actor.starts_with("user:"), "{}", change.actor);
}

#[tokio::test]
async fn a_refused_request_is_not_recorded_as_a_read() {
    // Otherwise whoever is probing fills the log with their own failures and buries the
    // one successful access that matters.
    let f = fixture("refused", Role::Viewer).await;

    f.call(f.send(
        "POST",
        "/api/v1/resources",
        &serde_json::json!({ "kind": "device", "name": "denied" }),
    ))
    .await;

    let audit = f.audit_log().await;
    assert!(
        audit.is_empty(),
        "a refused mutation is not a mutation: {audit:?}"
    );
}

#[tokio::test]
async fn the_audit_trail_does_not_cross_tenants() {
    // The log is read by an admin of one tenant. If it carried another customer's
    // activity it would be a leak in the very table meant to detect leaks.
    let mine = fixture("audit-mine", Role::Operator).await;
    let theirs = fixture("audit-theirs", Role::Operator).await;

    theirs
        .call(theirs.send(
            "POST",
            "/api/v1/resources",
            &serde_json::json!({ "kind": "device", "name": "not-yours" }),
        ))
        .await;

    assert!(
        mine.audit_log().await.is_empty(),
        "another tenant's mutation appeared in this tenant's audit log"
    );
    assert!(!theirs.audit_log().await.is_empty());
}

/// A site in this fixture's tenant.
async fn site(f: &Fixture, name: &str) -> uops_core::SiteId {
    let id = uops_core::SiteId::new();
    sqlx::query("INSERT INTO site (id, tenant_id, name) VALUES ($1, $2, $3)")
        .bind(id.into_uuid())
        .bind(f.tenant.into_uuid())
        .bind(name)
        .execute(f.store.pool())
        .await
        .expect("site");
    id
}

#[tokio::test]
async fn a_viewer_sees_the_map_and_cannot_change_it() {
    // Reading the estate is what everybody does; placing a site changes what everybody
    // else's map shows, which is an operator's decision.
    let f = fixture("map-viewer", Role::Viewer).await;
    let id = site(&f, "dhaka").await;

    let (status, body) = f.call(f.get("/api/v1/sites")).await;
    assert_eq!(status, StatusCode::OK);
    let sites = body.as_array().expect("an array");
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0]["name"], "dhaka");
    // Absent rather than null: most sites are unplaced and the field is skipped.
    assert!(sites[0].get("location").is_none(), "{:?}", sites[0]);
    assert_eq!(sites[0]["resources"]["total"], 0);

    let (status, _) = f
        .call(f.send(
            "PUT",
            &format!("/api/v1/sites/{id}/location"),
            &serde_json::json!({"location": {"latitude": 23.8103, "longitude": 90.4125}}),
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_operator_places_a_site_and_it_appears_on_the_map() {
    let f = fixture("map-operator", Role::Operator).await;
    let id = site(&f, "chattogram").await;

    let (status, _) = f
        .call(f.send(
            "PUT",
            &format!("/api/v1/sites/{id}/location"),
            &serde_json::json!({"location": {"latitude": 22.3569, "longitude": 91.7832}}),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = f.call(f.get("/api/v1/sites")).await;
    let location = &body.as_array().expect("array")[0]["location"];
    assert!((location["latitude"].as_f64().expect("lat") - 22.3569).abs() < 1e-9);
    assert!((location["longitude"].as_f64().expect("lon") - 91.7832).abs() < 1e-9);

    // And taken off again, which is `null` rather than a missing field: an operator
    // clearing a location is saying something, and an absent key would be the client
    // forgetting to send one.
    let (status, _) = f
        .call(f.send(
            "PUT",
            &format!("/api/v1/sites/{id}/location"),
            &serde_json::json!({ "location": null }),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = f.call(f.get("/api/v1/sites")).await;
    assert!(body.as_array().expect("array")[0].get("location").is_none());
}

#[tokio::test]
async fn a_coordinate_in_the_sea_is_refused_by_the_api() {
    // The constraint is in the schema; what this checks is that it arrives as a 400 with
    // a readable message rather than as a 500 with a constraint name.
    let f = fixture("map-typo", Role::Operator).await;
    let id = site(&f, "atlantis").await;

    let (status, body) = f
        .call(f.send(
            "PUT",
            &format!("/api/v1/sites/{id}/location"),
            &serde_json::json!({"location": {"latitude": 91.0, "longitude": 0.0}}),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("latitude"),
        "the message must say which number was wrong: {body}"
    );
}

#[tokio::test]
async fn placing_a_site_is_in_the_audit_log() {
    // SPEC §M1: an entry for every mutating call. A map that somebody redrew overnight
    // is exactly the change an operator asks "who did that" about.
    let f = fixture("map-audit", Role::Operator).await;
    let id = site(&f, "sylhet").await;

    f.call(f.send(
        "PUT",
        &format!("/api/v1/sites/{id}/location"),
        &serde_json::json!({"location": {"latitude": 24.8949, "longitude": 91.8687}}),
    ))
    .await;

    let audit = f.audit_log().await;
    assert!(
        audit
            .iter()
            .any(|(action, target)| action == "site.location" && target == &format!("site:{id}")),
        "{audit:?}"
    );
}

#[tokio::test]
async fn reading_the_map_is_in_the_access_log() {
    // The half SPEC calls out and nobody notices missing: a *read* leaves a row too.
    let f = fixture("map-access", Role::Viewer).await;
    site(&f, "rajshahi").await;

    f.call(f.get("/api/v1/sites")).await;

    let log = f.access_log().await;
    assert!(
        log.iter()
            .any(|(target, rows)| target == "site.list" && *rows == Some(1)),
        "{log:?}"
    );
}
