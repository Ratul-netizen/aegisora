//! Everything the server reads from its environment.
//!
//! Two defaults here are chosen against convenience, and both are worth stating.
//!
//! **The listener defaults to loopback.** A server that binds `0.0.0.0` by default is
//! reachable from the whole network the moment it starts, including during the minutes
//! between the first boot and the operator reading the printed password. Binding
//! `0.0.0.0` is a thing you say out loud, in `UOPS_BIND`, once you mean it.
//!
//! **Cookies are `Secure` unless told otherwise.** The exception exists because a
//! developer on `http://localhost` would otherwise watch the browser silently discard
//! every cookie and see an app that is broken for a reason nothing logs. It is spelled
//! `UOPS_INSECURE_COOKIES=1`, which is hard to type by accident and easy to find in a
//! deployment that should not have it.

use std::net::SocketAddr;

use uops_store_ch::ChConfig;
use uops_store_pg::Config as PgConfig;

/// The server's configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub postgres: PgConfig,
    pub clickhouse: ChConfig,
    pub secure_cookies: bool,
    pub first_run: FirstRunNames,
}

// There is deliberately no shutdown grace period here. Draining waits for the requests
// already accepted, and how long that is allowed to take is already decided by whoever
// sends the signal — Kubernetes' terminationGracePeriodSeconds, compose's stop_grace_period,
// an operator's second ctrl-C. A timeout in this process would be a second, quieter
// deadline that silently wins some of the time, and a request killed by it looks
// identical to a crash from the client's side.

/// What the first boot calls the organization, tenant and administrator it creates.
///
/// Only ever read on a database with no users. Changing these afterwards changes
/// nothing, which is why they are plain names and not a migration.
#[derive(Debug, Clone)]
pub struct FirstRunNames {
    pub org: String,
    pub tenant: String,
    pub tenant_slug: String,
    pub admin_email: String,
    pub admin_name: String,
}

/// Something in the environment is unusable.
#[derive(Debug)]
pub struct ConfigError {
    pub variable: &'static str,
    pub value: String,
    pub problem: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is not usable: {} ({})",
            self.variable, self.problem, self.value
        )
    }
}

impl std::error::Error for ConfigError {}

fn var(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// True for `1`, `true`, `yes`, `on`; false for anything else.
///
/// Deliberately not an error on an unrecognised value. The only flag read this way
/// turns a protection *off*, so the safe reading of `UOPS_INSECURE_COOKIES=maybe` is
/// "no", not "refuse to start".
fn is_affirmative(value: &str) -> bool {
    matches!(value.to_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Whether an environment variable is set to something affirmative.
///
/// Split from [`is_affirmative`] so the decision can be tested without a test that
/// mutates the process environment — which is both racy across threads and, since Rust
/// 2024, unsafe, and this workspace forbids unsafe.
fn flag(name: &str) -> bool {
    is_affirmative(&std::env::var(name).unwrap_or_default())
}

impl Config {
    /// Read the environment.
    ///
    /// # Errors
    ///
    /// When `UOPS_BIND` is not an address. Everything else has a default, because a
    /// server that refuses to start until five variables are set is a server nobody
    /// tries twice.
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind = var("UOPS_BIND", "127.0.0.1:8080");
        let bind = bind.parse::<SocketAddr>().map_err(|e| ConfigError {
            variable: "UOPS_BIND",
            value: bind.clone(),
            problem: e.to_string(),
        })?;

        Ok(Self {
            bind,
            postgres: PgConfig::from_env(),
            clickhouse: ChConfig::from_env(),
            secure_cookies: !flag("UOPS_INSECURE_COOKIES"),
            first_run: FirstRunNames {
                org: var("UOPS_ORG_NAME", "Default organization"),
                tenant: var("UOPS_TENANT_NAME", "Default tenant"),
                tenant_slug: var("UOPS_TENANT_SLUG", "default"),
                admin_email: var("UOPS_ADMIN_EMAIL", "admin@localhost"),
                admin_name: var("UOPS_ADMIN_NAME", "Administrator"),
            },
        })
    }

    /// What the startup banner says.
    ///
    /// Not `Display` on `Config`: the `PgConfig` inside holds a URL with a password in
    /// it, and a `Display` impl is exactly the kind of thing that later gets used in an
    /// error message. This is a function that must be called on purpose, and it prints
    /// only the host.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "bind={} postgres={} clickhouse={} secure_cookies={}",
            self.bind,
            redact(&self.postgres.url),
            redact(&self.clickhouse.url),
            self.secure_cookies,
        )
    }
}

/// A connection URL with any credentials removed.
///
/// `postgres://uops:hunter2@db:5432/uops` becomes `postgres://db:5432/uops`. Startup
/// output ends up in support tickets, screenshots and CI logs, and a URL is the most
/// common way a password reaches all three.
fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    // rsplit: a password may itself contain an `@`, and splitting on the first one
    // would leave the tail of the password in what gets printed as the host.
    match rest.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://{host}"),
        None => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_loses_its_credentials() {
        assert_eq!(
            redact("postgres://uops:hunter2@db.internal:5432/uops"),
            "postgres://db.internal:5432/uops"
        );
        // No credentials, nothing to remove.
        assert_eq!(redact("http://localhost:8123"), "http://localhost:8123");
        // A password containing an @ — the split must take the last one, or the host
        // comes back as part of the password and the password as part of the scheme.
        assert_eq!(
            redact("postgres://uops:p@ss@db:5432/uops"),
            "postgres://db:5432/uops"
        );
        // Not a URL at all. Returned unchanged rather than mangled, because the caller
        // is about to print it and a half-parsed string is worse than the original.
        assert_eq!(redact("not a url"), "not a url");
    }

    #[test]
    fn only_an_affirmative_turns_a_protection_off() {
        for value in ["1", "true", "TRUE", "yes", "on"] {
            assert!(is_affirmative(value), "{value:?} should turn the flag on");
        }
        // Unset arrives here as "", and everything unrecognised must read as off —
        // this flag only ever removes a protection.
        for value in ["0", "false", "no", "off", "maybe", "", " 1", "1 "] {
            assert!(
                !is_affirmative(value),
                "{value:?} should leave the flag off"
            );
        }
    }
}
