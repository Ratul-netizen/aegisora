//! The `ClickHouse` HTTP interface.
//!
//! One POST per statement, the SQL as the body, parameters as `param_<name>` in the
//! query string. That is the whole protocol — which is why the interesting parts of
//! this crate are somewhere else.
//!
//! Plaintext HTTP only, deliberately. See the note in `Cargo.toml`: every deployment
//! shape M0 supports reaches `ClickHouse` over a private network, and the TLS-enabled
//! dependency trees carry licences outside the `cargo-deny` allow-list. [`Executor`] is
//! the seam that a TLS transport plugs into when the deployment profiles need one.

use crate::error::{Error, Result};
use crate::runner::Executor;

/// Where the server is and who to be.
#[derive(Clone, Debug)]
pub struct Config {
    pub url: String,
    pub database: String,
    pub user: String,
    pub password: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            url: "http://localhost:8123".into(),
            database: "uops".into(),
            user: "default".into(),
            password: String::new(),
        }
    }
}

impl Config {
    /// Read the standard environment variables, falling back to the defaults.
    #[must_use]
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            url: std::env::var("CLICKHOUSE_URL").unwrap_or(d.url),
            database: std::env::var("CLICKHOUSE_DB").unwrap_or(d.database),
            user: std::env::var("CLICKHOUSE_USER").unwrap_or(d.user),
            password: std::env::var("CLICKHOUSE_PASSWORD").unwrap_or(d.password),
        }
    }
}

pub struct HttpExecutor {
    agent: ureq::Agent,
    config: Config,
}

impl std::fmt::Debug for HttpExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The password is in `config`. Print what identifies the server and nothing
        // that authenticates to it.
        f.debug_struct("HttpExecutor")
            .field("url", &self.config.url)
            .field("database", &self.config.database)
            .field("user", &self.config.user)
            .finish_non_exhaustive()
    }
}

impl HttpExecutor {
    #[must_use]
    pub fn new(config: Config) -> Self {
        let agent = ureq::Agent::config_builder()
            // ClickHouse reports errors as a non-2xx status with the message in the
            // body. Treating the status as a transport error would throw away the only
            // useful part — the line and column of the syntax error.
            .http_status_as_error(false)
            .build()
            .into();
        Self { agent, config }
    }
}

impl Executor for HttpExecutor {
    fn run(&self, sql: &str, params: &[(&str, String)]) -> Result<String> {
        let mut req = self
            .agent
            .post(&self.config.url)
            .query("database", &self.config.database)
            .query("user", &self.config.user)
            .query("password", &self.config.password);

        for (name, value) in params {
            req = req.query(format!("param_{name}"), value);
        }

        let mut response = req.send(sql).map_err(|e| Error::Unreachable {
            url: self.config.url.clone(),
            detail: e.to_string(),
        })?;

        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::Protocol(e.to_string()))?;

        if status >= 400 {
            // ClickHouse error bodies are several lines of stack; the first line is the
            // one that says what is wrong.
            let first = body.lines().next().unwrap_or(&body).trim();
            return Err(Error::Server(format!("HTTP {status}: {first}")));
        }

        Ok(body)
    }
}
