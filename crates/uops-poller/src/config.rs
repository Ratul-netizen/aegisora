//! Everything the poller reads from its environment.
//!
//! Three of these are chosen against convenience.
//!
//! **There is no default KEK.** The server can start without one because nothing it
//! does on a fresh database needs to open a credential. The poller's whole job is to
//! open credentials, so a poller with no key ring is a process that will start happily
//! and then fail on every device — which looks like a network problem for as long as it
//! takes someone to read the logs. It refuses to start instead, naming the variable.
//!
//! **The reload interval is a minute, not a second.** Devices are added by a human
//! through the API; re-reading the fleet on every tick would mean a query per second per
//! tenant for the benefit of noticing a new switch up to fifty-nine seconds sooner.
//!
//! **The device budget defaults below the tick.** A device given longer than a second
//! to answer can still be in flight when its next poll comes due, and SPEC §M2's first
//! acceptance criterion is about a *fleet*, where one slow device holding a slot is how
//! the whole schedule slips.

use std::time::Duration;

use uops_secrets::record::KeyId;
use uops_store_ch::ChConfig;
use uops_store_pg::Config as PgConfig;

/// The poller's configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub postgres: PgConfig,
    pub clickhouse: ChConfig,
    /// Where the key-encryption key comes from. See [`KekSource`].
    pub kek: KekSource,
    pub kek_id: KeyId,
    /// How often the fleet is re-read from `PostgreSQL`.
    pub reload_every: Duration,
    /// How many devices one tenant may contribute. A bound on the query, not a policy.
    pub device_limit: i64,
    pub limits: Limits,
}

/// Concurrency and time bounds, mirrored from `uops_poll::executor::Limits`.
///
/// Repeated here rather than re-exported so they can be read from the environment
/// without `uops-poll` growing a dependency on how this binary is configured.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub global: usize,
    pub per_device: usize,
    pub device_budget: Duration,
}

impl From<Limits> for uops_poll::Limits {
    fn from(l: Limits) -> Self {
        Self {
            global: l.global,
            per_device: l.per_device,
            device_budget: l.device_budget,
        }
    }
}

/// Where the KEK is read from.
///
/// A file is the default shape for an on-premise deployment — it can have permissions,
/// which an environment variable cannot, and `KekRing::from_file` rejects a
/// group-readable one outright. The variable exists because a container platform's
/// secret store usually presents as one.
#[derive(Debug, Clone)]
pub enum KekSource {
    File(std::path::PathBuf),
    Env(String),
}

/// Something in the environment is unusable.
#[derive(Debug)]
pub struct ConfigError {
    pub variable: &'static str,
    pub problem: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately does not print the value. The only variables that reach this are
        // the KEK's location and the connection URLs, and both are things a support
        // ticket should not carry.
        write!(f, "{} is not usable: {}", self.variable, self.problem)
    }
}

impl std::error::Error for ConfigError {}

fn var(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// A positive integer from the environment, or the default.
///
/// An unparseable value is an error rather than a fallback: `UOPS_POLL_GLOBAL=1O`
/// silently becoming 512 is the kind of thing that is discovered during an incident.
fn number<T: std::str::FromStr>(name: &'static str, default: T) -> Result<T, ConfigError> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(raw) => raw.trim().parse().map_err(|_| ConfigError {
            variable: name,
            problem: "expected a positive whole number".to_owned(),
        }),
    }
}

/// Seconds from the environment, or the default.
fn seconds(name: &'static str, default: u64) -> Result<Duration, ConfigError> {
    Ok(Duration::from_secs(number(name, default)?))
}

impl Config {
    /// Read the environment.
    ///
    /// # Errors
    ///
    /// When no KEK is configured, or when a numeric variable is not a number.
    pub fn from_env() -> Result<Self, ConfigError> {
        let kek = match (
            std::env::var("UOPS_KEK_FILE").ok(),
            std::env::var("UOPS_KEK_HEX").ok(),
        ) {
            (Some(path), _) => KekSource::File(path.into()),
            (None, Some(_)) => KekSource::Env("UOPS_KEK_HEX".to_owned()),
            (None, None) => {
                return Err(ConfigError {
                    variable: "UOPS_KEK_FILE",
                    problem: "the poller cannot open a credential without a key-encryption \
                              key; set UOPS_KEK_FILE to a file of 64 hex characters, or \
                              UOPS_KEK_HEX to the characters themselves"
                        .to_owned(),
                });
            }
        };

        let budget = seconds("UOPS_POLL_DEVICE_BUDGET", 1)?;

        Ok(Self {
            postgres: PgConfig::from_env(),
            clickhouse: ChConfig::from_env(),
            kek,
            kek_id: KeyId(var("UOPS_KEK_ID", "default")),
            reload_every: seconds("UOPS_POLL_RELOAD_SECS", 60)?,
            device_limit: number("UOPS_POLL_DEVICE_LIMIT", 10_000)?,
            limits: Limits {
                global: number("UOPS_POLL_GLOBAL", 512)?,
                per_device: number("UOPS_POLL_PER_DEVICE", 4)?,
                device_budget: budget,
            },
        })
    }

    /// What the startup banner says.
    ///
    /// Not `Display`: the configs inside hold URLs with passwords in them, and a
    /// `Display` impl is exactly the kind of thing that later gets used in an error
    /// message. This has to be called on purpose, and prints only hosts.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "postgres={} clickhouse={} kek={} reload={}s global={} per_device={} budget={}s",
            redact(&self.postgres.url),
            redact(&self.clickhouse.url),
            match &self.kek {
                KekSource::File(p) => format!("file {}", p.display()),
                KekSource::Env(v) => format!("env {v}"),
            },
            self.reload_every.as_secs(),
            self.limits.global,
            self.limits.per_device,
            self.limits.device_budget.as_secs(),
        )
    }
}

/// A connection URL with any credentials removed.
///
/// Duplicated from `uops-server`'s config rather than shared: the two binaries have no
/// crate in common below them that is the right home for it, and the alternative — a
/// dependency edge from the poller to the server — would be far worse than nine lines.
/// The tests below are this copy's, not a re-run of that one's.
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
        assert_eq!(redact("http://localhost:8123"), "http://localhost:8123");
        assert_eq!(
            redact("postgres://uops:p@ss@db:5432/uops"),
            "postgres://db:5432/uops"
        );
        assert_eq!(redact("not a url"), "not a url");
    }

    #[test]
    fn a_number_that_is_not_one_is_refused_rather_than_defaulted() {
        // The unset path, which is the one `number` owns. The parse itself is std's and
        // is asserted directly rather than through the environment: mutating it is
        // unsafe in Rust 2024, forbidden in this workspace, and racy across the test
        // threads regardless.
        assert_eq!(number::<usize>("UOPS_UNSET_FOR_THIS_TEST", 7).unwrap(), 7);
        assert!("1O".trim().parse::<usize>().is_err());
        assert_eq!(" 512 ".trim().parse::<usize>().unwrap(), 512);
    }
}
