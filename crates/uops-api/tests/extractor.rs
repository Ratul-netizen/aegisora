//! The authenticated scope extractor, through a real router and a real database.
//!
//! Unit tests can check that a cookie is parsed. Only this can check the thing that
//! matters: that a request carrying a valid session for one customer, naming another
//! customer's tenant, gets nothing — and gets it as a 404, so the attempt does not even
//! confirm the other tenant exists.
//!
//! Every case here is a refusal except one. That ratio is the point.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use tower::ServiceExt as _;
use uops_api::{AppState, Caller, SESSION_COOKIE, TENANT_HEADER};
use uops_core::{ActorId, OrgId, Role, Secret, SessionId, TenantId};
use uops_secrets::{password, session};
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

/// A handler that exists only to prove the extractor ran and produced a usable scope.
async fn whoami(caller: Caller) -> String {
    format!("{}:{}", caller.tenant_id(), caller.role().as_str())
}

/// A handler behind a role gate.
async fn operators_only(caller: Caller) -> Result<String, uops_api::ApiError> {
    caller.require(Role::Operator)?;
    Ok("ok".to_owned())
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

fn router(store: PgStore) -> Router {
    Router::new()
        .route("/whoami", get(whoami))
        .route("/operators-only", get(operators_only))
        .with_state(AppState::new(store, telemetry()))
}

struct Fixture {
    store: PgStore,
    tenant: TenantId,
    token: String,
    user: ActorId,
    session: SessionId,
}

/// One organization, one tenant, one user with `role`, one live session.
async fn fixture(slug: &str, role: Role) -> Fixture {
    let store = store().await;

    let org = OrgId::new();
    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("api-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");

    let tenant = new_tenant(&store, org, slug).await;

    let hash = password::hash(&Secret::new("pw".to_owned())).unwrap();
    let user = store
        .create_user(org, &format!("{slug}@example.com"), "API Test", &hash)
        .await
        .expect("user");
    store
        .grant_role(user, tenant, role, None)
        .await
        .expect("role");

    let (token, token_hash) = session::issue().unwrap();
    let session = store
        .create_session(user, &token_hash, Some("test"))
        .await
        .expect("session");

    Fixture {
        store,
        tenant,
        token: token.expose().to_owned(),
        user,
        session,
    }
}

async fn new_tenant(store: &PgStore, org: OrgId, slug: &str) -> TenantId {
    let id = TenantId::new();
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(id.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("api-{slug}"))
        .bind(format!("{slug}-{}", id.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");
    id
}

/// Build a request, with whichever of the two credentials the case is testing.
fn request(path: &str, token: Option<&str>, tenant: Option<TenantId>) -> Request<Body> {
    let mut builder = Request::builder().uri(path);
    if let Some(t) = token {
        builder = builder.header(header::COOKIE, format!("{SESSION_COOKIE}={t}"));
    }
    if let Some(t) = tenant {
        builder = builder.header(TENANT_HEADER, t.to_string());
    }
    builder.body(Body::empty()).unwrap()
}

async fn status(store: &PgStore, req: Request<Body>) -> StatusCode {
    router(store.clone()).oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn a_valid_session_on_its_own_tenant_is_the_only_way_through() {
    let f = fixture("happy", Role::Operator).await;

    let response = router(f.store.clone())
        .oneshot(request("/whoami", Some(&f.token), Some(f.tenant)))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(body.to_vec()).unwrap(),
        format!("{}:operator", f.tenant),
        "the handler must receive a scope for the tenant that was named"
    );
    assert_ne!(f.user, ActorId::nil());
    assert_ne!(f.session, SessionId::nil());
}

#[tokio::test]
async fn no_cookie_is_unauthenticated() {
    let f = fixture("nocookie", Role::Viewer).await;
    assert_eq!(
        status(&f.store, request("/whoami", None, Some(f.tenant))).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_token_that_was_never_issued_is_unauthenticated() {
    let f = fixture("badtoken", Role::Viewer).await;
    let (other, _) = session::issue().unwrap();
    assert_eq!(
        status(
            &f.store,
            request("/whoami", Some(other.expose()), Some(f.tenant))
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_request_that_names_no_tenant_is_refused_before_anything_is_read() {
    // Deliberately a 400 and not a default. Guessing "the user's only tenant" would be
    // convenient right up to the first MSP engineer with two customers, at which point
    // it silently guesses wrong.
    let f = fixture("notenant", Role::Viewer).await;
    assert_eq!(
        status(&f.store, request("/whoami", Some(&f.token), None)).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn another_customers_tenant_is_not_found_rather_than_forbidden() {
    // THE test. A valid session, a real tenant, and no role on it. A 403 would confirm
    // to one MSP customer that another exists and at what ID — the inventory leak the
    // whole error model is arranged to avoid.
    let f = fixture("mine", Role::Admin).await;

    let other_org = OrgId::new();
    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(other_org.into_uuid())
        .bind("api-org-theirs")
        .execute(f.store.pool())
        .await
        .unwrap();
    let theirs = new_tenant(&f.store, other_org, "theirs").await;

    assert_eq!(
        status(&f.store, request("/whoami", Some(&f.token), Some(theirs))).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_tenant_in_your_own_organisation_is_still_not_yours() {
    // The subtler version, and the one an MSP hits daily: same organization, same
    // engineer, a customer they were not granted. Being in the same org is not access.
    let f = fixture("sibling", Role::Admin).await;

    let org_of_user =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT org_id FROM app_user WHERE id = $1")
            .bind(f.user.into_uuid())
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    let sibling = new_tenant(&f.store, OrgId::from_uuid(org_of_user), "sibling-other").await;

    assert_eq!(
        status(&f.store, request("/whoami", Some(&f.token), Some(sibling))).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn logging_out_stops_the_next_request() {
    let f = fixture("logout", Role::Viewer).await;
    assert_eq!(
        status(&f.store, request("/whoami", Some(&f.token), Some(f.tenant))).await,
        StatusCode::OK
    );

    f.store.revoke_session(f.session).await.unwrap();

    assert_eq!(
        status(&f.store, request("/whoami", Some(&f.token), Some(f.tenant))).await,
        StatusCode::UNAUTHORIZED,
        "a revoked session must not survive into the next request"
    );
}

#[tokio::test]
async fn disabling_an_account_stops_the_next_request() {
    // Not at the next expiry. An operator disabling an account expects it to take
    // effect now, and the session lookup joins app_user precisely so it does.
    let f = fixture("disabled", Role::Admin).await;
    assert_eq!(
        status(&f.store, request("/whoami", Some(&f.token), Some(f.tenant))).await,
        StatusCode::OK
    );

    f.store.disable_user(f.user).await.unwrap();

    assert_eq!(
        status(&f.store, request("/whoami", Some(&f.token), Some(f.tenant))).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn losing_a_role_stops_the_next_request_without_ending_the_session() {
    // Revoking a role must not require revoking the session: the user may still hold
    // roles on other tenants, and signing them out of everything would be a blunt
    // instrument with its own support cost.
    let f = fixture("revoked-role", Role::Operator).await;
    assert_eq!(
        status(&f.store, request("/whoami", Some(&f.token), Some(f.tenant))).await,
        StatusCode::OK
    );

    f.store.revoke_role(f.user, f.tenant).await.unwrap();

    assert_eq!(
        status(&f.store, request("/whoami", Some(&f.token), Some(f.tenant))).await,
        StatusCode::NOT_FOUND,
        "and it becomes invisible, not forbidden — they no longer hold any role on it"
    );
}

#[tokio::test]
async fn a_viewer_is_forbidden_rather_than_hidden_from_an_operator_route() {
    // The one case where 403 is right: they demonstrably hold a role on this tenant, so
    // its existence is not a secret from them. Telling them which role they lack is
    // what lets them ask for it.
    let f = fixture("viewer", Role::Viewer).await;

    assert_eq!(
        status(&f.store, request("/whoami", Some(&f.token), Some(f.tenant))).await,
        StatusCode::OK,
        "a viewer can still read"
    );
    assert_eq!(
        status(
            &f.store,
            request("/operators-only", Some(&f.token), Some(f.tenant))
        )
        .await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn an_operator_passes_the_operator_gate_and_an_admin_passes_everything() {
    let operator = fixture("gate-op", Role::Operator).await;
    assert_eq!(
        status(
            &operator.store,
            request(
                "/operators-only",
                Some(&operator.token),
                Some(operator.tenant)
            )
        )
        .await,
        StatusCode::OK
    );

    let admin = fixture("gate-admin", Role::Admin).await;
    assert_eq!(
        status(
            &admin.store,
            request("/operators-only", Some(&admin.token), Some(admin.tenant))
        )
        .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_refusal_is_problem_json_and_says_nothing_useful_to_an_attacker() {
    let f = fixture("problem", Role::Viewer).await;
    let other_org = OrgId::new();
    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(other_org.into_uuid())
        .bind("api-org-problem")
        .execute(f.store.pool())
        .await
        .unwrap();
    let theirs = new_tenant(&f.store, other_org, "problem-theirs").await;

    let response = router(f.store.clone())
        .oneshot(request("/whoami", Some(&f.token), Some(theirs)))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(problem["type"], "not-found");
    assert_eq!(problem["status"], 404);
    // The response must not distinguish "this tenant is not yours" from "no such
    // tenant", in the body any more than in the status.
    let text = problem.to_string();
    assert!(!text.contains(&theirs.to_string()), "{text}");
    assert!(!text.to_lowercase().contains("role"), "{text}");
}
