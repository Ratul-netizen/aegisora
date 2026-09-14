//! The first run.
//!
//! An empty database cannot be administered, because administering it requires an
//! account and creating an account requires an administrator. Something has to break
//! that circle, and every way of breaking it is a hole of some size:
//!
//! | approach | the hole |
//! |---|---|
//! | shipped default credentials | every install shares them, and most never change them |
//! | a setup wizard on an open port | the window between first boot and first login is unauthenticated |
//! | an env var with the password | it is in the shell history, the compose file, and the process table |
//! | generate once, print once | the operator must read the log of the first boot |
//!
//! The last one is chosen. It has a real cost — if nobody reads that output, the only
//! way back in is to empty `app_user` and boot again — and in exchange there is no
//! moment at which this system is reachable by someone who has not been told a secret,
//! and no secret that is the same on two installations.
//!
//! # Why it is one transaction
//!
//! Bootstrap writes four rows across four tables: an organization, a tenant inside it,
//! a user in that organization, and an admin role binding the user to the tenant. A
//! partial bootstrap is worse than none — an org with no admin cannot be administered
//! and cannot be bootstrapped again, because [`bootstrap_first_run`] declines the
//! moment any user exists. It commits or it does not happen.
//!
//! # Why it takes an advisory lock
//!
//! Two replicas starting at once would both see an empty `app_user`, both generate a
//! password, and both insert — leaving two admins, of which the operator was shown
//! whichever log they happened to read. `SELECT EXISTS` does not take a lock that
//! prevents this; even `SERIALIZABLE` would give one of them an error to handle at the
//! least convenient moment in the process lifetime. A transaction-scoped advisory lock
//! makes the second replica wait, see the first one's user, and decline. It is released
//! on commit or rollback, including if the process is killed mid-bootstrap.
//!
//! # Why the store still never sees a plaintext password
//!
//! It takes a PHC hash, like [`crate::auth::PgStore::create_user`]. Generating the
//! password and printing it belong to the binary that has a terminal to print it on;
//! this module's job is to make the four rows appear together or not at all.

use uops_core::{ActorId, OrgId, Result, Role, TenantId};
use uops_secrets::PasswordHashString;

use crate::error::map;
use crate::store::PgStore;

/// Namespace for the bootstrap advisory lock.
///
/// Arbitrary, but must never collide with another advisory lock in this application.
/// Advisory locks share one 64-bit space per database, so the constant lives here and
/// not at a call site where a second one could be invented.
const BOOTSTRAP_LOCK: i64 = 0x7565_6f70_7331_0001;

/// What the first run is asked to create.
#[derive(Debug, Clone, Copy)]
pub struct FirstRunRequest<'a> {
    pub org_name: &'a str,
    pub tenant_name: &'a str,
    pub tenant_slug: &'a str,
    pub email: &'a str,
    pub display_name: &'a str,
    /// Already hashed. See the module docs.
    pub password_hash: &'a PasswordHashString,
}

/// What the first run created, for the caller to print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstRun {
    pub org: OrgId,
    pub tenant: TenantId,
    pub user: ActorId,
}

impl PgStore {
    /// True when at least one user exists.
    ///
    /// A cheap pre-check so a server that has been running for a year does not hash a
    /// password on every boot only to throw it away. It is *not* the decision — it
    /// races by construction, and [`Self::bootstrap_first_run`] makes the same check
    /// again under a lock. Callers may skip it entirely and lose only time.
    pub async fn any_user_exists(&self) -> Result<bool> {
        // tenant-exempt: this is a question about the installation, asked before any
        // tenant or session exists. It returns a boolean and never a row.
        let exists = sqlx::query_scalar!(r#"SELECT EXISTS (SELECT 1 FROM app_user)"#)
            .fetch_one(self.pool())
            .await
            .map_err(|e| map("user", String::new(), e))?;
        Ok(exists.unwrap_or(false))
    }

    /// Create the first organization, tenant, user and admin role — or decline.
    ///
    /// Returns `Ok(None)` when any user already exists, which is the normal result of
    /// every boot after the first. That is not an error and callers should not treat it
    /// as one.
    ///
    /// # Errors
    ///
    /// Storage failures, and a unique violation if the slug or email is somehow taken
    /// in a database that has no users — which would mean rows were inserted by hand.
    pub async fn bootstrap_first_run(&self, req: &FirstRunRequest<'_>) -> Result<Option<FirstRun>> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| map("bootstrap", String::new(), e))?;

        // tenant-exempt: this reads no rows. It is a lock on the installation, taken
        // before any tenant exists — indeed before the one this run is about to create.
        //
        // Held until this transaction ends, however it ends. A second replica blocks
        // here, then reads the committed user below and declines.
        sqlx::query!(r#"SELECT pg_advisory_xact_lock($1)"#, BOOTSTRAP_LOCK)
            .execute(&mut *tx)
            .await
            .map_err(|e| map("bootstrap", String::new(), e))?;

        // tenant-exempt: see any_user_exists. This is the authoritative check.
        let occupied = sqlx::query_scalar!(r#"SELECT EXISTS (SELECT 1 FROM app_user)"#)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| map("user", String::new(), e))?
            .unwrap_or(false);

        if occupied {
            // Dropping the transaction rolls it back and releases the lock. Nothing was
            // written, so there is nothing to undo — but saying so beats relying on the
            // reader to notice the early return.
            return Ok(None);
        }

        let org = OrgId::new();
        let tenant = TenantId::new();
        let user = ActorId::new();

        // tenant-exempt: organizations sit above the tenant isolation boundary.
        sqlx::query!(
            r#"INSERT INTO organization (id, name) VALUES ($1, $2)"#,
            org as OrgId,
            req.org_name,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map("organization", req.org_name.to_owned(), e))?;

        // tenant-exempt: this statement *creates* a tenant, so there is no tenant to
        // scope it to. Every statement that reads across tenants is scoped; this one
        // writes the row the scoping is later derived from.
        sqlx::query!(
            r#"INSERT INTO tenant (id, org_id, name, slug) VALUES ($1, $2, $3, $4)"#,
            tenant as TenantId,
            org as OrgId,
            req.tenant_name,
            req.tenant_slug,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map("tenant", req.tenant_slug.to_owned(), e))?;

        // tenant-exempt: users belong to an organization, not a tenant.
        sqlx::query!(
            r#"
            INSERT INTO app_user (id, org_id, email, display_name, password_hash)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            user as ActorId,
            org as OrgId,
            req.email,
            req.display_name,
            req.password_hash.as_str(),
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map("user", req.email.to_owned(), e))?;

        // granted_by is NULL: nobody granted this. It is the one role in the system
        // with no human behind it, and the audit log should say so rather than name a
        // user who did not exist when the decision was made.
        sqlx::query!(
            r#"
            INSERT INTO user_tenant_role (user_id, tenant_id, role, granted_by)
            VALUES ($1, $2, $3, NULL)
            "#,
            user as ActorId,
            tenant as TenantId,
            Role::Admin as Role,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map("role", user.to_string(), e))?;

        tx.commit()
            .await
            .map_err(|e| map("bootstrap", String::new(), e))?;

        Ok(Some(FirstRun { org, tenant, user }))
    }
}
