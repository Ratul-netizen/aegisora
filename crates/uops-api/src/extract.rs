//! The authenticated scope extractor.
//!
//! **This is the only place in the product where `TenantScope::from_authenticated` is
//! called.** Every isolation guarantee M0 built rests on that: the type system prevents
//! a query without a scope, the schema prevents a row referencing another tenant, and
//! this function is what decides that a scope may exist at all.
//!
//! Four things have to be true before one is produced, and each of them is a separate
//! refusal rather than a combined check:
//!
//! 1. A session cookie is present.
//! 2. It names a live session — not expired, not revoked, and not belonging to a
//!    disabled account. All four are one statement in `touch_session`.
//! 3. The request names a tenant.
//! 4. The user holds a role on *that* tenant.
//!
//! # Why the tenant is a header and not a path segment
//!
//! `/api/v1/resources`, not `/api/v1/tenants/{id}/resources`. An MSP engineer's session
//! spans several customers, and the alternative to naming the tenant per request is
//! storing a "currently selected" one on the session — which means a request's meaning
//! depends on invisible state, an audit row can be ambiguous about which customer was
//! read, and a stolen cookie carries a selection with it. Naming it per request makes
//! every log line unambiguous.
//!
//! A custom header is also a CSRF defence in its own right: a cross-origin form cannot
//! set one without a preflight the browser will refuse. That is a second layer under the
//! double-submit token, not a replacement for it.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use uops_core::{ActorId, Role, SessionId, TenantId, TenantScope};
use uops_secrets::session;

use crate::audit::{Audit, Recorder};
use crate::cookie::{self, SESSION_COOKIE};
use crate::error::ApiError;
use crate::state::AppState;

/// Which tenant this request is about.
pub const TENANT_HEADER: &str = "x-uops-tenant";

/// An authenticated caller, scoped to one tenant, with a role on it.
///
/// Holding one of these is proof that all four checks above passed. Handlers take it by
/// value and never construct it.
#[derive(Clone, Debug)]
pub struct Caller {
    scope: TenantScope,
    role: Role,
    user_id: ActorId,
    session_id: SessionId,
    /// Where this request records what it read or changed. Handed out by the extractor
    /// rather than extracted separately, so a handler holding a scope always has
    /// somewhere to record — see [`crate::audit`].
    audit: Audit,
}

impl Caller {
    /// The scope to hand to a repository or the query compiler.
    #[must_use]
    pub const fn scope(&self) -> &TenantScope {
        &self.scope
    }

    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.scope.tenant_id()
    }

    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    #[must_use]
    pub const fn user_id(&self) -> ActorId {
        self.user_id
    }

    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Where to record what this request read or changed.
    #[must_use]
    pub const fn audit(&self) -> &Audit {
        &self.audit
    }

    /// For the audit and access logs: `user:<uuid>`.
    #[must_use]
    pub fn actor(&self) -> String {
        self.scope.actor().as_audit_str()
    }

    /// Require at least `needed`, or refuse.
    ///
    /// A 403 rather than a 404: the caller demonstrably holds *a* role on this tenant,
    /// so it is not a secret that it exists, and telling them which role they lack is
    /// what lets them ask for the right one.
    pub fn require(&self, needed: Role) -> Result<(), ApiError> {
        if self.role.allows(needed) {
            return Ok(());
        }
        Err(ApiError::Forbidden(match needed {
            Role::Admin => "this action requires the admin role",
            Role::Operator => "this action requires the operator role",
            Role::Viewer => "this action requires a role on this tenant",
        }))
    }
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = cookie::read(parts, SESSION_COOKIE).ok_or(ApiError::Unauthenticated)?;

        // Expiry, revocation, the absolute cap and the account being disabled are all
        // decided here, in one statement, which also slides the idle window.
        let live = state
            .store
            .touch_session(&session::hash_of(&token))
            .await?
            .ok_or(ApiError::Unauthenticated)?;

        let tenant = tenant_header(parts)?;

        // The check that turns another customer's data into a 404. `None` here is not
        // an error condition — it is the ordinary answer for every tenant this user
        // was not granted, including ones in their own organization.
        let role = state
            .store
            .role_for(live.user_id, tenant)
            .await?
            .ok_or(ApiError::NotFound)?;

        let scope = TenantScope::from_authenticated(tenant, live.user_id);

        // Registering here is what makes auditing unavoidable: this is the only way to
        // obtain a scope, so every handler that can read tenant data has already been
        // attributed by the time it runs. See crate::audit.
        let recorder = parts
            .extensions
            .get::<Recorder>()
            .cloned()
            .unwrap_or_default();
        recorder.set_context(tenant, scope.actor().as_audit_str());

        Ok(Self {
            scope,
            role,
            user_id: live.user_id,
            session_id: live.session_id,
            audit: Audit::new(recorder),
        })
    }
}

/// An authenticated caller who has not named a tenant.
///
/// For the handful of endpoints that precede choosing one: `GET /me`, the tenant list
/// the switcher is built from, and logout. Deliberately a different type, so no handler
/// can reach tenant data while holding it — there is no `TenantScope` in here to hand to
/// a repository.
#[derive(Clone, Debug)]
pub struct Authenticated {
    pub user_id: ActorId,
    pub session_id: SessionId,
    pub org_id: uops_core::OrgId,
}

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = cookie::read(parts, SESSION_COOKIE).ok_or(ApiError::Unauthenticated)?;
        let live = state
            .store
            .touch_session(&session::hash_of(&token))
            .await?
            .ok_or(ApiError::Unauthenticated)?;

        Ok(Self {
            user_id: live.user_id,
            session_id: live.session_id,
            org_id: live.org_id,
        })
    }
}

fn tenant_header(parts: &Parts) -> Result<TenantId, ApiError> {
    let raw = parts
        .headers
        .get(TENANT_HEADER)
        .ok_or_else(|| {
            ApiError::BadRequest(format!(
                "{TENANT_HEADER} is required: every request names the tenant it is about"
            ))
        })?
        .to_str()
        .map_err(|_| ApiError::BadRequest(format!("{TENANT_HEADER} is not valid text")))?;

    raw.parse::<TenantId>()
        .map_err(|_| ApiError::BadRequest(format!("{TENANT_HEADER} is not a UUID")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, Request, header};

    fn parts_with(cookie: Option<&str>, tenant: Option<&str>) -> Parts {
        let mut builder = Request::builder().uri("/");
        if let Some(c) = cookie {
            builder = builder.header(header::COOKIE, c);
        }
        if let Some(t) = tenant {
            builder = builder.header(TENANT_HEADER, HeaderValue::from_str(t).unwrap());
        }
        builder.body(()).unwrap().into_parts().0
    }

    #[test]
    fn a_missing_tenant_header_says_what_to_send() {
        // An unexplained 400 on every request would be an unpleasant first hour with
        // this API.
        let err = tenant_header(&parts_with(None, None)).unwrap_err();
        let message = err.to_string();
        assert!(message.contains(TENANT_HEADER), "{message}");
        assert!(message.contains("required"), "{message}");
    }

    #[test]
    fn a_malformed_tenant_header_is_a_400_not_a_404() {
        // "Not a UUID" is the caller's mistake and they can fix it. Reporting it as
        // "not found" would send them looking for a tenant that was never named.
        let err = tenant_header(&parts_with(None, Some("not-a-uuid"))).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "{err}");
    }

    #[test]
    fn a_well_formed_tenant_header_parses() {
        let id = TenantId::new();
        let parts = parts_with(None, Some(&id.to_string()));
        assert_eq!(tenant_header(&parts).unwrap(), id);
    }

    #[test]
    fn role_gates_are_inclusive_upwards() {
        let caller = |role| Caller {
            scope: TenantScope::system(TenantId::new()),
            role,
            user_id: ActorId::new(),
            session_id: SessionId::new(),
            audit: Audit::default(),
        };

        assert!(caller(Role::Admin).require(Role::Operator).is_ok());
        assert!(caller(Role::Operator).require(Role::Viewer).is_ok());
        assert!(caller(Role::Viewer).require(Role::Viewer).is_ok());

        assert!(caller(Role::Viewer).require(Role::Operator).is_err());
        assert!(caller(Role::Operator).require(Role::Admin).is_err());
    }

    #[test]
    fn a_refused_role_says_which_one_was_needed() {
        let viewer = Caller {
            scope: TenantScope::system(TenantId::new()),
            role: Role::Viewer,
            user_id: ActorId::new(),
            session_id: SessionId::new(),
            audit: Audit::default(),
        };
        let err = viewer.require(Role::Admin).unwrap_err();
        assert!(err.to_string().contains("admin"), "{err}");
    }
}
