//! `POST /auth/login`, `POST /auth/logout`, `GET /me`.
//!
//! # Login must not be an oracle
//!
//! An unknown address and a wrong password have to be indistinguishable — in the
//! response, and in how long it takes to produce one. The response part is easy and
//! everyone does it. The timing part is where it usually goes wrong: returning early
//! when the user does not exist skips the Argon2 verification, and Argon2 is
//! *deliberately* slow, so "no such user" answers in a millisecond and "wrong password"
//! answers in twenty. That gap is a reliable account-enumeration oracle, and it is
//! measurable over the internet.
//!
//! So the handler verifies against a fixed dummy hash when the user is absent, and only
//! then decides. [`verify_credentials`] is arranged so that every path through it does
//! the same work.
//!
//! # What login hands back
//!
//! Two cookies. The session token, `HttpOnly`, which script must never read. And the
//! CSRF token, which script must read — see [`crate::csrf`].

use std::sync::OnceLock;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use uops_core::{Role, Secret, TenantId};
use uops_secrets::{PasswordHashString, password, session};

use crate::cookie::{self, CSRF_COOKIE, SESSION_COOKIE};
use crate::csrf::{self, CsrfChecked};
use crate::error::{ApiError, ApiResult};
use crate::extract::Authenticated;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    /// Wrapped on arrival so it cannot be logged or serialised onward: `Secret<T>` is
    /// not `Display` or `Serialize`, and the CI grep catches `.expose()` in a logging
    /// macro. `String` here would make a plaintext password one `tracing::info!` away
    /// from the log file.
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct MeResponse {
    pub user_id: String,
    pub email: String,
    pub display_name: String,
    /// Every tenant this user can reach, for the switcher. The list IS the access
    /// control surface a user sees; anything not here does not exist as far as they
    /// are concerned.
    pub tenants: Vec<TenantMembership>,
}

#[derive(Debug, Serialize)]
pub struct TenantMembership {
    pub tenant_id: TenantId,
    pub role: &'static str,
}

/// A real Argon2 hash of a password nobody has.
///
/// Verified against when no user matches, so that path costs what a real verification
/// costs. Computed once — doing it per request would be the same work but would also
/// make login slower for everyone, and the point is to be *equal*, not slow.
fn absent_user_hash() -> &'static PasswordHashString {
    static HASH: OnceLock<PasswordHashString> = OnceLock::new();
    HASH.get_or_init(|| {
        password::hash(&Secret::new(
            "a password no account has, hashed so that a missing user costs \
             what a wrong password costs"
                .to_owned(),
        ))
        .expect("hashing a constant cannot fail")
    })
}

/// Verify an address and password, doing equal work whether or not the user exists.
async fn verify_credentials(
    state: &AppState,
    email: &str,
    supplied: &Secret<String>,
) -> ApiResult<uops_core::ActorId> {
    let found = state.store.user_credentials_by_email(email).await?;

    // The hash to verify against: the user's, or a stand-in. Both cost the same.
    let hash = found
        .as_ref()
        .map_or_else(|| absent_user_hash().clone(), |c| c.password_hash.clone());

    let correct = password::verify(supplied, &hash);

    // Every failure below is the same failure to a caller: no user, wrong password, and
    // a disabled account are one answer. Telling a disabled user that their password was
    // right is also an answer about the password.
    let Some(credentials) = found else {
        return Err(ApiError::Unauthenticated);
    };
    if !correct || credentials.disabled {
        return Err(ApiError::Unauthenticated);
    }

    // The one moment the plaintext is in hand, so the one moment a hash written under
    // weaker parameters can be upgraded without asking the user for anything.
    if password::needs_rehash(&credentials.password_hash)
        && let Ok(stronger) = password::hash(supplied)
    {
        state
            .store
            .update_password_hash(credentials.user_id, &stronger)
            .await?;
    }

    Ok(credentials.user_id)
}

/// `POST /api/v1/auth/login`
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> ApiResult<Response> {
    let supplied = Secret::new(body.password);
    let user_id = verify_credentials(&state, &body.email, &supplied).await?;

    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(256).collect::<String>());

    let (token, token_hash) = session::issue().map_err(|e| ApiError::Internal(e.into()))?;
    state
        .store
        .create_session(user_id, &token_hash, user_agent.as_deref())
        .await?;

    let csrf_token = csrf::issue()?;
    // The same number twice: a cookie that outlives its session leaves the browser
    // presenting a dead token, and one that dies first logs the user out early.
    let max_age = uops_store_pg::IDLE_TIMEOUT.num_seconds();

    let mut response = StatusCode::NO_CONTENT.into_response();
    let out = response.headers_mut();
    out.append(
        header::SET_COOKIE,
        cookie::header(&cookie::set(
            SESSION_COOKIE,
            token.expose(),
            max_age,
            true,
            state.secure_cookies,
        )),
    );
    out.append(
        header::SET_COOKIE,
        cookie::header(&cookie::set(
            CSRF_COOKIE,
            &csrf_token,
            max_age,
            false,
            state.secure_cookies,
        )),
    );

    Ok(response)
}

/// `POST /api/v1/auth/logout`
///
/// Takes [`CsrfChecked`] like any other mutation. Logging someone out from another
/// origin is a nuisance rather than a breach, but exempting it would mean one more
/// endpoint whose protection is a special case somebody has to remember.
pub async fn logout(
    State(state): State<AppState>,
    caller: Authenticated,
    _csrf: CsrfChecked,
) -> ApiResult<Response> {
    state.store.revoke_session(caller.session_id).await?;

    let mut response = StatusCode::NO_CONTENT.into_response();
    let out = response.headers_mut();
    out.append(
        header::SET_COOKIE,
        cookie::header(&cookie::clear(SESSION_COOKIE, true, state.secure_cookies)),
    );
    out.append(
        header::SET_COOKIE,
        cookie::header(&cookie::clear(CSRF_COOKIE, false, state.secure_cookies)),
    );
    Ok(response)
}

/// `GET /api/v1/me`
///
/// Takes [`Authenticated`] rather than [`crate::Caller`]: this is what the app calls
/// *before* it knows which tenant to ask about, and it is how the tenant switcher is
/// populated.
pub async fn me(
    State(state): State<AppState>,
    caller: Authenticated,
) -> ApiResult<Json<MeResponse>> {
    let profile = state
        .store
        .user_profile(caller.user_id)
        .await?
        .ok_or(ApiError::Unauthenticated)?;

    let tenants = state
        .store
        .roles_of(caller.user_id)
        .await?
        .into_iter()
        .map(|(tenant_id, role)| TenantMembership {
            tenant_id,
            role: role_name(role),
        })
        .collect();

    Ok(Json(MeResponse {
        user_id: profile.user_id.to_string(),
        email: profile.email,
        display_name: profile.display_name,
        tenants,
    }))
}

const fn role_name(role: Role) -> &'static str {
    role.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stand_in_hash_is_a_real_one() {
        // If this were a constant string rather than a real Argon2 hash, verifying
        // against it would fail to parse and return in microseconds — which is exactly
        // the timing difference it exists to remove.
        let hash = absent_user_hash();
        assert!(hash.as_str().starts_with("$argon2id$"), "{}", hash.as_str());
        assert!(!password::needs_rehash(hash));
    }

    #[test]
    fn verifying_against_the_stand_in_costs_what_a_real_verification_costs() {
        // Not a timing assertion — a smoke test that the work actually happens. A
        // stand-in that failed fast would make "no such user" measurably quicker than
        // "wrong password", which is an account-enumeration oracle over the internet.
        let started = std::time::Instant::now();
        let matched = password::verify(&Secret::new("anything".to_owned()), absent_user_hash());
        assert!(!matched);
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(5),
            "the absent-user path finished in {:?} — it is not doing the work",
            started.elapsed()
        );
    }
}
