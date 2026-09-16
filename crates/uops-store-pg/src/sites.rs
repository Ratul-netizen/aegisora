//! Sites, and what is happening at them.
//!
//! The map's query. One row per site, with the coordinate an operator placed and a count
//! of its resources by status — which is everything a pin needs: where to draw it, how
//! big, and what colour.
//!
//! # Why the rollup is one query and not one per site
//!
//! A tenant with forty sites would otherwise be forty-one round trips to draw a map, and
//! the map is the first screen somebody opens. `FILTER (WHERE ...)` does the whole
//! estate in one pass over an index this schema already has — `resource_tenant_kind_idx`
//! is `(tenant_id, kind)` and the scan is per tenant either way.
//!
//! # Why sites with no coordinate are still returned
//!
//! Most of them will have none: an operator places the sites that matter and leaves the
//! rest. The map draws what it can and the caller lists the remainder, which is more
//! useful than a map that silently omits half the estate — a site that is missing from a
//! map looks like a site with nothing wrong.

use uops_core::{Result, SiteId, TenantScope};

use crate::error::map;
use crate::store::PgStore;

/// A site, where it is, and how its resources are doing.
#[derive(Clone, Debug, PartialEq)]
pub struct SiteOverview {
    pub id: SiteId,
    pub name: String,
    pub timezone: String,
    /// `None` when nobody has placed this site. Both halves or neither — the schema
    /// refuses one without the other.
    pub location: Option<Location>,
    pub resources: StatusCounts,
}

/// Signed degrees, WGS 84.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
}

/// How many resources are in each state.
///
/// Counted rather than summarised to a single worst-case, because a pin showing "one of
/// two hundred is down" and a pin showing "two hundred of two hundred are down" are the
/// same colour and very different mornings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatusCounts {
    pub up: i64,
    pub down: i64,
    pub degraded: i64,
    pub unknown: i64,
    pub maintenance: i64,
    /// Decommissioned resources are excluded from every count above and from `total`.
    /// They are kept so history resolves, and a map that showed them would show an
    /// estate the customer no longer has.
    pub total: i64,
}

impl PgStore {
    /// Every site in the scope's tenant, with its location and a status rollup.
    ///
    /// # Errors
    ///
    /// Storage failures.
    pub async fn site_overview(&self, scope: &TenantScope) -> Result<Vec<SiteOverview>> {
        let rows = sqlx::query!(
            r#"
            SELECT s.id        AS "id: SiteId",
                   s.name,
                   s.timezone,
                   s.latitude,
                   s.longitude,
                   -- LEFT JOIN, so a site with no resources is still a row. A new site
                   -- an operator has just placed should appear on the map before
                   -- anything is put in it.
                   count(r.id) FILTER (WHERE r.status = 'up')          AS "up!",
                   count(r.id) FILTER (WHERE r.status = 'down')        AS "down!",
                   count(r.id) FILTER (WHERE r.status = 'degraded')    AS "degraded!",
                   count(r.id) FILTER (WHERE r.status = 'unknown')     AS "unknown!",
                   count(r.id) FILTER (WHERE r.status = 'maintenance') AS "maintenance!",
                   count(r.id)                                         AS "total!"
              FROM site s
              LEFT JOIN resource r
                ON r.site_id = s.id
               AND r.tenant_id = s.tenant_id
               AND r.status <> 'decommissioned'
             WHERE s.tenant_id = $1
             GROUP BY s.id, s.name, s.timezone, s.latitude, s.longitude
             ORDER BY s.name
            "#,
            scope.tenant_id() as uops_core::TenantId,
        )
        .fetch_all(self.pool())
        .await
        .map_err(|e| map("site", String::new(), e))?;

        Ok(rows
            .into_iter()
            .map(|r| SiteOverview {
                id: r.id,
                name: r.name,
                timezone: r.timezone,
                // The schema guarantees both or neither, so this cannot produce half a
                // location — but it is written as a pair rather than two unwraps so that
                // it stays true if the constraint is ever relaxed.
                location: match (r.latitude, r.longitude) {
                    (Some(latitude), Some(longitude)) => Some(Location {
                        latitude,
                        longitude,
                    }),
                    _ => None,
                },
                resources: StatusCounts {
                    up: r.up,
                    down: r.down,
                    degraded: r.degraded,
                    unknown: r.unknown,
                    maintenance: r.maintenance,
                    total: r.total,
                },
            })
            .collect())
    }

    /// Place a site on the map, or take it off.
    ///
    /// `None` clears the location. The schema refuses half a coordinate, so there is no
    /// way to express one here either.
    ///
    /// # Errors
    ///
    /// Storage failures, a site in another tenant, or a coordinate outside its range —
    /// which the database refuses and which arrives here as `Invalid` rather than as a
    /// constraint name.
    pub async fn place_site(
        &self,
        scope: &TenantScope,
        id: SiteId,
        location: Option<Location>,
    ) -> Result<()> {
        // Checked here as well as in the schema. The constraint is what makes it true;
        // this is what makes the message say which number was wrong, which a caller
        // filling in a form needs and a constraint violation does not give.
        if let Some(l) = location {
            if !(-90.0..=90.0).contains(&l.latitude) {
                return Err(uops_core::Error::Invalid(format!(
                    "latitude {} is outside -90..=90",
                    l.latitude
                )));
            }
            if !(-180.0..=180.0).contains(&l.longitude) {
                return Err(uops_core::Error::Invalid(format!(
                    "longitude {} is outside -180..=180",
                    l.longitude
                )));
            }
        }

        let affected = sqlx::query!(
            r#"
            UPDATE site
               SET latitude = $3, longitude = $4
             WHERE tenant_id = $1 AND id = $2
            "#,
            scope.tenant_id() as uops_core::TenantId,
            id as SiteId,
            location.map(|l| l.latitude),
            location.map(|l| l.longitude),
        )
        .execute(self.pool())
        .await
        .map_err(|e| map("site", id.to_string(), e))?
        .rows_affected();

        if affected == 0 {
            // 404-never-403: a site in another tenant is indistinguishable from one that
            // does not exist, because confirming the id would leak another customer's
            // estate.
            return Err(uops_core::Error::NotFound {
                kind: "site",
                id: id.to_string(),
            });
        }
        Ok(())
    }
}
