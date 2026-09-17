//! Dashboards — SPEC §M4.
//!
//! A panel is `{ query, viz }`: the same `Query` AST the Explorer posts, a saved search
//! stores and an alert rule holds, plus how to draw it. There is no fourth representation
//! of a question anywhere in this product, and a panel is the third thing that proves it —
//! a saved search with a picture on it.
//!
//! # Every panel's query is compiled before the dashboard is stored
//!
//! The same check saving a search and writing a rule make, for a third reason: a panel
//! that cannot be answered renders as an error box on a wall display that nobody is
//! looking at closely, next to nineteen panels that work. Refusing it at save time puts
//! the sentence in front of the person who can fix it.
//!
//! # The window in a panel's query is provenance
//!
//! Same as a saved search, same as a rule. A dashboard is read over the time range in its
//! header, and every panel's window is replaced when it is read — the *span* is not even
//! kept here, because unlike an alert rule ("avg over five minutes") a panel is a picture
//! of whatever range the person is looking at.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uops_core::{ActorId, Error as CoreError, Result, TenantScope};
use uops_query::{Query, ResolvedResources, compile};

use crate::error::map;
use crate::store::PgStore;

/// How a panel draws what its query returned.
///
/// The five SPEC names for v0.1, and deliberately not heatmap, geo map, topology or pie:
/// *"five types built well beats twelve built badly"*, and two of those four depend on M6
/// and M7 anyway.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Viz {
    /// A line per series over the panel's window.
    TimeSeries {
        #[serde(default)]
        unit: String,
    },
    /// One number: the most recent value the query returned.
    Stat {
        #[serde(default)]
        unit: String,
        #[serde(default = "two")]
        decimals: u8,
    },
    /// The rows as they came back. The escape hatch for a question no picture answers.
    Table,
    /// One number against a range somebody chose.
    ///
    /// `min` and `max` are required rather than derived from the data: a gauge whose
    /// bounds move with what it is showing is a gauge that always reads half full.
    Gauge {
        min: f64,
        max: f64,
        #[serde(default)]
        unit: String,
    },
    /// What is firing right now. The one panel with no query of its own — it reads the
    /// alert list, which is the control plane rather than telemetry.
    Alerts,
}

const fn two() -> u8 {
    2
}

impl Viz {
    /// Whether this kind of panel reads telemetry.
    #[must_use]
    pub const fn needs_query(&self) -> bool {
        !matches!(self, Self::Alerts)
    }

    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::TimeSeries { .. } => "time_series",
            Self::Stat { .. } => "stat",
            Self::Table => "table",
            Self::Gauge { .. } => "gauge",
            Self::Alerts => "alerts",
        }
    }
}

/// One panel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Panel {
    /// Stable for the life of the panel, so a client can key a list on it and a layout
    /// change does not remount every chart on the page.
    pub id: String,
    pub title: String,
    /// `None` only for [`Viz::Alerts`] — enforced by [`validate`].
    #[serde(default)]
    pub query: Option<Query>,
    pub viz: Viz,
    /// Columns of twelve. Twelve because it divides by two, three, four and six, which is
    /// every layout somebody actually asks for.
    #[serde(default = "six")]
    pub width: u8,
    /// Rows of a fixed height, decided by the client. Stored because the layout is part of
    /// the dashboard rather than of the browser that drew it last.
    #[serde(default = "two_rows")]
    pub height: u8,
}

const fn six() -> u8 {
    6
}

const fn two_rows() -> u8 {
    2
}

/// A dashboard as stored.
#[derive(Clone, Debug)]
pub struct Dashboard {
    pub id: uuid::Uuid,
    pub tenant_id: uops_core::TenantId,
    pub name: String,
    pub description: String,
    pub panels: Vec<Panel>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What a caller supplies to create or replace one.
#[derive(Clone, Debug)]
pub struct NewDashboard {
    pub name: String,
    pub description: String,
    pub panels: Vec<Panel>,
}

/// Reject a dashboard that could not be drawn.
///
/// Three things, each of which otherwise becomes an error box on a wall display:
/// a panel whose query the compiler refuses, a telemetry panel with no query at all, and
/// an alert panel carrying one it would ignore.
fn validate(panels: &[Panel], scope: &TenantScope) -> Result<()> {
    for panel in panels {
        match (&panel.query, panel.viz.needs_query()) {
            (Some(query), true) => {
                compile(query, scope, &ResolvedResources::whole_tenant(scope))
                    .map(|_| ())
                    .map_err(|e| {
                        CoreError::Invalid(format!("panel {}: {e}", panel.title.trim()))
                    })?;
            }
            (None, true) => {
                return Err(CoreError::Invalid(format!(
                    "panel {} is a {} and has no query — it would draw nothing",
                    panel.title.trim(),
                    panel.viz.as_str()
                )));
            }
            // An alerts panel with a query attached is a misunderstanding worth
            // correcting rather than silently ignoring: somebody built a query expecting
            // it to filter the alert list, and it would not.
            (Some(_), false) => {
                return Err(CoreError::Invalid(format!(
                    "panel {} shows the alert list and cannot take a query",
                    panel.title.trim()
                )));
            }
            (None, false) => {}
        }
    }

    Ok(())
}

impl PgStore {
    /// Create a dashboard.
    pub async fn create_dashboard(
        &self,
        scope: &TenantScope,
        by: Option<ActorId>,
        new: &NewDashboard,
    ) -> Result<Dashboard> {
        validate(&new.panels, scope)?;
        let panels = serde_json::to_value(&new.panels)?;

        // tenant-exempt: the tenant is the first bound parameter, from the scope.
        let row = sqlx::query!(
            r#"
            INSERT INTO dashboard (tenant_id, name, description, panels, created_by)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id, tenant_id AS "tenant_id: uops_core::TenantId",
                      name, description, panels, created_at, updated_at
            "#,
            scope.tenant_id() as uops_core::TenantId,
            new.name.trim(),
            new.description,
            panels,
            by as Option<ActorId>,
        )
        .fetch_one(self.pool())
        .await
        .map_err(|e| map("dashboard", new.name.clone(), e))?;

        Ok(Dashboard {
            id: row.id,
            tenant_id: row.tenant_id,
            name: row.name,
            description: row.description,
            panels: serde_json::from_value(row.panels)?,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }

    /// A tenant's dashboards, by name.
    ///
    /// Panels included: a list of dashboards is a handful of rows, and the alternative is
    /// a second request per dashboard to find out how many panels each has.
    pub async fn dashboards(&self, scope: &TenantScope) -> Result<Vec<Dashboard>> {
        // tenant-exempt: the tenant is the only bound parameter, from the scope.
        let rows = sqlx::query!(
            r#"
            SELECT id, tenant_id AS "tenant_id: uops_core::TenantId",
                   name, description, panels, created_at, updated_at
              FROM dashboard
             WHERE tenant_id = $1
             ORDER BY name
            "#,
            scope.tenant_id() as uops_core::TenantId,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("dashboard", "list".to_owned(), e))?;

        rows.into_iter()
            .map(|row| {
                Ok(Dashboard {
                    id: row.id,
                    tenant_id: row.tenant_id,
                    name: row.name,
                    description: row.description,
                    panels: serde_json::from_value(row.panels)?,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                })
            })
            .collect()
    }

    /// One dashboard.
    ///
    /// # Errors
    ///
    /// `NotFound` for another tenant's, which is the same answer as for one that does not
    /// exist.
    pub async fn dashboard(&self, scope: &TenantScope, id: uuid::Uuid) -> Result<Dashboard> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query!(
            r#"
            SELECT id, tenant_id AS "tenant_id: uops_core::TenantId",
                   name, description, panels, created_at, updated_at
              FROM dashboard
             WHERE tenant_id = $1 AND id = $2
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("dashboard", id.to_string(), e))?
        .ok_or(CoreError::NotFound {
            kind: "dashboard",
            id: id.to_string(),
        })?;

        Ok(Dashboard {
            id: row.id,
            tenant_id: row.tenant_id,
            name: row.name,
            description: row.description,
            panels: serde_json::from_value(row.panels)?,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }

    /// Replace a dashboard.
    ///
    /// Whole, like a saved search and an alert rule: a dashboard is a document, and a
    /// partial update of a panel list is a merge nobody can review.
    pub async fn update_dashboard(
        &self,
        scope: &TenantScope,
        id: uuid::Uuid,
        new: &NewDashboard,
    ) -> Result<Dashboard> {
        validate(&new.panels, scope)?;
        let panels = serde_json::to_value(&new.panels)?;

        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let row = sqlx::query!(
            r#"
            UPDATE dashboard
               SET name = $3, description = $4, panels = $5
             WHERE tenant_id = $1 AND id = $2
            RETURNING id, tenant_id AS "tenant_id: uops_core::TenantId",
                      name, description, panels, created_at, updated_at
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id,
            new.name.trim(),
            new.description,
            panels,
        )
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map("dashboard", new.name.clone(), e))?
        .ok_or(CoreError::NotFound {
            kind: "dashboard",
            id: id.to_string(),
        })?;

        Ok(Dashboard {
            id: row.id,
            tenant_id: row.tenant_id,
            name: row.name,
            description: row.description,
            panels: serde_json::from_value(row.panels)?,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }

    /// Delete a dashboard.
    pub async fn delete_dashboard(&self, scope: &TenantScope, id: uuid::Uuid) -> Result<()> {
        // tenant-exempt: the tenant is a bound parameter, from the scope.
        let done = sqlx::query!(
            "DELETE FROM dashboard WHERE tenant_id = $1 AND id = $2",
            scope.tenant_id() as uops_core::TenantId,
            id,
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("dashboard", id.to_string(), e))?;

        if done.rows_affected() == 0 {
            return Err(CoreError::NotFound {
                kind: "dashboard",
                id: id.to_string(),
            });
        }
        Ok(())
    }
}
