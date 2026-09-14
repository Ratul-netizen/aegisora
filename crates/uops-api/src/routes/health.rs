//! `GET /api/v1/health` — the one route with no authentication.
//!
//! Read by a container orchestrator that has no session and cannot get one, so it is
//! outside the audit layer and outside `Caller`. That makes it the one place where a
//! careless addition leaks something to an unauthenticated caller, so what it reports
//! is deliberately short:
//!
//! * whether each store answered
//! * the `ClickHouse` server version
//!
//! The version is there because on-premise support asks for it constantly and because
//! the text index syntax moved between 25.x and 26.x — knowing which server is actually
//! running is the difference between a five-minute diagnosis and an afternoon. It is
//! also the one piece of genuine reconnaissance value here, which is the trade: a
//! version string is not a secret to anyone who can reach the port, and an operator who
//! cannot see it debugs blind.
//!
//! Nothing else. Not the database URL, not the tenant count, not the build path.
//!
//! # Degraded is still 200
//!
//! A store being unreachable makes `ok` false and leaves the status at 200. The
//! alternative — 503 when `ClickHouse` is down — makes an orchestrator restart or
//! remove a process that is working correctly and telling you the truth about its
//! dependency. A restart does not fix someone else's database.

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use uops_store_ch::TelemetryStore;

use crate::state::AppState;

/// What `/api/v1/health` returns.
#[derive(Debug, Serialize)]
pub struct Health {
    /// True only when every store answered.
    pub ok: bool,
    pub control_plane: ComponentHealth,
    pub telemetry: ComponentHealth,
}

/// One dependency's state. No error text: a connection error can carry a host, a port
/// and sometimes a username, and this is read by anyone who can reach the port.
#[derive(Debug, Serialize)]
pub struct ComponentHealth {
    pub reachable: bool,
    /// The server's own version, when it answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// `GET /api/v1/health`
pub async fn health(State(state): State<AppState>) -> Json<Health> {
    let control_plane = ComponentHealth {
        reachable: state.store.health().await.is_ok(),
        version: None,
    };

    let telemetry = match state.telemetry.health().await {
        Ok(h) => ComponentHealth {
            reachable: h.reachable,
            version: Some(h.version),
        },
        Err(_) => ComponentHealth {
            reachable: false,
            version: None,
        },
    };

    Json(Health {
        ok: control_plane.reachable && telemetry.reachable,
        control_plane,
        telemetry,
    })
}
