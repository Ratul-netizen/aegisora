//! Login, logout and `/me`, through the real router against a real database.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;
use uops_api::{AppState, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE};
use uops_core::{ActorId, OrgId, Role, Secret, TenantId};
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
    email: String,
    user: ActorId,
}

async fn fixture(slug: &str) -> Fixture {
    let store = store().await;
    let org = OrgId::new();
    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(org.into_uuid())
        .bind(format!("routes-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");

    let tenant = TenantId::new();
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(tenant.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("routes-{slug}"))
        .bind(format!("{slug}-{}", tenant.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");

    // Unique per test run, because login looks up by address across the deployment.
    let email = format!("{slug}-{}@example.com", tenant.into_uuid().simple());
    let hash = password::hash(&Secret::new("correct horse".to_owned())).unwrap();
    let user = store
        .create_user(org, &email, "Route Test", &hash)
        .await
        .expect("user");
    store
        .grant_role(user, tenant, Role::Operator, None)
        .await
        .expect("role");

    Fixture {
        store,
        tenant,
        email,
        user,
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

fn app(store: &PgStore) -> axum::Router {
    uops_api::router(AppState::new(store.clone(), telemetry()))
}

fn login_request(email: &str, pw: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "email": email, "password": pw }).to_string(),
        ))
        .unwrap()
}

/// Every `Set-Cookie` on a response.
fn cookies(response: &axum::response::Response) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::to_owned)
        .collect()
}

fn value_of(cookies: &[String], name: &str) -> Option<String> {
    cookies.iter().find_map(|c| {
        let (pair, _) = c.split_once("; ")?;
        let (found, value) = pair.split_once('=')?;
        (found == name).then(|| value.to_owned())
    })
}

/// Log in and return the two tokens a browser would now be holding.
async fn sign_in(f: &Fixture) -> (String, String) {
    let response = app(&f.store)
        .oneshot(login_request(&f.email, "correct horse"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let set = cookies(&response);
    (
        value_of(&set, SESSION_COOKIE).expect("session cookie"),
        value_of(&set, CSRF_COOKIE).expect("csrf cookie"),
    )
}

#[tokio::test]
async fn login_sets_a_session_cookie_and_a_csrf_cookie_with_the_right_flags() {
    // The flags ARE the mechanism: an script-readable session cookie turns any XSS into
    // an account takeover, and a script-unreadable CSRF token cannot be echoed into the
    // header that makes double-submit work.
    let f = fixture("flags").await;

    let response = app(&f.store)
        .oneshot(login_request(&f.email, "correct horse"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let set = cookies(&response);
    let session = set.iter().find(|c| c.starts_with(SESSION_COOKIE)).unwrap();
    let csrf = set.iter().find(|c| c.starts_with(CSRF_COOKIE)).unwrap();

    assert!(session.contains("HttpOnly"), "{session}");
    assert!(session.contains("Secure"), "{session}");
    assert!(session.contains("SameSite=Lax"), "{session}");

    assert!(
        !csrf.contains("HttpOnly"),
        "the app must be able to read it: {csrf}"
    );
    assert!(csrf.contains("Secure"), "{csrf}");
}

#[tokio::test]
async fn a_wrong_password_and_an_unknown_address_are_the_same_answer() {
    // Account enumeration: if these differ in status, in body, or in header, an
    // attacker can confirm which addresses have accounts here.
    let f = fixture("enumerate").await;

    let wrong = app(&f.store)
        .oneshot(login_request(&f.email, "wrong horse"))
        .await
        .unwrap();
    let unknown = app(&f.store)
        .oneshot(login_request("nobody-at-all@example.com", "wrong horse"))
        .await
        .unwrap();

    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(unknown.status(), wrong.status());
    assert!(cookies(&wrong).is_empty(), "a failed login sets no cookie");
    assert!(cookies(&unknown).is_empty());

    let wrong_body = axum::body::to_bytes(wrong.into_body(), 4096).await.unwrap();
    let unknown_body = axum::body::to_bytes(unknown.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(
        wrong_body, unknown_body,
        "the two failures must be byte-identical"
    );
}

/// The fastest of several attempts. The minimum is the cleanest estimate of what a
/// request actually costs — a mean or a single sample mostly measures scheduler noise,
/// and on a shared CI runner that noise is larger than the signal.
async fn fastest_login(f: &Fixture, email: &str, pw: &str) -> std::time::Duration {
    let mut best = std::time::Duration::MAX;
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let response = app(&f.store)
            .oneshot(login_request(email, pw))
            .await
            .unwrap();
        best = best.min(started.elapsed());
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    best
}

#[tokio::test]
async fn an_unknown_address_does_not_answer_faster_than_a_wrong_password() {
    // The oracle everyone forgets. Returning early when the user does not exist skips
    // Argon2 — deliberately slow — so "no such user" comes back in a millisecond and
    // "wrong password" in twenty. That gap is measurable over the internet and is a
    // reliable way to enumerate which addresses have accounts here.
    //
    // The two are COMPARED rather than each being checked against a fixed floor. An
    // earlier version of this test asserted only that the unknown-address path took at
    // least 5ms, and it passed happily with the short-circuit in place: the router and
    // the database round trip alone cost more than that, so the threshold measured
    // everything except the thing it was protecting.
    let f = fixture("timing").await;

    let unknown = fastest_login(&f, "definitely-nobody@example.com", "x").await;
    let wrong = fastest_login(&f, &f.email, "wrong horse").await;

    // Half is deliberately generous. A short-circuit is not a 20% difference, it is an
    // order of magnitude — Argon2 at 19 MiB against a database round trip.
    assert!(
        unknown * 2 >= wrong,
        "an unknown address answered in {unknown:?} against {wrong:?} for a wrong          password — the absent-user path is short-circuiting past the verification,          which lets an attacker enumerate accounts"
    );
}

#[tokio::test]
async fn a_disabled_account_cannot_log_in_even_with_the_right_password() {
    // And is not told that the password was right, because that is also an answer.
    let f = fixture("disabled-login").await;
    f.store.disable_user(f.user).await.unwrap();

    let response = app(&f.store)
        .oneshot(login_request(&f.email, "correct horse"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(cookies(&response).is_empty());
}

#[tokio::test]
async fn me_lists_the_tenants_the_switcher_is_built_from() {
    let f = fixture("me").await;
    let (session, _) = sign_in(&f).await;

    let response = app(&f.store)
        .oneshot(
            Request::builder()
                .uri("/api/v1/me")
                .header(header::COOKIE, format!("{SESSION_COOKIE}={session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 8192)
        .await
        .unwrap();
    let me: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(me["email"], f.email);
    assert_eq!(me["tenants"][0]["tenant_id"], f.tenant.to_string());
    assert_eq!(me["tenants"][0]["role"], "operator");
    // Whatever else /me grows, it must never carry password material.
    assert!(!body.windows(4).any(|w| w == b"hash"), "{me}");
}

#[tokio::test]
async fn me_without_a_session_is_unauthenticated() {
    let f = fixture("me-anon").await;
    let response = app(&f.store)
        .oneshot(
            Request::builder()
                .uri("/api/v1/me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn logout_without_the_csrf_header_is_refused() {
    // The whole point of double-submit: the browser attaches the cookies for any
    // origin, so the cookie alone proves nothing about who initiated the request.
    let f = fixture("logout-csrf").await;
    let (session, csrf) = sign_in(&f).await;

    let response = app(&f.store)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/logout")
                .header(
                    header::COOKIE,
                    format!("{SESSION_COOKIE}={session}; {CSRF_COOKIE}={csrf}"),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn logout_clears_the_cookies_and_ends_the_session() {
    let f = fixture("logout").await;
    let (session, csrf) = sign_in(&f).await;

    let response = app(&f.store)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/logout")
                .header(
                    header::COOKIE,
                    format!("{SESSION_COOKIE}={session}; {CSRF_COOKIE}={csrf}"),
                )
                .header(CSRF_HEADER, &csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let set = cookies(&response);
    assert!(
        set.iter().all(|c| c.contains("Max-Age=0")),
        "both cookies must be cleared: {set:?}"
    );

    // And the token itself is dead, not merely forgotten by the browser.
    let after = app(&f.store)
        .oneshot(
            Request::builder()
                .uri("/api/v1/me")
                .header(header::COOKIE, format!("{SESSION_COOKIE}={session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_stolen_cookie_without_the_csrf_token_cannot_mutate() {
    // The attacker's exact position: they can make the browser send its cookies, and
    // they cannot read them. Here the session cookie is present and the CSRF cookie is
    // too — what is missing is the header, because their page could never read it.
    let f = fixture("stolen").await;
    let (session, csrf) = sign_in(&f).await;

    let response = app(&f.store)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/logout")
                .header(
                    header::COOKIE,
                    format!("{SESSION_COOKIE}={session}; {CSRF_COOKIE}={csrf}"),
                )
                .header(CSRF_HEADER, "a value the attacker guessed")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // And the session is untouched: a refused CSRF check must not have side effects.
    let still = app(&f.store)
        .oneshot(
            Request::builder()
                .uri("/api/v1/me")
                .header(header::COOKIE, format!("{SESSION_COOKIE}={session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(still.status(), StatusCode::OK);
}
