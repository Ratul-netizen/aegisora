//! Users, roles and sessions.
//!
//! This is the layer that makes `TenantScope::from_authenticated` reachable honestly.
//! Everything M0 built rests on that one function only ever being called after a real
//! session has been verified against a real role, so the checks here are the foundation
//! of every isolation guarantee above them.
//!
//! Two deliberate shapes:
//!
//! * **The store never sees a password.** It stores and returns a PHC hash; verifying is
//!   `uops_secrets::password`, and deciding is the API. A repository that took a
//!   plaintext password would be a repository that could log one.
//! * **Session lookup is one statement that also touches the clock.** Validate, slide
//!   the idle timeout, and enforce the absolute cap together — on the hot path of every
//!   authenticated request, three round trips would be three times the cost of the thing
//!   being protected.

use chrono::{DateTime, Duration, Utc};
use uops_core::{ActorId, OrgId, Result, Role, SessionId, TenantId};
use uops_secrets::{PasswordHashString, SessionTokenHash};

use crate::error::map;
use crate::store::PgStore;

/// Idle timeout — SPEC §M0.8. Ends a session abandoned on a shared NOC screen.
pub const IDLE_TIMEOUT: Duration = Duration::hours(12);
/// Absolute cap. Bounds a stolen token that is being kept alive by use, which the idle
/// timeout alone never would.
pub const ABSOLUTE_TIMEOUT: Duration = Duration::days(7);

/// What a login needs to check, and nothing more.
#[derive(Clone, Debug)]
pub struct UserCredentials {
    pub user_id: ActorId,
    pub password_hash: PasswordHashString,
    /// Disabled accounts are kept so the audit log can still name them.
    pub disabled: bool,
}

/// Who a user is. No password material of any kind.
#[derive(Clone, Debug)]
pub struct UserProfile {
    pub user_id: ActorId,
    pub org_id: OrgId,
    pub email: String,
    pub display_name: String,
    pub disabled: bool,
}

/// A session that was live at the moment it was looked up.
#[derive(Clone, Debug)]
pub struct AuthenticatedSession {
    pub session_id: SessionId,
    pub user_id: ActorId,
    pub org_id: OrgId,
    pub expires_at: DateTime<Utc>,
}

impl PgStore {
    /// Create a user. The caller has already hashed the password.
    pub async fn create_user(
        &self,
        org: OrgId,
        email: &str,
        display_name: &str,
        password_hash: &PasswordHashString,
    ) -> Result<ActorId> {
        let id = ActorId::new();
        // tenant-exempt: users belong to an organization, which sits above the tenant
        // isolation boundary — the same reason `organization` has no tenant_id.
        sqlx::query!(
            r#"
            INSERT INTO app_user (id, org_id, email, display_name, password_hash)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            id as ActorId,
            org as OrgId,
            email,
            display_name,
            password_hash.as_str(),
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("user", email.to_owned(), e))?;
        Ok(id)
    }

    /// Look up what a login attempt needs to verify.
    ///
    /// Matched case-insensitively: nobody remembers whether they signed up as `Ratul@`
    /// or `ratul@`, and two accounts differing only in case is an account-takeover
    /// vector rather than a convenience.
    pub async fn user_credentials(
        &self,
        org: OrgId,
        email: &str,
    ) -> Result<Option<UserCredentials>> {
        // tenant-exempt: authentication happens before any tenant is known — which
        // tenants this user may reach is the *next* question, answered by roles_of.
        let row = sqlx::query!(
            r#"
            SELECT id AS "id: ActorId", password_hash, disabled_at
              FROM app_user
             WHERE org_id = $1 AND lower(email) = lower($2)
            "#,
            org as OrgId,
            email,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("user", email.to_owned(), e))?;

        Ok(row.map(|r| UserCredentials {
            user_id: r.id,
            password_hash: PasswordHashString::from_stored(r.password_hash),
            disabled: r.disabled_at.is_some(),
        }))
    }

    /// Find a user by email address alone, across the whole deployment.
    ///
    /// Login has no organization to work with: the user types an address into a form.
    /// A deployment with one organization — the common case, and every single-company
    /// install — resolves unambiguously.
    ///
    /// Two accounts sharing an address across organizations returns `None`, which the
    /// caller reports as a failed login. Silently picking one would let whoever
    /// registered second intercept the first account's logins. Distinguishing the case
    /// in the response would confirm an address exists. When a deployment genuinely
    /// needs both, login gains an organization selector from the subdomain — M2, and a
    /// deliberate decision rather than a default that happened to be convenient.
    pub async fn user_credentials_by_email(&self, email: &str) -> Result<Option<UserCredentials>> {
        // tenant-exempt: authentication precedes knowing a tenant.
        let rows = sqlx::query!(
            r#"
            SELECT id AS "id: ActorId", password_hash, disabled_at
              FROM app_user
             WHERE lower(email) = lower($1)
             LIMIT 2
            "#,
            email,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("user", email.to_owned(), e))?;

        if rows.len() != 1 {
            return Ok(None);
        }
        let r = &rows[0];
        Ok(Some(UserCredentials {
            user_id: r.id,
            password_hash: PasswordHashString::from_stored(r.password_hash.clone()),
            disabled: r.disabled_at.is_some(),
        }))
    }

    /// Who a user is, for `GET /me`.
    pub async fn user_profile(&self, user: ActorId) -> Result<Option<UserProfile>> {
        // tenant-exempt: a user is an organization-level record.
        let row = sqlx::query!(
            r#"
            SELECT id AS "id: ActorId", org_id AS "org_id: OrgId", email, display_name,
                   disabled_at
              FROM app_user
             WHERE id = $1
            "#,
            user as ActorId,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("user", user.to_string(), e))?;

        Ok(row.map(|r| UserProfile {
            user_id: r.id,
            org_id: r.org_id,
            email: r.email,
            display_name: r.display_name,
            disabled: r.disabled_at.is_some(),
        }))
    }

    /// Rewrite a hash after a successful login under weaker parameters.
    pub async fn update_password_hash(
        &self,
        user: ActorId,
        password_hash: &PasswordHashString,
    ) -> Result<()> {
        // tenant-exempt: a user is an organization-level record.
        sqlx::query!(
            r#"UPDATE app_user SET password_hash = $2 WHERE id = $1"#,
            user as ActorId,
            password_hash.as_str(),
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("user", user.to_string(), e))?;
        Ok(())
    }

    /// Grant a role on one tenant. Re-granting changes the role rather than failing.
    pub async fn grant_role(
        &self,
        user: ActorId,
        tenant: TenantId,
        role: Role,
        granted_by: Option<ActorId>,
    ) -> Result<()> {
        sqlx::query!(
            r#"
            INSERT INTO user_tenant_role (user_id, tenant_id, role, granted_by)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (user_id, tenant_id)
            DO UPDATE SET role = EXCLUDED.role,
                          granted_at = now(),
                          granted_by = EXCLUDED.granted_by
            "#,
            user as ActorId,
            tenant as TenantId,
            role as Role,
            granted_by as Option<ActorId>,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("role", user.to_string(), e))?;
        Ok(())
    }

    pub async fn revoke_role(&self, user: ActorId, tenant: TenantId) -> Result<()> {
        sqlx::query!(
            r#"DELETE FROM user_tenant_role WHERE user_id = $1 AND tenant_id = $2"#,
            user as ActorId,
            tenant as TenantId,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("role", user.to_string(), e))?;
        Ok(())
    }

    /// The user's role on one tenant, or `None` if they have none.
    ///
    /// `None` is the answer that matters: it is what turns a request for another
    /// customer's data into a 404 before any repository is called.
    pub async fn role_for(&self, user: ActorId, tenant: TenantId) -> Result<Option<Role>> {
        let row = sqlx::query_scalar!(
            r#"
            SELECT r.role AS "role: Role"
              FROM user_tenant_role r
              JOIN app_user u ON u.id = r.user_id
             WHERE r.user_id = $1
               AND r.tenant_id = $2
               -- A disabled account keeps its rows so the audit log can name it, and
               -- loses its access immediately. Checked here rather than only at login,
               -- because a live session must stop working the moment the account does.
               AND u.disabled_at IS NULL
            "#,
            user as ActorId,
            tenant as TenantId,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("role", user.to_string(), e))?;
        Ok(row)
    }

    /// Every tenant this user can reach, for the tenant switcher.
    pub async fn roles_of(&self, user: ActorId) -> Result<Vec<(TenantId, Role)>> {
        let rows = sqlx::query!(
            r#"
            SELECT r.tenant_id AS "tenant_id: TenantId", r.role AS "role: Role"
              FROM user_tenant_role r
              JOIN app_user u ON u.id = r.user_id
             WHERE r.user_id = $1 AND u.disabled_at IS NULL
             ORDER BY r.tenant_id
            "#,
            user as ActorId,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("role", user.to_string(), e))?;

        Ok(rows.into_iter().map(|r| (r.tenant_id, r.role)).collect())
    }

    /// Open a session for a user who has just authenticated.
    pub async fn create_session(
        &self,
        user: ActorId,
        token_hash: &SessionTokenHash,
        user_agent: Option<&str>,
    ) -> Result<SessionId> {
        let id = SessionId::new();
        let expires_at = Utc::now() + IDLE_TIMEOUT;

        // tenant-exempt: a session belongs to a user, not to a tenant — the same user
        // moves between tenants within one session.
        sqlx::query!(
            r#"
            INSERT INTO session (id, user_id, token_hash, expires_at, user_agent)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            id as SessionId,
            user as ActorId,
            token_hash.as_bytes().as_slice(),
            expires_at,
            user_agent,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("session", id.to_string(), e))?;
        Ok(id)
    }

    /// Validate a presented token, and slide its idle timeout in the same statement.
    ///
    /// One round trip, because this runs on every authenticated request. The `LEAST`
    /// is the absolute cap: sliding the idle window can never push expiry past seven
    /// days from when the session was created, however continuously it is used.
    ///
    /// A session whose user has since been disabled does not authenticate — the join
    /// is why, and it is why an account can be shut off without hunting down its
    /// sessions first.
    pub async fn touch_session(
        &self,
        token_hash: &SessionTokenHash,
    ) -> Result<Option<AuthenticatedSession>> {
        let idle = sqlx::postgres::types::PgInterval::try_from(IDLE_TIMEOUT)
            .map_err(|e| uops_core::Error::Storage(e.to_string()))?;
        let absolute = sqlx::postgres::types::PgInterval::try_from(ABSOLUTE_TIMEOUT)
            .map_err(|e| uops_core::Error::Storage(e.to_string()))?;

        // tenant-exempt: this is authentication, which precedes knowing a tenant.
        let row = sqlx::query!(
            r#"
            UPDATE session s
               SET last_seen_at = now(),
                   expires_at   = LEAST(now() + $2::interval, s.created_at + $3::interval)
              FROM app_user u
             WHERE s.token_hash = $1
               AND s.user_id = u.id
               AND s.revoked_at IS NULL
               AND s.expires_at > now()
               AND u.disabled_at IS NULL
            RETURNING s.id      AS "session_id: SessionId",
                      s.user_id AS "user_id: ActorId",
                      u.org_id  AS "org_id: OrgId",
                      s.expires_at
            "#,
            token_hash.as_bytes().as_slice(),
            idle,
            absolute,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("session", String::new(), e))?;

        Ok(row.map(|r| AuthenticatedSession {
            session_id: r.session_id,
            user_id: r.user_id,
            org_id: r.org_id,
            expires_at: r.expires_at,
        }))
    }

    /// End one session. Logging out.
    pub async fn revoke_session(&self, session: SessionId) -> Result<()> {
        // tenant-exempt: sessions are not tenant-scoped.
        sqlx::query!(
            r#"UPDATE session SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL"#,
            session as SessionId,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("session", session.to_string(), e))?;
        Ok(())
    }

    /// End every session for a user. "Sign out everywhere", and what disabling an
    /// account should do immediately rather than within twelve hours.
    pub async fn revoke_sessions_of(&self, user: ActorId) -> Result<u64> {
        // tenant-exempt: sessions are not tenant-scoped.
        let affected = sqlx::query!(
            r#"UPDATE session SET revoked_at = now() WHERE user_id = $1 AND revoked_at IS NULL"#,
            user as ActorId,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("session", user.to_string(), e))?
        .rows_affected();
        Ok(affected)
    }

    /// Delete sessions that expired more than a day ago.
    ///
    /// Expiry is enforced by the query, not by this: a session is dead the moment
    /// `expires_at` passes, whether or not anything has swept it. This exists so the
    /// table does not grow forever.
    pub async fn purge_expired_sessions(&self) -> Result<u64> {
        // tenant-exempt: sessions are not tenant-scoped.
        let removed =
            sqlx::query!(r#"DELETE FROM session WHERE expires_at < now() - interval '1 day'"#)
                .execute(self.pool())
                .await
                .map_err(|e| map("session", String::new(), e))?
                .rows_affected();
        Ok(removed)
    }

    /// Disable an account: it stops authenticating, and its sessions end now.
    pub async fn disable_user(&self, user: ActorId) -> Result<()> {
        // tenant-exempt: a user is an organization-level record.
        sqlx::query!(
            r#"UPDATE app_user SET disabled_at = now() WHERE id = $1 AND disabled_at IS NULL"#,
            user as ActorId,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("user", user.to_string(), e))?;

        self.revoke_sessions_of(user).await?;
        Ok(())
    }
}
