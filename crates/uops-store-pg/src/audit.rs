//! The two logs — SPEC §M0.8.
//!
//! | log | records | asked by |
//! |---|---|---|
//! | `audit_log` | every mutating call, with before and after | "who changed this, and to what" |
//! | `access_log` | every **read** of a credential, a resource, or a telemetry query | "who *saw* this" |
//!
//! The second is the one that is unusual, and SPEC is explicit about why: defence and
//! law-enforcement buyers audit who saw what, not only who changed it. It is trivial to
//! add now and invasive to retrofit onto twenty handlers later.
//!
//! # Neither write can fail a request
//!
//! Both return `Result` so a caller *may* care, and the API deliberately does not: a
//! logging outage must not become an outage. The same reasoning as the credential access
//! log in `uops-secrets`, which is infallible by construction for the same reason.
//! What the API does instead is record the failure where an operator will see it.

use uops_core::{Result, TenantId};

use crate::error::map;
use crate::store::PgStore;

/// One mutating call.
#[derive(Clone, Debug)]
pub struct AuditEntry {
    pub tenant_id: TenantId,
    /// `user:<uuid>` | `collector` | `system`.
    pub actor: String,
    /// Dotted and stable: `resource.create`, `identity.merge`. Stable because an
    /// auditor's saved filter should keep working across releases.
    pub action: String,
    pub target: String,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
    pub ip: Option<std::net::IpAddr>,
}

/// One read.
#[derive(Clone, Debug)]
pub struct AccessEntry {
    pub tenant_id: TenantId,
    pub actor: String,
    /// `resource:<id>` | `resources` | `query` | `credential:<id>`.
    pub target: String,
    /// The *shape* of a query, never its parameters — those carry a customer's
    /// hostnames and addresses, and an auditor needs to know what was asked, not to be
    /// handed a second copy of the data.
    pub fingerprint: Option<String>,
    pub row_count: Option<i64>,
    pub ip: Option<std::net::IpAddr>,
}

impl PgStore {
    /// Record a mutation.
    pub async fn record_audit(&self, entry: &AuditEntry) -> Result<()> {
        sqlx::query!(
            r#"
            INSERT INTO audit_log (tenant_id, actor, action, target, before, after, ip)
            -- Bound as text and cast, rather than pulling in an IP-address crate for
            -- one column. PostgreSQL still validates it: a malformed address fails the
            -- cast rather than being stored as a string that looks like one.
            VALUES ($1, $2, $3, $4, $5, $6, $7::text::inet)
            "#,
            entry.tenant_id as TenantId,
            entry.actor,
            entry.action,
            entry.target,
            entry.before,
            entry.after,
            entry.ip.map(|ip| ip.to_string()),
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("audit_log", entry.target.clone(), e))?;
        Ok(())
    }

    /// Record a read.
    pub async fn record_access(&self, entry: &AccessEntry) -> Result<()> {
        sqlx::query!(
            r#"
            INSERT INTO access_log (tenant_id, actor, target, fingerprint, row_count, ip)
            VALUES ($1, $2, $3, $4, $5, $6::text::inet)
            "#,
            entry.tenant_id as TenantId,
            entry.actor,
            entry.target,
            entry.fingerprint,
            entry.row_count,
            entry.ip.map(|ip| ip.to_string()),
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("access_log", entry.target.clone(), e))?;
        Ok(())
    }

    /// Recent audit entries. Admin only, enforced above this.
    pub async fn audit_entries(&self, tenant: TenantId, limit: i64) -> Result<Vec<AuditEntry>> {
        let rows = sqlx::query!(
            r#"
            SELECT actor, action, target, before, after, host(ip) AS ip
              FROM audit_log
             WHERE tenant_id = $1
             ORDER BY at DESC
             LIMIT $2
            "#,
            tenant as TenantId,
            limit.clamp(1, 1_000),
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("audit_log", String::new(), e))?;

        Ok(rows
            .into_iter()
            .map(|r| AuditEntry {
                tenant_id: tenant,
                actor: r.actor,
                action: r.action,
                target: r.target,
                before: r.before,
                after: r.after,
                ip: r.ip.and_then(|s| s.parse().ok()),
            })
            .collect())
    }

    /// Recent access entries. Admin only, enforced above this.
    pub async fn access_entries(&self, tenant: TenantId, limit: i64) -> Result<Vec<AccessEntry>> {
        let rows = sqlx::query!(
            r#"
            SELECT actor, target, fingerprint, row_count, host(ip) AS ip
              FROM access_log
             WHERE tenant_id = $1
             ORDER BY at DESC
             LIMIT $2
            "#,
            tenant as TenantId,
            limit.clamp(1, 1_000),
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("access_log", String::new(), e))?;

        Ok(rows
            .into_iter()
            .map(|r| AccessEntry {
                tenant_id: tenant,
                actor: r.actor,
                target: r.target,
                fingerprint: r.fingerprint,
                row_count: r.row_count,
                ip: r.ip.and_then(|s| s.parse().ok()),
            })
            .collect())
    }
}
