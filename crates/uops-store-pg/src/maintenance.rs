//! Maintenance windows, stored.
//!
//! The occurrence arithmetic is [`uops_core::maintenance`] — pure, so the awkward cases
//! (a spring-forward hour that does not exist, two ambiguous local times in an autumn
//! fall-back) are testable without a database. This is the row, the CRUD, and the one
//! query the alert engine will ask.
//!
//! # Why "which windows exist" and "which are open" are separate
//!
//! [`live_windows`](PgStore::live_windows) returns every window in the tenant that has not
//! expired; the caller asks each one [`Schedule::is_open_at`]. Pushing the occurrence
//! rules into SQL would mean writing them twice, in two languages, with the DST edge cases
//! in both — and the two copies would disagree eventually, silently, in whichever
//! direction nobody tested.
//!
//! It is also cheap. Windows are written by hand and a tenant has them in the tens, so
//! "every live window" is a small result set that an alert engine can hold and refresh on
//! an interval rather than query per evaluation.
//!
//! # What this deliberately does not do
//!
//! Decide anything. The alert engine in M4 owns *whether to suppress*; this stops at
//! "here are the windows and what each one covers". Building the suppression before the
//! thing being suppressed exists would be guessing at an interface.

use chrono::{DateTime, Utc};
use uops_core::{
    ActorId, Error as CoreError, Recurrence, ResourceGroupId, ResourceId, Result, Schedule, SiteId,
    Suppression, Target, TenantScope,
};

use crate::error::map;
use crate::store::PgStore;

/// One stored window.
#[derive(Clone, Debug)]
pub struct MaintenanceWindow {
    pub id: uuid::Uuid,
    pub tenant_id: uops_core::TenantId,
    pub reason: String,
    pub target: Target,
    pub schedule: Schedule,
    pub suppression: Suppression,
    pub created_by: Option<ActorId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl MaintenanceWindow {
    /// Is this window open right now?
    #[must_use]
    pub fn is_open_at(&self, at: DateTime<Utc>) -> bool {
        self.schedule.is_open_at(at)
    }
}

/// What a caller supplies to schedule one.
#[derive(Clone, Debug)]
pub struct NewWindow {
    pub reason: String,
    pub target: Target,
    pub schedule: Schedule,
    pub suppression: Suppression,
}

/// The row as it comes back, before the enums are reassembled.
struct Row {
    id: uuid::Uuid,
    tenant_id: uops_core::TenantId,
    reason: String,
    target_resource_id: Option<ResourceId>,
    target_group_id: Option<ResourceGroupId>,
    target_site_id: Option<SiteId>,
    starts_at: DateTime<Utc>,
    duration_minutes: i32,
    timezone: String,
    recurrence: String,
    recur_weekday: Option<i16>,
    recur_day: Option<i16>,
    until: Option<DateTime<Utc>>,
    suppress_alerts: bool,
    suppress_notifications: bool,
    created_by: Option<ActorId>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<Row> for MaintenanceWindow {
    type Error = CoreError;

    fn try_from(r: Row) -> Result<Self> {
        // The schema's CHECK guarantees exactly one target, so the `else` is unreachable
        // through any supported path. It is still an error rather than a panic: the one
        // way to reach it is a row written by hand, and a monitoring server that dies
        // because a DBA typed something at 3am is worse than one that reports it.
        let target = match (r.target_resource_id, r.target_group_id, r.target_site_id) {
            (Some(id), None, None) => Target::Resource(id),
            (None, Some(id), None) => Target::Group(id),
            (None, None, Some(id)) => Target::Site(id),
            _ => {
                return Err(CoreError::Storage(format!(
                    "maintenance window {} does not target exactly one thing",
                    r.id
                )));
            }
        };

        let recurrence = match r.recurrence.as_str() {
            "once" => Recurrence::Once,
            "daily" => Recurrence::Daily,
            "weekly" => Recurrence::Weekly {
                weekday: weekday_from(r.recur_weekday, r.id)?,
            },
            "monthly" => Recurrence::Monthly {
                day: u8::try_from(r.recur_day.unwrap_or(0))
                    .map_err(|_| unknown(r.id, "day of month"))?,
            },
            other => {
                return Err(CoreError::Storage(format!(
                    "maintenance window {} has recurrence {other:?}",
                    r.id
                )));
            }
        };

        Ok(Self {
            id: r.id,
            tenant_id: r.tenant_id,
            reason: r.reason,
            target,
            schedule: Schedule {
                starts_at: r.starts_at,
                duration_minutes: i64::from(r.duration_minutes),
                timezone: r.timezone,
                recurrence,
                until: r.until,
            },
            suppression: Suppression {
                alerts: r.suppress_alerts,
                notifications: r.suppress_notifications,
            },
            created_by: r.created_by,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

fn unknown(id: uuid::Uuid, what: &str) -> CoreError {
    CoreError::Storage(format!("maintenance window {id} has an unreadable {what}"))
}

fn weekday_from(stored: Option<i16>, id: uuid::Uuid) -> Result<chrono::Weekday> {
    use chrono::Weekday;
    // Monday = 0, matching `Weekday::num_days_from_monday`. Written out rather than
    // computed so the mapping is visible: an off-by-one here moves every weekly window
    // in the estate by a day, and nothing would report it.
    match stored {
        Some(0) => Ok(Weekday::Mon),
        Some(1) => Ok(Weekday::Tue),
        Some(2) => Ok(Weekday::Wed),
        Some(3) => Ok(Weekday::Thu),
        Some(4) => Ok(Weekday::Fri),
        Some(5) => Ok(Weekday::Sat),
        Some(6) => Ok(Weekday::Sun),
        _ => Err(unknown(id, "weekday")),
    }
}

/// The columns as `(recurrence, weekday, day)`.
fn recurrence_columns(r: Recurrence) -> (&'static str, Option<i16>, Option<i16>) {
    match r {
        Recurrence::Once => ("once", None, None),
        Recurrence::Daily => ("daily", None, None),
        Recurrence::Weekly { weekday } => (
            "weekly",
            i16::try_from(weekday.num_days_from_monday()).ok(),
            None,
        ),
        Recurrence::Monthly { day } => ("monthly", None, Some(i16::from(day))),
    }
}

impl PgStore {
    /// Schedule a window.
    ///
    /// # Errors
    ///
    /// `Invalid` when the schedule fails [`Schedule::validate`] — checked here so the
    /// caller learns *which* field is wrong instead of a constraint name, and because the
    /// timezone is validated against `chrono-tz` rather than PostgreSQL's own zone table,
    /// which is a different list.
    ///
    /// `NotFound` when the target is not this tenant's. The composite foreign keys refuse
    /// the write, so that is the database's answer rather than a check that could drift.
    pub async fn schedule_maintenance(
        &self,
        scope: &TenantScope,
        by: Option<ActorId>,
        new: &NewWindow,
    ) -> Result<MaintenanceWindow> {
        new.schedule
            .validate()
            .map_err(|e| CoreError::Invalid(e.to_string()))?;
        if new.reason.trim().is_empty() {
            return Err(CoreError::Invalid(
                uops_core::WindowError::NoReason.to_string(),
            ));
        }

        let (recurrence, weekday, day) = recurrence_columns(new.schedule.recurrence);
        let (resource, group, site) = match new.target {
            Target::Resource(id) => (Some(id), None, None),
            Target::Group(id) => (None, Some(id), None),
            Target::Site(id) => (None, None, Some(id)),
        };
        let duration = i32::try_from(new.schedule.duration_minutes)
            .map_err(|_| CoreError::Invalid(uops_core::WindowError::TooLong.to_string()))?;

        // tenant-exempt: the tenant is the first bound parameter, from the scope.
        let row = sqlx::query_as!(
            Row,
            r#"
            INSERT INTO maintenance_window
                (tenant_id, reason,
                 target_resource_id, target_group_id, target_site_id,
                 starts_at, duration_minutes, timezone,
                 recurrence, recur_weekday, recur_day, until,
                 suppress_alerts, suppress_notifications, created_by)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
            RETURNING
                id,
                tenant_id              AS "tenant_id: uops_core::TenantId",
                reason,
                target_resource_id     AS "target_resource_id: ResourceId",
                target_group_id        AS "target_group_id: ResourceGroupId",
                target_site_id         AS "target_site_id: SiteId",
                starts_at, duration_minutes, timezone,
                recurrence, recur_weekday, recur_day, until,
                suppress_alerts, suppress_notifications,
                created_by             AS "created_by: ActorId",
                created_at, updated_at
            "#,
            scope.tenant_id() as uops_core::TenantId,
            new.reason.trim(),
            resource as Option<ResourceId>,
            group as Option<ResourceGroupId>,
            site as Option<SiteId>,
            new.schedule.starts_at,
            duration,
            new.schedule.timezone,
            recurrence,
            weekday,
            day,
            new.schedule.until,
            new.suppression.alerts,
            new.suppression.notifications,
            by as Option<ActorId>,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("maintenance_window", new.reason.clone(), e))?;

        row.try_into()
    }

    /// Every window in the tenant that could still open, soonest first.
    ///
    /// "Could still open" excludes windows whose `until` has passed. It deliberately does
    /// **not** exclude finished one-off windows — a `Recurrence::Once` window from last
    /// year has no `until` and is filtered by the caller's `is_open_at`, because teaching
    /// SQL that rule would be the first of the two copies this module exists to avoid.
    pub async fn live_windows(
        &self,
        scope: &TenantScope,
        now: DateTime<Utc>,
    ) -> Result<Vec<MaintenanceWindow>> {
        let rows = sqlx::query_as!(
            Row,
            r#"
            SELECT
                id,
                tenant_id              AS "tenant_id: uops_core::TenantId",
                reason,
                target_resource_id     AS "target_resource_id: ResourceId",
                target_group_id        AS "target_group_id: ResourceGroupId",
                target_site_id         AS "target_site_id: SiteId",
                starts_at, duration_minutes, timezone,
                recurrence, recur_weekday, recur_day, until,
                suppress_alerts, suppress_notifications,
                created_by             AS "created_by: ActorId",
                created_at, updated_at
              FROM maintenance_window
             WHERE tenant_id = $1
               AND (until IS NULL OR until >= $2)
             ORDER BY starts_at
            "#,
            scope.tenant_id() as uops_core::TenantId,
            now,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("maintenance_window", String::new(), e))?;

        rows.into_iter().map(MaintenanceWindow::try_from).collect()
    }

    /// One window.
    ///
    /// # Errors
    ///
    /// `NotFound` for another tenant's, which is the same answer as for one that never
    /// existed.
    pub async fn maintenance_window(
        &self,
        scope: &TenantScope,
        id: uuid::Uuid,
    ) -> Result<MaintenanceWindow> {
        let row = sqlx::query_as!(
            Row,
            r#"
            SELECT
                id,
                tenant_id              AS "tenant_id: uops_core::TenantId",
                reason,
                target_resource_id     AS "target_resource_id: ResourceId",
                target_group_id        AS "target_group_id: ResourceGroupId",
                target_site_id         AS "target_site_id: SiteId",
                starts_at, duration_minutes, timezone,
                recurrence, recur_weekday, recur_day, until,
                suppress_alerts, suppress_notifications,
                created_by             AS "created_by: ActorId",
                created_at, updated_at
              FROM maintenance_window
             WHERE tenant_id = $1 AND id = $2
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("maintenance_window", id.to_string(), e))?;

        row.try_into()
    }

    /// Cancel a window.
    ///
    /// # Errors
    ///
    /// `NotFound` when it is not this tenant's, rather than a silent success.
    pub async fn cancel_maintenance(&self, scope: &TenantScope, id: uuid::Uuid) -> Result<()> {
        let done = sqlx::query!(
            "DELETE FROM maintenance_window WHERE tenant_id = $1 AND id = $2",
            scope.tenant_id() as uops_core::TenantId,
            id,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("maintenance_window", id.to_string(), e))?;

        if done.rows_affected() == 0 {
            return Err(CoreError::NotFound {
                kind: "maintenance_window",
                id: id.to_string(),
            });
        }
        Ok(())
    }

    /// Which resources one window covers, right now.
    ///
    /// Resolved at read time rather than stored, and that is the point of targeting a
    /// group: a device added to *Dhaka Core Routers* on Friday is covered by Saturday's
    /// window without anybody editing the window.
    pub async fn covered_by(&self, scope: &TenantScope, target: Target) -> Result<Vec<ResourceId>> {
        match target {
            // A resource covers itself. Not expanded to its children: an interface is a
            // resource with its own alerts, and silencing a device should not silently
            // silence forty-eight ports somebody may be watching individually. When that
            // turns out to be the wrong default it is a flag on the window, not a change
            // of meaning here.
            Target::Resource(id) => Ok(vec![id]),
            Target::Group(id) => sqlx::query_scalar!(
                r#"
                    SELECT resource_id AS "id!: ResourceId"
                      FROM resource_group_member
                     WHERE tenant_id = $1 AND group_id = $2
                    "#,
                scope.tenant_id() as uops_core::TenantId,
                id as ResourceGroupId,
            )
            .fetch_all(self.pool())
            .await
            .map_err(|e| map("resource_group", id.to_string(), e)),
            Target::Site(id) => sqlx::query_scalar!(
                r#"
                    SELECT id AS "id!: ResourceId"
                      FROM resource
                     WHERE tenant_id = $1 AND site_id = $2
                    "#,
                scope.tenant_id() as uops_core::TenantId,
                id as SiteId,
            )
            .fetch_all(self.pool())
            .await
            .map_err(|e| map("site", id.to_string(), e)),
        }
    }

    /// Is this resource in maintenance at `at`, and what does that suppress?
    ///
    /// The question the alert engine asks about one resource. `None` means alert normally.
    ///
    /// When two windows cover the same resource, the suppressions are **unioned**: if
    /// either says to suppress alerts, alerts are suppressed. Any other rule would let
    /// adding a second window make the estate noisier than one, which is the opposite of
    /// what somebody scheduling maintenance is asking for.
    pub async fn maintenance_for(
        &self,
        scope: &TenantScope,
        resource: ResourceId,
        at: DateTime<Utc>,
    ) -> Result<Option<Suppression>> {
        let open: Vec<MaintenanceWindow> = self
            .live_windows(scope, at)
            .await?
            .into_iter()
            .filter(|w| w.is_open_at(at))
            .collect();

        let mut found: Option<Suppression> = None;
        for window in open {
            if self
                .covered_by(scope, window.target)
                .await?
                .contains(&resource)
            {
                found = Some(match found {
                    None => window.suppression,
                    Some(prior) => Suppression {
                        alerts: prior.alerts || window.suppression.alerts,
                        notifications: prior.notifications || window.suppression.notifications,
                    },
                });
            }
        }
        Ok(found)
    }
}
