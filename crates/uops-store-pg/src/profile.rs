//! Monitoring profiles in `PostgreSQL`.
//!
//! The built-ins are embedded in the binary — see `uops_profile::builtin` — so this
//! table is not where they *live*. It is where they become visible: an operator asking
//! "what is polling this device and why" gets an answer from the database rather than
//! from a release note, and a customer who needs a sixth profile writes a row rather
//! than a pull request.
//!
//! # Seeding is an upsert keyed on version, and never a delete
//!
//! [`PgStore::seed_builtin_profiles`] runs at startup. It inserts what is missing and
//! updates a built-in whose definition has changed within the same version — which is
//! what a bug fix in a shipped profile looks like.
//!
//! What it must not do is remove rows it did not put there. A tenant's own profile with
//! the same key is a deliberate override, and "reconcile the table to match the binary"
//! would delete it on the next restart. So this only ever touches rows with a null
//! `tenant_id`.
//!
//! # A tenant's profile set is theirs plus the built-ins, theirs winning
//!
//! [`PgStore::profiles_for`] returns one profile per key: the tenant's if they have one,
//! the built-in otherwise, and the highest version of whichever it is. That is how a
//! customer fixes a wrong OID on a Thursday without waiting for a release — and it is
//! resolved in SQL rather than by returning both and making every caller re-implement
//! the precedence.

use uops_core::{Result, TenantId};
use uops_profile::Profile;

use crate::error::map;
use crate::store::PgStore;

impl PgStore {
    /// Insert or refresh every profile that ships with the product.
    ///
    /// Returns how many rows it wrote. Idempotent: a second call on an unchanged binary
    /// writes nothing, which is what makes it safe at every startup.
    ///
    /// # Errors
    ///
    /// Storage failures, or a built-in that does not serialise — which cannot happen,
    /// because `builtin::all()` has already parsed it.
    pub async fn seed_builtin_profiles(&self, profiles: &[Profile]) -> Result<u64> {
        let mut written = 0;

        for profile in profiles {
            let definition = serde_json::to_value(profile).map_err(|e| {
                uops_core::Error::Invalid(format!("profile {} does not serialise: {e}", profile.id))
            })?;

            // tenant-exempt: a built-in has no tenant by definition — that is what makes
            // it visible to every one of them. The partial unique index in migration
            // 0007 is what keeps two of them from claiming the same key.
            let result = sqlx::query!(
                r#"
                INSERT INTO monitoring_profile (id, tenant_id, profile_key, version, definition)
                VALUES (gen_random_uuid(), NULL, $1, $2, $3)
                ON CONFLICT (profile_key, version) WHERE tenant_id IS NULL
                DO UPDATE SET definition = EXCLUDED.definition,
                              updated_at = now()
                          WHERE monitoring_profile.definition IS DISTINCT FROM EXCLUDED.definition
                "#,
                profile.id,
                i32::try_from(profile.version).unwrap_or(i32::MAX),
                definition,
            )
            .execute(self.pool())
            .await
            .map_err(|e| map("monitoring_profile", profile.id.clone(), e))?;

            written += result.rows_affected();
        }

        Ok(written)
    }

    /// Every profile this tenant can use: theirs, plus the built-ins they have not
    /// overridden.
    ///
    /// One row per key, highest version, tenant's own winning over a built-in with the
    /// same key. Disabled rows are excluded — a disabled built-in is how an operator
    /// turns one off without deleting something they cannot put back.
    ///
    /// # Errors
    ///
    /// Storage failures, or a stored definition that no longer parses — which means a
    /// row was edited by hand into something this version cannot read, and is worth
    /// failing loudly rather than skipping.
    pub async fn profiles_for(&self, tenant: TenantId) -> Result<Vec<Profile>> {
        // DISTINCT ON with an ORDER BY that puts the winner first: tenant's own before
        // built-in, then highest version. One statement, so no caller has to know the
        // precedence rule and none of them can disagree about it.
        //
        // tenant-exempt: the built-in half of this union has no tenant_id by design, and
        // the tenant half is filtered to the one asked for. A query that filtered every
        // row by tenant_id would return no built-ins at all, which is the opposite of
        // what this is for.
        let rows = sqlx::query!(
            r#"
            SELECT DISTINCT ON (profile_key)
                   profile_key,
                   definition AS "definition!"
              FROM monitoring_profile
             WHERE enabled
               AND (tenant_id = $1 OR tenant_id IS NULL)
             ORDER BY profile_key,
                      (tenant_id IS NOT NULL) DESC,
                      version DESC
            "#,
            tenant as TenantId,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("monitoring_profile", String::new(), e))?;

        rows.into_iter()
            .map(|r| {
                serde_json::from_value::<Profile>(r.definition).map_err(|e| {
                    uops_core::Error::Invalid(format!(
                        "stored profile {} cannot be read by this version: {e}",
                        r.profile_key
                    ))
                })
            })
            .collect()
    }

    /// Store a tenant's own profile, shadowing a built-in with the same key.
    ///
    /// # Errors
    ///
    /// Storage failures, or a profile that does not serialise.
    pub async fn put_profile(&self, tenant: TenantId, profile: &Profile) -> Result<()> {
        let definition = serde_json::to_value(profile).map_err(|e| {
            uops_core::Error::Invalid(format!("profile {} does not serialise: {e}", profile.id))
        })?;

        sqlx::query!(
            r#"
            INSERT INTO monitoring_profile (id, tenant_id, profile_key, version, definition)
            VALUES (gen_random_uuid(), $1, $2, $3, $4)
            ON CONFLICT (tenant_id, profile_key, version)
            DO UPDATE SET definition = EXCLUDED.definition, updated_at = now()
            "#,
            tenant as TenantId,
            profile.id,
            i32::try_from(profile.version).unwrap_or(i32::MAX),
            definition,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("monitoring_profile", profile.id.clone(), e))?;
        Ok(())
    }
}
