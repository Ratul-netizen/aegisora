//! Sites, and where they are — SPEC §M6's map, ahead of the rest of it.
//!
//! Two verbs. Reading gives every site with its coordinate and a rollup of its
//! resources by status, which is everything a pin needs: where to draw it, how big, and
//! what colour. Writing places a site, or takes it off the map.
//!
//! # Why placing a site is an operator's job and not a lookup
//!
//! Geo-IP answers a different question, and for this product it usually answers nothing:
//! a management address is RFC 1918 and resolves nowhere, and a public address resolves
//! to whoever registered the block rather than to the equipment. An operator typing a
//! coordinate once per site is more accurate than a lookup that is wrong in a way nobody
//! can see — and it is once per *site*, not once per device.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use uops_core::{Role, SiteId};
use uops_store_pg::Location;

use crate::csrf::CsrfChecked;
use crate::error::ApiResult;
use crate::extract::Caller;
use crate::state::AppState;

/// One site, as the map draws it.
#[derive(Debug, Serialize)]
pub struct SiteView {
    pub id: SiteId,
    pub name: String,
    pub timezone: String,
    /// Absent when nobody has placed this site. Most sites, for most customers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Coordinate>,
    pub resources: Counts,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Coordinate {
    pub latitude: f64,
    pub longitude: f64,
}

/// Resources by status.
///
/// Counted rather than reduced to a worst case: "one of two hundred is down" and "two
/// hundred of two hundred are down" are the same colour and very different mornings.
#[derive(Debug, Serialize)]
pub struct Counts {
    pub up: i64,
    pub down: i64,
    pub degraded: i64,
    pub unknown: i64,
    pub maintenance: i64,
    /// Decommissioned resources are in none of the above and not in this either.
    pub total: i64,
}

/// `GET /api/v1/sites`
///
/// Every site in the tenant, placed or not. Not paginated: a tenant has sites in the
/// tens, the map needs all of them at once to draw, and a cursor would be a second round
/// trip for a screen that has to render in one.
pub async fn list(State(state): State<AppState>, caller: Caller) -> ApiResult<Json<Vec<SiteView>>> {
    caller.require(Role::Viewer)?;

    let sites = state.store.site_overview(caller.scope()).await?;
    caller.audit().read(
        "site.list",
        Some(i64::try_from(sites.len()).unwrap_or(i64::MAX)),
    );

    Ok(Json(
        sites
            .into_iter()
            .map(|s| SiteView {
                id: s.id,
                name: s.name,
                timezone: s.timezone,
                location: s.location.map(|l| Coordinate {
                    latitude: l.latitude,
                    longitude: l.longitude,
                }),
                resources: Counts {
                    up: s.resources.up,
                    down: s.resources.down,
                    degraded: s.resources.degraded,
                    unknown: s.resources.unknown,
                    maintenance: s.resources.maintenance,
                    total: s.resources.total,
                },
            })
            .collect(),
    ))
}

/// What a client sends to place a site.
///
/// `null` clears it. The two halves travel together because half a coordinate is not a
/// location, and the schema refuses one anyway.
#[derive(Debug, Deserialize)]
pub struct Placement {
    #[serde(default)]
    pub location: Option<Coordinate>,
}

/// `PUT /api/v1/sites/{id}/location`
///
/// Operator, not viewer: this changes what everybody's map shows.
pub async fn place(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<SiteId>,
    _csrf: CsrfChecked,
    Json(body): Json<Placement>,
) -> ApiResult<StatusCode> {
    caller.require(Role::Operator)?;

    let location = body.location.map(|c| Location {
        latitude: c.latitude,
        longitude: c.longitude,
    });
    state.store.place_site(caller.scope(), id, location).await?;

    caller.audit().wrote(
        "site.location",
        format!("site:{id}"),
        None,
        Some(match location {
            Some(l) => serde_json::json!({
                "latitude": l.latitude,
                "longitude": l.longitude,
            }),
            // Distinguishable from "not recorded": the audit row for taking a site off
            // the map should say that is what happened.
            None => serde_json::Value::Null,
        }),
    );

    Ok(StatusCode::NO_CONTENT)
}
