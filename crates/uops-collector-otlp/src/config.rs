//! What the receiver is told, and what it refuses to start without.
//!
//! # How an OTLP request gets a tenant
//!
//! The same answer as syslog, and for the same reason: **one listener per tenant**. An
//! OTLP request carries resource attributes that describe the *emitter* — `host.id`,
//! `service.name` — and nothing that says which customer it belongs to. Anything that did
//! would be a field the sender controls.
//!
//! So the tenant comes from where the request arrived, because the binding is the one
//! thing a sender cannot influence.
//!
//! **The alternative here is stronger than it was for syslog, and is still not taken
//! yet.** OTLP exporters send arbitrary headers — the Collector's `otlphttp` has a
//! `headers:` block — so a per-tenant token would be idiomatic, would let one endpoint
//! serve every tenant, and is how most hosted OTLP endpoints work. It is deferred because
//! it is a **new credential type**: something to mint, show once, rotate, revoke and
//! audit, and `uops-secrets` already owns those decisions. Adding a second, weaker
//! credential path beside it to save a port would be the wrong trade.
//!
//! An unknown emitter inside a listener's tenant is not rejected. The resolver creates a
//! provisional resource and a review item — rule 1 of SPEC §M0.2 — so an application that
//! starts reporting before anybody adds it to inventory still has its telemetry.

use std::collections::BTreeSet;
use std::net::SocketAddr;

use serde::Deserialize;

/// Where the listener list is read from.
pub const CONFIG_PATH: &str = "UOPS_OTLP_CONFIG";

/// Where rows go when `ClickHouse` will not take them.
///
/// Unset means no spill, which is a supported deployment — see the syslog daemon's
/// equivalent. Logs and metrics get **separate subdirectories** underneath: a segment does
/// not record its row type, so a directory holding both would read a metric back as a log
/// and fail every line.
pub const SPILL_PATH: &str = "UOPS_OTLP_SPILL";

/// One tenant's ingress.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Listener {
    /// The tenant's slug, as `tenant.slug` in `PostgreSQL`. A slug rather than a uuid
    /// because a human writes this file, and a mistyped uuid is a silent misattribution
    /// while a mistyped slug fails at startup.
    pub tenant: String,
    /// `0.0.0.0:4318` — the port OTLP/HTTP uses by convention.
    pub bind: SocketAddr,
    /// What `source_vendor` says for rows from here when the resource does not know.
    #[serde(default)]
    pub vendor: String,
}

/// The whole file.
#[derive(Clone, Debug, Deserialize, Default)]
pub struct File {
    #[serde(default)]
    pub listeners: Vec<Listener>,
}

/// What `main` needs.
#[derive(Clone, Debug)]
pub struct Config {
    pub listeners: Vec<Listener>,
    pub postgres: uops_store_pg::Config,
    pub clickhouse: uops_store_ch::ChConfig,
    pub spill: Option<std::path::PathBuf>,
    /// How many rows may wait between the handlers and a batcher.
    ///
    /// Smaller than the syslog daemon's, because the shapes differ: syslog is a firehose
    /// of single messages over UDP and this is a few large requests a second over TCP. A
    /// full queue here means the handler waits, which becomes the exporter waiting, which
    /// is exactly the backpressure HTTP is for.
    pub queue: usize,
    /// The largest request body accepted.
    ///
    /// The Collector batches, so a legitimate request can be megabytes. A cap is still
    /// required: without one an unauthenticated endpoint is an allocation somebody else
    /// controls.
    pub max_body: usize,
}

/// A generous default for a batching collector, and far short of anything alarming.
pub const DEFAULT_MAX_BODY: usize = 16 * 1024 * 1024;

/// Rows between the handlers and a batcher.
pub const DEFAULT_QUEUE: usize = 100_000;

impl Config {
    /// Read the environment and the listener file.
    ///
    /// # Errors
    ///
    /// Anything missing or unreadable, named.
    pub fn load() -> Result<Self, String> {
        let path = std::env::var(CONFIG_PATH)
            .map_err(|_| format!("{CONFIG_PATH} is not set; it must name the listener file"))?;
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("{CONFIG_PATH}={path} could not be read: {e}"))?;
        let file: File = serde_yaml_ng::from_str(&text)
            .map_err(|e| format!("{path} is not a valid listener file: {e}"))?;

        Self::from_parts(file, std::env::var("DATABASE_URL").ok()).map(|c| Config {
            spill: std::env::var(SPILL_PATH).ok().map(std::path::PathBuf::from),
            ..c
        })
    }

    fn from_parts(file: File, database_url: Option<String>) -> Result<Self, String> {
        validate(&file.listeners)?;

        Ok(Self {
            listeners: file.listeners,
            postgres: uops_store_pg::Config {
                url: database_url.ok_or_else(|| "DATABASE_URL is not set".to_owned())?,
                ..uops_store_pg::Config::default()
            },
            clickhouse: uops_store_ch::ChConfig::from_env(),
            spill: None,
            queue: DEFAULT_QUEUE,
            max_body: DEFAULT_MAX_BODY,
        })
    }

    /// One line for the log, with nothing secret in it.
    #[must_use]
    pub fn summary(&self) -> String {
        let binds: Vec<String> = self
            .listeners
            .iter()
            .map(|l| format!("{} on {}", l.tenant, l.bind))
            .collect();
        let spill = self.spill.as_ref().map_or_else(
            || format!("no spill (set {SPILL_PATH} to survive a long ClickHouse outage)"),
            |p| format!("spill {}", p.display()),
        );
        format!(
            "{} listener(s): {}; queue {}, body limit {} MiB, {spill}",
            self.listeners.len(),
            binds.join(", "),
            self.queue,
            self.max_body / (1024 * 1024),
        )
    }
}

fn validate(listeners: &[Listener]) -> Result<(), String> {
    if listeners.is_empty() {
        return Err(
            "the listener file has no listeners; a receiver with nothing bound would look \
             healthy and ingest nothing"
                .to_owned(),
        );
    }

    let mut tenants = BTreeSet::new();
    let mut addresses = BTreeSet::new();
    for listener in listeners {
        if listener.tenant.trim().is_empty() {
            return Err("a listener has no tenant".to_owned());
        }
        if !tenants.insert(listener.tenant.clone()) {
            // The shape of the mistake where somebody copies a block and forgets to
            // change the tenant, whose consequence is one customer's telemetry in
            // another's account.
            return Err(format!(
                "{} has more than one listener; one tenant needs one address",
                listener.tenant
            ));
        }
        if !addresses.insert(listener.bind) {
            return Err(format!(
                "{} is bound by more than one listener; each tenant needs its own address \
                 or port, because the binding is what identifies the tenant",
                listener.bind
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(yaml: &str) -> Result<Config, String> {
        Config::from_parts(
            serde_yaml_ng::from_str(yaml).expect("parse"),
            Some("postgres://u:secret@localhost/db".to_owned()),
        )
    }

    #[test]
    fn a_listener_file_reads_as_written() {
        let config = load(
            r"
listeners:
  - tenant: acme
    bind: 0.0.0.0:4318
    vendor: acme-corp
  - tenant: globex
    bind: 0.0.0.0:4319
",
        )
        .expect("load");

        assert_eq!(config.listeners.len(), 2);
        assert_eq!(config.listeners[0].bind.port(), 4318);
        assert_eq!(config.listeners[0].vendor, "acme-corp");
        assert_eq!(config.listeners[1].vendor, "", "no vendor is a blank");
    }

    #[test]
    fn a_receiver_with_no_listeners_refuses_to_start() {
        // It would otherwise sit there looking healthy and ingesting nothing.
        assert!(load("listeners: []").is_err());
        assert!(load("{}").is_err());
    }

    #[test]
    fn two_listeners_for_one_tenant_are_refused() {
        // The copy-paste mistake whose consequence is one customer's telemetry in
        // another's account.
        let err = load(
            r"
listeners:
  - tenant: acme
    bind: 0.0.0.0:4318
  - tenant: acme
    bind: 0.0.0.0:4319
",
        )
        .expect_err("must refuse");
        assert!(err.contains("more than one listener"), "{err}");
    }

    #[test]
    fn two_tenants_on_one_address_are_refused() {
        let err = load(
            r"
listeners:
  - tenant: acme
    bind: 0.0.0.0:4318
  - tenant: globex
    bind: 0.0.0.0:4318
",
        )
        .expect_err("must refuse");
        assert!(err.contains("bound by more than one"), "{err}");
    }

    #[test]
    fn the_summary_has_nothing_secret_in_it() {
        let config = load(
            r"
listeners:
  - tenant: acme
    bind: 0.0.0.0:4318
",
        )
        .expect("load");
        let summary = config.summary();
        assert!(summary.contains("acme") && summary.contains("0.0.0.0:4318"));
        assert!(!summary.contains("secret"), "{summary}");
        // And it says when there is no spill, because somebody who thinks they
        // configured one should find out at startup rather than during the outage.
        assert!(summary.contains("no spill"), "{summary}");
    }
}
