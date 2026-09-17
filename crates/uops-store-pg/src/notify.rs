//! Notification channels, and the limits that stop a storm — SPEC §M4.
//!
//! # Why the limit is a row rather than a counter in memory
//!
//! An in-process token bucket forgets everything on restart, and a rule that is trying to
//! send ten thousand notifications is *also* the thing most likely to make somebody
//! restart the process. It is also per-process, so it would come apart the first time an
//! installation ran two of anything. The limit lives in the same table as the record of
//! what was sent, so it survives a restart and is the same limit for every evaluator.
//!
//! # Why an attempt is recorded before it is delivered
//!
//! [`PgStore::reserve_notification`] writes the row and decides the outcome in one
//! statement, then the caller delivers and marks a failure if there was one. The order
//! matters: deciding first and recording afterwards means a process that dies mid-send
//! leaves no trace of having tried, and the next evaluation sends the same page again.
//! Recording first costs a row for a delivery that may fail, which is exactly the row an
//! operator wants when they ask why they were paged twice.
//!
//! A failed attempt keeps its capacity: the rate index counts only `sent`, so a webhook
//! that is refusing everything does not consume the rate — but it does consume the
//! reservation that was made for it, which is what stops a broken channel from retrying
//! at full speed.
//!
//! # What "atomic" means here, exactly
//!
//! One statement, so a single evaluator cannot exceed its own limit. Two evaluators
//! racing can each see the same count and each send, exceeding the limit by the number
//! racing — which is why running two is a thing an installation opts into
//! (`UOPS_ALERTS=off` on the replicas that should not) rather than the default.

use chrono::{DateTime, Utc};
use uops_core::{ActorId, Error as CoreError, Result, TenantScope};

use crate::error::map;
use crate::store::PgStore;

/// Where a notification goes.
#[derive(Clone, Debug)]
pub struct Channel {
    pub id: uuid::Uuid,
    pub tenant_id: uops_core::TenantId,
    pub name: String,
    /// `webhook` | `email`.
    pub kind: String,
    /// Per kind, and never secret — see migration 0015.
    pub config: serde_json::Value,
    pub enabled: bool,
    pub max_per_minute: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What a caller supplies to create or replace one.
#[derive(Clone, Debug)]
pub struct NewChannel {
    pub name: String,
    pub kind: String,
    pub config: serde_json::Value,
    pub enabled: bool,
    pub max_per_minute: i32,
}

/// What became of one attempt to notify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Cleared to deliver. The row is already written as `sent`; a failure marks it.
    Sent,
    /// The channel has already sent its allowance this minute.
    RateLimited,
    /// The tenant has already sent its allowance today.
    OverBudget,
    /// Delivery was attempted and the transport refused it.
    Failed,
}

impl Outcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::RateLimited => "rate_limited",
            Self::OverBudget => "over_budget",
            Self::Failed => "failed",
        }
    }

    fn from_str(text: &str) -> Result<Self> {
        match text {
            "sent" => Ok(Self::Sent),
            "rate_limited" => Ok(Self::RateLimited),
            "over_budget" => Ok(Self::OverBudget),
            "failed" => Ok(Self::Failed),
            other => Err(CoreError::Invalid(format!(
                "unknown notification outcome {other}"
            ))),
        }
    }
}

/// A reservation: the row that was written, and whether it cleared the limits.
#[derive(Clone, Copy, Debug)]
pub struct Reservation {
    pub id: uuid::Uuid,
    pub outcome: Outcome,
}

impl Reservation {
    /// Whether the caller should now deliver.
    #[must_use]
    pub const fn allowed(&self) -> bool {
        matches!(self.outcome, Outcome::Sent)
    }
}

/// What an attempt is about.
#[derive(Clone, Debug)]
pub struct Attempt {
    pub channel_id: uuid::Uuid,
    pub rule_id: uuid::Uuid,
    pub dedup_key: String,
    /// `firing` or `resolved`. The only two phases anybody is told about.
    pub phase: String,
}

/// One recorded attempt, for the "why was I paged" view.
#[derive(Clone, Debug)]
pub struct SentRecord {
    pub id: uuid::Uuid,
    pub channel_id: uuid::Uuid,
    pub rule_id: uuid::Uuid,
    pub dedup_key: String,
    pub phase: String,
    pub outcome: Outcome,
    pub detail: String,
    pub sent_at: DateTime<Utc>,
}

struct ChannelRow {
    id: uuid::Uuid,
    tenant_id: uops_core::TenantId,
    name: String,
    kind: String,
    config: serde_json::Value,
    enabled: bool,
    max_per_minute: i32,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<ChannelRow> for Channel {
    fn from(r: ChannelRow) -> Self {
        Self {
            id: r.id,
            tenant_id: r.tenant_id,
            name: r.name,
            kind: r.kind,
            config: r.config,
            enabled: r.enabled,
            max_per_minute: r.max_per_minute,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

impl PgStore {
    /// Create a channel.
    pub async fn create_channel(
        &self,
        scope: &TenantScope,
        by: Option<ActorId>,
        new: &NewChannel,
    ) -> Result<Channel> {
        // tenant-exempt: the tenant is the first bound parameter, from the scope.
        let row = sqlx::query_as!(
            ChannelRow,
            r#"
            INSERT INTO notification_channel
                (tenant_id, name, kind, config, enabled, max_per_minute, created_by)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING
                id,
                tenant_id AS "tenant_id: uops_core::TenantId",
                name, kind, config, enabled, max_per_minute, created_at, updated_at
            "#,
            scope.tenant_id() as uops_core::TenantId,
            new.name.trim(),
            new.kind,
            new.config,
            new.enabled,
            new.max_per_minute,
            by as Option<ActorId>,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("notification_channel", new.name.clone(), e))?;

        Ok(row.into())
    }

    /// A tenant's channels, oldest first — the order they were set up in, which is the
    /// order somebody who set them up thinks of them in.
    pub async fn channels(&self, scope: &TenantScope) -> Result<Vec<Channel>> {
        // tenant-exempt: the tenant is the only bound parameter, from the scope.
        let rows = sqlx::query_as!(
            ChannelRow,
            r#"
            SELECT
                id,
                tenant_id AS "tenant_id: uops_core::TenantId",
                name, kind, config, enabled, max_per_minute, created_at, updated_at
              FROM notification_channel
             WHERE tenant_id = $1
             ORDER BY created_at
            "#,
            scope.tenant_id() as uops_core::TenantId,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("notification_channel", "list".to_owned(), e))?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// One channel.
    ///
    /// # Errors
    ///
    /// `NotFound` for another tenant's channel, which is the same answer as for one that
    /// does not exist.
    pub async fn channel(&self, scope: &TenantScope, id: uuid::Uuid) -> Result<Channel> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query_as!(
            ChannelRow,
            r#"
            SELECT
                id,
                tenant_id AS "tenant_id: uops_core::TenantId",
                name, kind, config, enabled, max_per_minute, created_at, updated_at
              FROM notification_channel
             WHERE tenant_id = $1 AND id = $2
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("notification_channel", id.to_string(), e))?
        .ok_or(CoreError::NotFound {
            kind: "notification_channel",
            id: id.to_string(),
        })?;

        Ok(row.into())
    }

    /// Replace a channel.
    pub async fn update_channel(
        &self,
        scope: &TenantScope,
        id: uuid::Uuid,
        new: &NewChannel,
    ) -> Result<Channel> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query_as!(
            ChannelRow,
            r#"
            UPDATE notification_channel
               SET name = $3, kind = $4, config = $5, enabled = $6, max_per_minute = $7
             WHERE tenant_id = $1 AND id = $2
            RETURNING
                id,
                tenant_id AS "tenant_id: uops_core::TenantId",
                name, kind, config, enabled, max_per_minute, created_at, updated_at
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id,
            new.name.trim(),
            new.kind,
            new.config,
            new.enabled,
            new.max_per_minute,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("notification_channel", new.name.clone(), e))?
        .ok_or(CoreError::NotFound {
            kind: "notification_channel",
            id: id.to_string(),
        })?;

        Ok(row.into())
    }

    /// Delete a channel, and with it the record of what it sent.
    pub async fn delete_channel(&self, scope: &TenantScope, id: uuid::Uuid) -> Result<()> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let done = sqlx::query!(
            "DELETE FROM notification_channel WHERE tenant_id = $1 AND id = $2",
            scope.tenant_id() as uops_core::TenantId,
            id,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("notification_channel", id.to_string(), e))?;

        if done.rows_affected() == 0 {
            return Err(CoreError::NotFound {
                kind: "notification_channel",
                id: id.to_string(),
            });
        }
        Ok(())
    }

    /// Claim the right to send one notification, recording the attempt either way.
    ///
    /// One statement: the budget, then the rate, then the row. Budget first because it is
    /// the bigger fact — an operator whose tenant has spent its day's allowance needs to
    /// be told that rather than that one channel is briefly busy.
    ///
    /// # Errors
    ///
    /// When the channel does not belong to this tenant, or the write fails.
    pub async fn reserve_notification(
        &self,
        scope: &TenantScope,
        attempt: &Attempt,
    ) -> Result<Reservation> {
        // tenant-exempt: the tenant is the first bound parameter, from the scope, and it
        // appears in every sub-select below for the same reason.
        let row = sqlx::query!(
            r#"
            INSERT INTO notification_sent
                (tenant_id, channel_id, rule_id, dedup_key, phase, outcome)
            SELECT $1, $2, $3, $4, $5,
                   CASE
                     WHEN (SELECT count(*)
                             FROM notification_sent
                            WHERE tenant_id = $1
                              AND outcome = 'sent'
                              AND sent_at >= date_trunc('day', now()))
                          >= (SELECT notification_budget_per_day FROM tenant WHERE id = $1)
                       THEN 'over_budget'
                     WHEN (SELECT count(*)
                             FROM notification_sent
                            WHERE channel_id = $2
                              AND outcome = 'sent'
                              AND sent_at > now() - interval '1 minute')
                          >= (SELECT max_per_minute
                                FROM notification_channel
                               WHERE id = $2 AND tenant_id = $1)
                       THEN 'rate_limited'
                     ELSE 'sent'
                   END
             WHERE EXISTS (
                 SELECT 1 FROM notification_channel
                  WHERE id = $2 AND tenant_id = $1 AND enabled
             )
            RETURNING id, outcome
            "#,
            scope.tenant_id() as uops_core::TenantId,
            attempt.channel_id,
            attempt.rule_id,
            attempt.dedup_key,
            attempt.phase,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("notification_sent", attempt.dedup_key.clone(), e))?
        // No row means the `WHERE EXISTS` found no enabled channel of this tenant with
        // that id. A disabled channel and another tenant's channel answer identically,
        // which is the same 404-never-403 reasoning as everywhere else.
        .ok_or(CoreError::NotFound {
            kind: "notification_channel",
            id: attempt.channel_id.to_string(),
        })?;

        Ok(Reservation {
            id: row.id,
            outcome: Outcome::from_str(&row.outcome)?,
        })
    }

    /// Mark a reserved notification as having failed to deliver.
    ///
    /// `detail` is the transport's own words. An operator debugging a webhook needs the
    /// status code and the body, not "delivery failed".
    pub async fn notification_failed(
        &self,
        scope: &TenantScope,
        id: uuid::Uuid,
        detail: &str,
    ) -> Result<()> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        sqlx::query!(
            r#"
            UPDATE notification_sent
               SET outcome = 'failed', detail = $3
             WHERE tenant_id = $1 AND id = $2
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id,
            // Bounded, because a transport that returns a megabyte of HTML on error would
            // otherwise put a megabyte of HTML in a row somebody reads in a terminal.
            detail.chars().take(500).collect::<String>(),
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("notification_sent", id.to_string(), e))?;

        Ok(())
    }

    /// What was attempted for this tenant, newest first.
    ///
    /// The answer to "why was nobody paged", which is why it includes the refusals.
    pub async fn notifications(&self, scope: &TenantScope, limit: i64) -> Result<Vec<SentRecord>> {
        // tenant-exempt: the tenant is the first bound parameter, from the scope.
        let rows = sqlx::query!(
            r#"
            SELECT id, channel_id, rule_id, dedup_key, phase, outcome, detail, sent_at
              FROM notification_sent
             WHERE tenant_id = $1
             ORDER BY sent_at DESC
             LIMIT $2
            "#,
            scope.tenant_id() as uops_core::TenantId,
            limit.clamp(1, 500),
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("notification_sent", "list".to_owned(), e))?;

        rows.into_iter()
            .map(|r| {
                Ok(SentRecord {
                    id: r.id,
                    channel_id: r.channel_id,
                    rule_id: r.rule_id,
                    dedup_key: r.dedup_key,
                    phase: r.phase,
                    outcome: Outcome::from_str(&r.outcome)?,
                    detail: r.detail,
                    sent_at: r.sent_at,
                })
            })
            .collect()
    }
}
