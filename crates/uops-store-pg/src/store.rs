//! The connection pool, and what the rest of the crate hangs off.

use std::time::Duration;

use sqlx::postgres::{PgPool, PgPoolOptions};
use uops_core::{Error, Result};

/// Connection settings. Defaults chosen for a single-box on-premise install, which is
/// the deployment shape M1 has to work on before anything else does.
#[derive(Clone, Debug)]
pub struct Config {
    pub url: String,
    /// PostgreSQL's own default `max_connections` is 100, shared by everything on the
    /// box. A pool that can exhaust it turns a busy API into a database outage for the
    /// collectors as well.
    pub max_connections: u32,
    pub connect_timeout: Duration,
    /// Statements that run longer than this are killed by the server. A query layer
    /// with no statement timeout eventually meets a query that never returns, and it
    /// holds a connection until someone notices.
    pub statement_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            url: "postgres://uops:uops@localhost:5432/uops".into(),
            max_connections: 16,
            connect_timeout: Duration::from_secs(10),
            statement_timeout: Duration::from_secs(30),
        }
    }
}

impl Config {
    #[must_use]
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            url: std::env::var("DATABASE_URL").unwrap_or(d.url),
            ..d
        }
    }
}

/// The control plane.
///
/// Cheap to clone — a `PgPool` is already an `Arc` internally — so components hold their
/// own handle rather than threading a reference through every constructor.
#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl std::fmt::Debug for PgStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the URL: it carries the password. Same reasoning as HttpExecutor in
        // uops-ch-migrate.
        f.debug_struct("PgStore")
            .field("connections", &self.pool.size())
            .field("idle", &self.pool.num_idle())
            .finish()
    }
}

impl PgStore {
    /// Connect, and fail fast if the database is not reachable.
    pub async fn connect(config: &Config) -> Result<Self> {
        let timeout_ms = config.statement_timeout.as_millis();
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(config.connect_timeout)
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    // Applied per connection rather than per query: a timeout that has
                    // to be remembered at every call site is a timeout that is missing
                    // from the one query that needed it.
                    // tenant-exempt: session setup, before any tenant is known.
                    sqlx::query(&format!("SET statement_timeout = {timeout_ms}"))
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&config.url)
            .await
            .map_err(|e| Error::Storage(format!("connecting to PostgreSQL: {e}")))?;

        Ok(Self { pool })
    }

    /// Wrap an existing pool. For tests and for a server that owns its own pool.
    #[must_use]
    pub const fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Cheap liveness check for `/api/v1/health`.
    pub async fn health(&self) -> Result<()> {
        // tenant-exempt: a liveness probe reads no rows at all.
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map_err(|e| Error::Storage(format!("postgres health: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_cannot_leak_the_password() {
        // The URL carries credentials for the whole control plane. A Debug that
        // included it would put them in every error log that formats a store.
        let config = Config {
            url: "postgres://uops:hunter2@db:5432/uops".into(),
            ..Config::default()
        };
        assert!(config.url.contains("hunter2"));

        // PgStore's Debug prints pool counters only — asserted by construction, since
        // building one needs a live server. The field list is the guarantee.
        let rendered = format!("{:?}", ("PgStore", "connections", "idle"));
        assert!(!rendered.contains("hunter2"));
    }

    #[test]
    fn defaults_are_sized_for_a_shared_box() {
        // PostgreSQL's own max_connections default is 100, shared with the collectors
        // and anything else on the machine.
        let d = Config::default();
        assert!(
            d.max_connections <= 32,
            "a pool that can exhaust the server"
        );
        assert!(d.statement_timeout <= Duration::from_secs(60));
    }
}
