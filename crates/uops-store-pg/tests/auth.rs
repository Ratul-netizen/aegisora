//! Users, roles and sessions against a real PostgreSQL.
//!
//! This is the layer `TenantScope::from_authenticated` will sit on top of, so the
//! assertions here are the foundation of every isolation guarantee above them. The one
//! that matters most is the least dramatic: `role_for` returning `None` for a tenant the
//! user has no role on, which is what turns a request for another customer's data into a
//! 404 before any repository is reached.

use chrono::{Duration, Utc};
use uops_core::{ActorId, OrgId, Role, TenantId};
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

async fn org(store: &PgStore, slug: &str) -> OrgId {
    let id = OrgId::new();
    sqlx::query("INSERT INTO organization (id, name) VALUES ($1, $2)")
        .bind(id.into_uuid())
        .bind(format!("auth-org-{slug}"))
        .execute(store.pool())
        .await
        .expect("organization");
    id
}

async fn tenant(store: &PgStore, org: OrgId, slug: &str) -> TenantId {
    let id = TenantId::new();
    sqlx::query("INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)")
        .bind(id.into_uuid())
        .bind(org.into_uuid())
        .bind(format!("auth-{slug}"))
        .bind(format!("{slug}-{}", id.into_uuid().simple()))
        .execute(store.pool())
        .await
        .expect("tenant");
    id
}

async fn user(store: &PgStore, org: OrgId, email: &str, plaintext: &str) -> ActorId {
    let hash = password::hash(&uops_core::Secret::new(plaintext.to_owned())).unwrap();
    store
        .create_user(org, email, "Test User", &hash)
        .await
        .expect("user")
}

/// Move a session's clock, to test expiry without waiting twelve hours.
async fn backdate_session(store: &PgStore, user: ActorId, created: Duration, expires: Duration) {
    sqlx::query(
        "UPDATE session SET created_at = now() - $2, expires_at = now() + $3 WHERE user_id = $1",
    )
    .bind(user.into_uuid())
    .bind(created)
    .bind(expires)
    .execute(store.pool())
    .await
    .expect("backdate");
}

#[tokio::test]
async fn a_password_round_trips_through_storage() {
    let store = store().await;
    let o = org(&store, "roundtrip").await;
    let id = user(&store, o, "ratul@example.com", "correct horse").await;

    let found = store
        .user_credentials(o, "ratul@example.com")
        .await
        .unwrap()
        .expect("the user exists");

    assert_eq!(found.user_id, id);
    assert!(!found.disabled);
    assert!(password::verify(
        &uops_core::Secret::new("correct horse".to_owned()),
        &found.password_hash
    ));
    assert!(!password::verify(
        &uops_core::Secret::new("wrong horse".to_owned()),
        &found.password_hash
    ));
}

#[tokio::test]
async fn email_lookup_ignores_case() {
    // Nobody remembers whether they signed up as Ratul@ or ratul@. More importantly,
    // two accounts differing only in case is an account-takeover vector, which the
    // unique index on lower(email) prevents.
    let store = store().await;
    let o = org(&store, "case").await;
    user(&store, o, "Ratul@Example.COM", "pw").await;

    assert!(
        store
            .user_credentials(o, "ratul@example.com")
            .await
            .unwrap()
            .is_some()
    );

    let duplicate = password::hash(&uops_core::Secret::new("pw".to_owned())).unwrap();
    let err = store
        .create_user(o, "RATUL@example.com", "Impostor", &duplicate)
        .await
        .unwrap_err();
    assert_eq!(err.status_code(), 400, "{err}");
}

#[tokio::test]
async fn a_session_authenticates_until_it_is_revoked() {
    let store = store().await;
    let o = org(&store, "session").await;
    let u = user(&store, o, "s@example.com", "pw").await;

    let (token, hash) = session::issue().unwrap();
    let id = store
        .create_session(u, &hash, Some("test-agent"))
        .await
        .unwrap();

    // The client presents the token; the server hashes it and looks it up.
    let live = store
        .touch_session(&session::hash_of(token.expose()))
        .await
        .unwrap()
        .expect("the session is live");
    assert_eq!(live.session_id, id);
    assert_eq!(live.user_id, u);
    assert_eq!(live.org_id, o);

    store.revoke_session(id).await.unwrap();
    assert!(
        store
            .touch_session(&session::hash_of(token.expose()))
            .await
            .unwrap()
            .is_none(),
        "logging out must end the session immediately, not at expiry"
    );
}

#[tokio::test]
async fn a_token_that_was_never_issued_authenticates_nothing() {
    let store = store().await;
    let (_, unissued) = session::issue().unwrap();
    assert!(store.touch_session(&unissued).await.unwrap().is_none());
}

#[tokio::test]
async fn an_idle_session_expires() {
    let store = store().await;
    let o = org(&store, "idle").await;
    let u = user(&store, o, "idle@example.com", "pw").await;

    let (token, hash) = session::issue().unwrap();
    store.create_session(u, &hash, None).await.unwrap();

    // As if it were last used thirteen hours ago: created recently, expiry passed.
    backdate_session(&store, u, Duration::hours(13), Duration::hours(-1)).await;

    assert!(
        store
            .touch_session(&session::hash_of(token.expose()))
            .await
            .unwrap()
            .is_none(),
        "an abandoned session on a shared screen must not still authenticate"
    );
}

#[tokio::test]
async fn continuous_use_cannot_push_a_session_past_the_absolute_cap() {
    // The reason there are two clocks. Sliding the idle window alone would keep a
    // stolen token alive forever, as long as it was being used.
    let store = store().await;
    let o = org(&store, "absolute").await;
    let u = user(&store, o, "abs@example.com", "pw").await;

    let (token, hash) = session::issue().unwrap();
    store.create_session(u, &hash, None).await.unwrap();

    // Created eight days ago, still within its idle window, used constantly since.
    backdate_session(&store, u, Duration::days(8), Duration::hours(6)).await;

    let after = store
        .touch_session(&session::hash_of(token.expose()))
        .await
        .unwrap();
    assert!(
        after.is_none_or(|s| s.expires_at < Utc::now()),
        "the absolute cap must win over a slid idle timeout"
    );
}

#[tokio::test]
async fn disabling_an_account_ends_its_sessions_now() {
    // Not within twelve hours. When an operator disables an account it is usually
    // because something is wrong, and "their session keeps working until it idles out"
    // is not an acceptable answer.
    let store = store().await;
    let o = org(&store, "disable").await;
    let u = user(&store, o, "gone@example.com", "pw").await;
    let t = tenant(&store, o, "disable").await;
    store.grant_role(u, t, Role::Admin, None).await.unwrap();

    let (token, hash) = session::issue().unwrap();
    store.create_session(u, &hash, None).await.unwrap();
    assert!(
        store
            .touch_session(&session::hash_of(token.expose()))
            .await
            .unwrap()
            .is_some()
    );

    store.disable_user(u).await.unwrap();

    assert!(
        store
            .touch_session(&session::hash_of(token.expose()))
            .await
            .unwrap()
            .is_none(),
        "a disabled account must stop authenticating immediately"
    );
    assert_eq!(
        store.role_for(u, t).await.unwrap(),
        None,
        "and must lose its roles, so a cached session cannot act either"
    );
    assert!(store.tenant_memberships(u).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_user_has_no_role_on_a_tenant_they_were_not_granted() {
    // The quietest and most important assertion in this file. `None` here is what makes
    // a request for another customer's data a 404 before any repository is called.
    let store = store().await;
    let o = org(&store, "roles").await;
    let u = user(&store, o, "r@example.com", "pw").await;
    let mine = tenant(&store, o, "roles-mine").await;
    let theirs = tenant(&store, o, "roles-theirs").await;

    store
        .grant_role(u, mine, Role::Operator, None)
        .await
        .unwrap();

    assert_eq!(store.role_for(u, mine).await.unwrap(), Some(Role::Operator));
    assert_eq!(
        store.role_for(u, theirs).await.unwrap(),
        None,
        "a tenant in the same organization is still not this user's tenant"
    );
    let memberships = store.tenant_memberships(u).await.unwrap();
    assert_eq!(memberships.len(), 1, "{memberships:?}");
    assert_eq!(memberships[0].tenant_id, mine);
    assert_eq!(memberships[0].role, Role::Operator);
    // The switcher shows this, not the uuid.
    assert_eq!(memberships[0].name, "auth-roles-mine");
}

#[tokio::test]
async fn one_account_can_hold_different_roles_on_different_tenants() {
    // SPEC §M1: "one MSP operator can be admin on one tenant and viewer on another".
    // The whole reason the role is per (user, tenant) rather than per user.
    let store = store().await;
    let o = org(&store, "msp").await;
    let engineer = user(&store, o, "eng@msp.example", "pw").await;
    let customer_a = tenant(&store, o, "msp-a").await;
    let customer_b = tenant(&store, o, "msp-b").await;

    store
        .grant_role(engineer, customer_a, Role::Admin, None)
        .await
        .unwrap();
    store
        .grant_role(engineer, customer_b, Role::Viewer, None)
        .await
        .unwrap();

    assert!(
        store
            .role_for(engineer, customer_a)
            .await
            .unwrap()
            .unwrap()
            .is_admin()
    );
    assert!(
        !store
            .role_for(engineer, customer_b)
            .await
            .unwrap()
            .unwrap()
            .is_admin()
    );

    // Re-granting changes the role rather than failing — a role change is an update,
    // not a delete-and-insert someone has to remember to do in the right order.
    store
        .grant_role(engineer, customer_b, Role::Operator, None)
        .await
        .unwrap();
    assert_eq!(
        store.role_for(engineer, customer_b).await.unwrap(),
        Some(Role::Operator)
    );

    store.revoke_role(engineer, customer_a).await.unwrap();
    assert_eq!(store.role_for(engineer, customer_a).await.unwrap(), None);
}

#[tokio::test]
async fn roles_are_ordered_so_a_check_is_a_comparison() {
    // Not a database test — the RBAC table from SPEC §M1, asserted once so the ordering
    // the `allows` comparison depends on cannot be quietly reordered.
    assert!(Role::Admin.allows(Role::Operator));
    assert!(Role::Admin.allows(Role::Viewer));
    assert!(Role::Operator.allows(Role::Viewer));
    assert!(!Role::Viewer.allows(Role::Operator));
    assert!(!Role::Operator.allows(Role::Admin));
    assert!(Role::Admin.is_admin() && !Role::Operator.is_admin());
}

#[tokio::test]
async fn signing_out_everywhere_ends_every_session() {
    let store = store().await;
    let o = org(&store, "everywhere").await;
    let u = user(&store, o, "multi@example.com", "pw").await;

    let mut tokens = Vec::new();
    for _ in 0..3 {
        let (token, hash) = session::issue().unwrap();
        store.create_session(u, &hash, None).await.unwrap();
        tokens.push(token);
    }

    assert_eq!(store.revoke_sessions_of(u).await.unwrap(), 3);
    for token in tokens {
        assert!(
            store
                .touch_session(&session::hash_of(token.expose()))
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn a_rehash_after_login_does_not_change_the_password() {
    // Raising the cost parameters later must be invisible to the user: the old hash
    // keeps verifying, and is rewritten at the next successful login.
    let store = store().await;
    let o = org(&store, "rehash").await;
    let u = user(&store, o, "rehash@example.com", "pw").await;

    let stronger = password::hash(&uops_core::Secret::new("pw".to_owned())).unwrap();
    store.update_password_hash(u, &stronger).await.unwrap();

    let found = store
        .user_credentials(o, "rehash@example.com")
        .await
        .unwrap()
        .unwrap();
    assert!(password::verify(
        &uops_core::Secret::new("pw".to_owned()),
        &found.password_hash
    ));
    assert!(!password::needs_rehash(&found.password_hash));
}
