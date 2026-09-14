//! SPEC §M1 acceptance: *a user in tenant A cannot read anything from tenant B —
//! verified by an integration test that attempts it on every endpoint, not by
//! inspection.*
//!
//! The words that make this hard to write honestly are **every endpoint**. A file of
//! hand-written cases is inspection wearing a test's clothes: it passes forever, and the
//! twenty-first route added next year is not in it. Nobody notices, because nothing
//! fails.
//!
//! So the route table is read out of `src/routes/mod.rs` at compile time and every route
//! in it must appear in the table below. Adding a route without deciding what isolation
//! means for it breaks this test at the coverage assertion, before any request is sent.
//! That is the same trick `uops-store-pg::enforced` plays on SQL statements, for the
//! same reason: a rule that depends on people remembering is a rule with a half-life.
//!
//! # What "cannot read" is worth testing at
//!
//! Two attacks, and the second is the dangerous one.
//!
//! 1. **Naming someone else's tenant.** `X-Uops-Tenant: <B>` from a user with no role on
//!    B. Expected: 404, never 403 — a tenant you cannot see does not exist, and 403
//!    would confirm that it does.
//!
//! 2. **Naming your own tenant and someone else's object.** `X-Uops-Tenant: <A>` with
//!    B's resource id in the path. This is the one that gets shipped: the tenant check
//!    passes, and whether the object comes back depends on whether the *repository*
//!    filters by tenant rather than on whether the extractor ran. It is exactly what
//!    `TenantScope` and the composite foreign keys exist for, and exactly what a
//!    hand-written test suite tends to leave out.
//!
//! Both must be indistinguishable from asking for something that was never there.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use uops_api::{AppState, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE, TENANT_HEADER};
use uops_core::{OrgId, ResourceId, Role, Secret, TenantId};
use uops_secrets::password;
use uops_store_pg::{Config, NewResource, PgStore};

/// The router's own source. Parsed below, so this test cannot fall behind it.
const ROUTES_SOURCE: &str = include_str!("../src/routes/mod.rs");

// ---------------------------------------------------------------------------
// What isolation means for each route
// ---------------------------------------------------------------------------

/// How a route is expected to behave when it is pointed at another tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expectation {
    /// Scoped to a tenant. Must 404 for a tenant the caller cannot see, and 404 for an
    /// object belonging to one.
    Scoped,
    /// Not about any tenant: authentication itself, or liveness. Cannot leak tenant data
    /// because it is never given a tenant — but it is named here so that adding a route
    /// is a decision rather than an omission.
    Unscoped,
}

struct RouteCase {
    path: &'static str,
    method: &'static str,
    expectation: Expectation,
    /// A body, for the methods that need one.
    body: Option<&'static str>,
}

/// Every route in the router, and what isolation means for it.
///
/// `{id}` is substituted with the *other* tenant's resource id.
const CASES: &[RouteCase] = &[
    RouteCase {
        path: "/api/v1/auth/login",
        method: "POST",
        expectation: Expectation::Unscoped,
        body: Some(r#"{"email":"nobody@example.invalid","password":"x"}"#),
    },
    RouteCase {
        path: "/api/v1/auth/logout",
        method: "POST",
        expectation: Expectation::Unscoped,
        body: None,
    },
    RouteCase {
        path: "/api/v1/me",
        method: "GET",
        expectation: Expectation::Unscoped,
        body: None,
    },
    RouteCase {
        path: "/api/v1/health",
        method: "GET",
        expectation: Expectation::Unscoped,
        body: None,
    },
    RouteCase {
        path: "/api/v1/resources",
        method: "GET",
        expectation: Expectation::Scoped,
        body: None,
    },
    RouteCase {
        path: "/api/v1/resources",
        method: "POST",
        expectation: Expectation::Scoped,
        body: Some(r#"{"kind":"host","name":"intruder","attributes":{}}"#),
    },
    RouteCase {
        path: "/api/v1/resources/{id}",
        method: "GET",
        expectation: Expectation::Scoped,
        body: None,
    },
    RouteCase {
        path: "/api/v1/resources/{id}",
        method: "DELETE",
        expectation: Expectation::Scoped,
        body: None,
    },
    RouteCase {
        path: "/api/v1/resources/{id}/status",
        method: "PATCH",
        expectation: Expectation::Scoped,
        body: Some(r#"{"status":"down"}"#),
    },
    RouteCase {
        path: "/api/v1/query",
        method: "POST",
        expectation: Expectation::Scoped,
        body: Some(
            r#"{"signal":"log","time":{"start":"2026-01-01T00:00:00Z","end":"2026-01-02T00:00:00Z"},"resources":{"type":"all"},"limit":1}"#,
        ),
    },
];

/// Every path the router registers, read from its source.
///
/// Deliberately not a list maintained by hand. `axum::Router` does not expose its
/// routes, so the source is the only place the truth lives.
fn registered_paths() -> Vec<String> {
    let mut paths = Vec::new();
    for (i, _) in ROUTES_SOURCE.match_indices(".route(") {
        let rest = &ROUTES_SOURCE[i..];
        let Some(open) = rest.find('"') else { continue };
        let Some(close) = rest[open + 1..].find('"') else {
            continue;
        };
        let path = &rest[open + 1..open + 1 + close];
        if path.starts_with("/api/") && !paths.iter().any(|p: &String| p == path) {
            paths.push(path.to_owned());
        }
    }
    paths
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

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

fn telemetry() -> uops_store_ch::ChStore {
    uops_store_ch::ChStore::new(uops_store_ch::ChClient::new(
        uops_store_ch::ChConfig::from_env(),
    ))
}

fn app(store: &PgStore) -> Router {
    uops_api::router(AppState::new(store.clone(), telemetry()))
}

struct Party {
    tenant: TenantId,
    session: String,
    csrf: String,
    resource: ResourceId,
}

/// One tenant, one admin on it, one resource in it.
///
/// Admin deliberately — the strongest role there is. A viewer being unable to reach
/// another tenant proves much less than an administrator being unable to: if isolation
/// were a role check rather than a scope, admin is where it would leak.
async fn party(store: &PgStore, slug: &str) -> Party {
    let org = OrgId::new();
    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("iso-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");

    let tenant = TenantId::new();
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("iso-{slug}"))
        .bind(format!("{slug}-{}", tenant.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");

    let email = format!("{slug}-{}@example.invalid", tenant.into_uuid().simple());
    let hash = password::hash(&Secret::new("pw".to_owned())).unwrap();
    let user = store
        .create_user(org, &email, "Isolation Test", &hash)
        .await
        .expect("user");
    store
        .grant_role(user, tenant, Role::Admin, None)
        .await
        .expect("role");

    let scope = uops_core::TenantScope::system(tenant);
    let resource = store
        .create_resource(
            &scope,
            &NewResource::new(uops_core::ResourceKind::Host, format!("{slug}-secret-host")),
        )
        .await
        .expect("resource")
        .id;

    let (session, csrf) = sign_in(store, &email).await;
    Party {
        tenant,
        session,
        csrf,
        resource,
    }
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

/// Send one case as `attacker`, aimed at `tenant` and `victim_resource`.
async fn attempt(
    store: &PgStore,
    attacker: &Party,
    tenant: TenantId,
    victim_resource: ResourceId,
    case: &RouteCase,
) -> (StatusCode, String) {
    let path = case.path.replace("{id}", &victim_resource.to_string());

    let mut builder = Request::builder()
        .method(case.method)
        .uri(&path)
        .header(
            header::COOKIE,
            format!(
                "{SESSION_COOKIE}={}; {CSRF_COOKIE}={}",
                attacker.session, attacker.csrf
            ),
        )
        .header(CSRF_HEADER, &attacker.csrf)
        .header(TENANT_HEADER, tenant.to_string());

    if case.body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }

    let request = builder
        .body(case.body.map_or_else(Body::empty, Body::from))
        .unwrap();

    let response = app(store).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

// ---------------------------------------------------------------------------
// The acceptance test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_route_in_the_router_has_an_isolation_case() {
    // The assertion that makes the rest of this file mean "every endpoint". It runs
    // first because it needs no database and no fixtures: a route added without a
    // decision about isolation should fail here, in a sentence, rather than by a
    // reviewer noticing.
    let registered = registered_paths();
    assert!(
        registered.len() >= 6,
        "the route scanner found only {} paths — it has stopped matching the router",
        registered.len()
    );

    let mut uncovered: Vec<&String> = registered
        .iter()
        .filter(|path| !CASES.iter().any(|c| c.path == path.as_str()))
        .collect();
    uncovered.sort();

    assert!(
        uncovered.is_empty(),
        "these routes exist and no isolation case names them. Add one to CASES and \
         decide whether it is Scoped or Unscoped:\n  {}",
        uncovered
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );

    // And the reverse, so a route that is deleted does not leave a case that tests
    // nothing while appearing to test something.
    let stale: Vec<&str> = CASES
        .iter()
        .map(|c| c.path)
        .filter(|path| !registered.iter().any(|p| p == path))
        .collect();
    assert!(
        stale.is_empty(),
        "these isolation cases name routes that no longer exist: {stale:?}"
    );
}

#[tokio::test]
async fn a_user_cannot_reach_a_tenant_they_have_no_role_on() {
    let store = store().await;
    let a = party(&store, "atkr").await;
    let b = party(&store, "vctm").await;

    for case in CASES {
        if case.expectation != Expectation::Scoped {
            continue;
        }

        // A's session, B's tenant, B's resource. Everything about this request says B
        // except the account making it.
        let (status, body) = attempt(&store, &a, b.tenant, b.resource, case).await;

        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{} {} must be 404 for a tenant the caller cannot see — 403 would confirm \
             the tenant exists, which is itself the leak.\nbody: {body}",
            case.method,
            case.path
        );
        assert!(
            !body.contains("vctm-secret-host"),
            "{} {} returned the other tenant's data:\n{body}",
            case.method,
            case.path
        );
    }
}

#[tokio::test]
async fn a_valid_tenant_header_does_not_unlock_another_tenants_objects() {
    let store = store().await;
    let a = party(&store, "atkr2").await;
    let b = party(&store, "vctm2").await;

    for case in CASES {
        // Only the routes that take an object id. The others cannot express this
        // attack, and asserting 404 for them would assert that a legitimate request
        // fails.
        if case.expectation != Expectation::Scoped || !case.path.contains("{id}") {
            continue;
        }

        // A's own tenant in the header — the extractor is satisfied, the role check
        // passes, and what happens next is entirely up to whether the repository
        // filtered by tenant.
        let (status, body) = attempt(&store, &a, a.tenant, b.resource, case).await;

        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{} {} reached another tenant's object through a valid tenant header. This \
             is the one that ships.\nbody: {body}",
            case.method,
            case.path
        );
        assert!(
            !body.contains("vctm2-secret-host"),
            "{} {} returned the other tenant's data:\n{body}",
            case.method,
            case.path
        );
    }

    // And the object is still there and untouched — a "404" that deleted the row on the
    // way past would satisfy every assertion above.
    let scope = uops_core::TenantScope::system(b.tenant);
    // Unknown is what create_resource leaves it at. The PATCH attempt would have made
    // it Down and the DELETE attempt Decommissioned, so one assertion covers both.
    let still = store
        .resource(&scope, b.resource)
        .await
        .expect("victim row");
    assert_eq!(
        still.status,
        uops_core::ResourceStatus::Unknown,
        "a 404 that changed the row on the way past satisfies every assertion above"
    );
}

#[tokio::test]
async fn the_tenant_list_a_user_is_shown_contains_only_their_own() {
    let store = store().await;
    let a = party(&store, "atkr3").await;
    let b = party(&store, "vctm3").await;

    // /me is Unscoped — it takes no tenant header — which makes it the one route where
    // a leak would not look like a tenant check at all. It is the list the switcher is
    // built from, so anything extra here becomes a tenant the UI offers to open.
    let (status, body) = attempt(
        &store,
        &a,
        a.tenant,
        b.resource,
        &RouteCase {
            path: "/api/v1/me",
            method: "GET",
            expectation: Expectation::Unscoped,
            body: None,
        },
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&a.tenant.to_string()),
        "a user must see their own tenant:\n{body}"
    );
    assert!(
        !body.contains(&b.tenant.to_string()),
        "/me listed a tenant this user has no role on:\n{body}"
    );
}

#[tokio::test]
async fn an_unauthenticated_request_reaches_nothing_scoped() {
    let store = store().await;
    let b = party(&store, "vctm4").await;

    for case in CASES {
        if case.expectation != Expectation::Scoped {
            continue;
        }

        let path = case.path.replace("{id}", &b.resource.to_string());
        let mut builder = Request::builder()
            .method(case.method)
            .uri(&path)
            .header(TENANT_HEADER, b.tenant.to_string());
        if case.body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        let request = builder
            .body(case.body.map_or_else(Body::empty, Body::from))
            .unwrap();

        let response = app(&store).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);

        // 401 or 403 — no session at all, or no CSRF token to echo. Which one depends
        // on the order the extractors run and is not the point; reaching the handler is.
        assert!(
            status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
            "{} {} answered {status} without a session\nbody: {body}",
            case.method,
            case.path
        );
        assert!(
            !body.contains("vctm4-secret-host"),
            "{} {} returned tenant data to an unauthenticated caller:\n{body}",
            case.method,
            case.path
        );
    }
}
