//! What the daemon is told, and what it refuses to start without.
//!
//! # How a syslog message gets a tenant
//!
//! This is the decision the whole file exists to record, because a syslog message
//! **carries no tenant and cannot be made to**. RFC 5424 has structured data and nobody
//! populates it; RFC 3164 has a hostname and a body. There is no field to put a customer
//! in, and if there were, the sender would control it.
//!
//! So the tenant comes from **where the message arrived**: one listener per tenant, on
//! its own address or port. That is how every multi-tenant collector does it, and the
//! reason is that the binding is the one thing the sender cannot influence.
//!
//! The alternative considered and not taken was an explicit sender-address allow-list,
//! with unknown senders refused. It is a tighter posture and it loses the logs of every
//! device somebody forgot to register — which are disproportionately the devices
//! involved in an incident, because an unregistered device is one nobody is watching.
//! An operator who wants that posture can have it at the firewall, where it belongs, and
//! where it is one rule rather than a second identity system.
//!
//! **An unknown sender inside a listener's tenant is not dropped.** The resolver creates
//! a provisional resource and a review-queue item, which is rule 1 of SPEC §M0.2 —
//! *never block ingestion* — and means a device that starts logging before anybody adds
//! it to inventory still has its logs when somebody goes looking.
//!
//! # Why a file and not environment variables
//!
//! A listener list is inherently a list, and `UOPS_SYSLOG_LISTENER_0_UDP` is not
//! configuration, it is a workaround. The monitoring profiles are YAML for the same
//! reason: a human writes this, reviews it, and puts it in a change ticket.
//!
//! The connection strings stay in the environment, because those carry passwords and a
//! file on disk is a file in a backup.

use std::collections::BTreeSet;
use std::net::SocketAddr;

use serde::Deserialize;

/// Where the listener list is read from.
pub const CONFIG_PATH: &str = "UOPS_SYSLOG_CONFIG";

/// One tenant's ingress.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Listener {
    /// The tenant's slug, as `tenant.slug` in PostgreSQL. A slug rather than a uuid
    /// because a human writes this file, and a mistyped uuid is a silent
    /// misattribution while a mistyped slug fails at startup.
    pub tenant: String,
    /// `0.0.0.0:514`, or absent for a TCP-only listener.
    #[serde(default)]
    pub udp: Option<SocketAddr>,
    /// `0.0.0.0:601`, or absent for a UDP-only listener.
    #[serde(default)]
    pub tcp: Option<SocketAddr>,
    /// What `source_vendor` says for rows from here, when the resource itself does not
    /// know. Empty by default: a guess that disagrees with the resource's own vendor is
    /// worse than a blank.
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
    /// How many rows may wait between the receivers and the batcher.
    ///
    /// The queue that absorbs a burst while the batcher is mid-insert. Bounded, because
    /// the whole point of a bound is that the drop is visible and counted rather than an
    /// out-of-memory kill that loses everything.
    pub queue: usize,
    /// How many tasks resolve identity in parallel, per listener.
    ///
    /// One would be enough for the cached path, which is a mutex and an LRU lookup. It
    /// is not enough for the *uncached* path: a resolution that has to reach PostgreSQL
    /// awaits, and with a single worker one slow lookup stalls every message behind it.
    pub workers: usize,
}

/// The default ingest queue, in rows.
///
/// Large enough to cover a multi-second `ClickHouse` insert at the 50 000 msg/s target,
/// small enough that the memory is bounded and the drop counter moves before the process
/// does.
pub const DEFAULT_QUEUE: usize = 200_000;

impl Config {
    /// Read the environment and the listener file.
    ///
    /// # Errors
    ///
    /// Anything missing or unreadable, named. A daemon that starts with no listeners
    /// would sit there looking healthy and ingesting nothing, which is the failure this
    /// product is least able to notice.
    pub fn load() -> Result<Self, String> {
        let path = std::env::var(CONFIG_PATH)
            .map_err(|_| format!("{CONFIG_PATH} is not set; it must name the listener file"))?;
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("{CONFIG_PATH}={path} could not be read: {e}"))?;
        let file: File = serde_yaml_ng::from_str(&text)
            .map_err(|e| format!("{path} is not a valid listener file: {e}"))?;

        Self::from_parts(file, std::env::var("DATABASE_URL").ok())
    }

    fn from_parts(file: File, database_url: Option<String>) -> Result<Self, String> {
        validate(&file.listeners)?;

        let postgres = uops_store_pg::Config {
            url: database_url.ok_or_else(|| "DATABASE_URL is not set".to_owned())?,
            ..uops_store_pg::Config::default()
        };

        Ok(Self {
            listeners: file.listeners,
            postgres,
            clickhouse: uops_store_ch::ChConfig::from_env(),
            queue: DEFAULT_QUEUE,
            workers: std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get),
        })
    }

    /// One line for the log, with nothing secret in it.
    #[must_use]
    pub fn summary(&self) -> String {
        let binds: Vec<String> = self
            .listeners
            .iter()
            .map(|l| {
                let mut on = Vec::new();
                if let Some(a) = l.udp {
                    on.push(format!("udp {a}"));
                }
                if let Some(a) = l.tcp {
                    on.push(format!("tcp {a}"));
                }
                format!("{} on {}", l.tenant, on.join(" + "))
            })
            .collect();
        format!(
            "{} listener(s): {}; queue {}, {} workers each",
            self.listeners.len(),
            binds.join(", "),
            self.queue,
            self.workers,
        )
    }
}

/// Everything about the listener list that can be wrong.
fn validate(listeners: &[Listener]) -> Result<(), String> {
    if listeners.is_empty() {
        return Err(
            "the listener file has no listeners; a syslog daemon with nothing bound \
             would look healthy and ingest nothing"
                .to_owned(),
        );
    }

    let mut seen_tenants = BTreeSet::new();
    let mut seen_addresses = BTreeSet::new();

    for listener in listeners {
        if listener.tenant.trim().is_empty() {
            return Err("a listener has no tenant".to_owned());
        }
        if listener.udp.is_none() && listener.tcp.is_none() {
            return Err(format!(
                "the listener for {} binds neither udp nor tcp",
                listener.tenant
            ));
        }
        if !seen_tenants.insert(listener.tenant.clone()) {
            // Two listeners for one tenant would work, and would also be the shape of
            // the mistake where somebody copies a block and forgets to change the
            // tenant — at which case one customer's logs land in another's. Refused,
            // because a tenant wanting two ports can have one listener with both.
            return Err(format!(
                "{} has more than one listener; give it one listener with both \
                 transports instead",
                listener.tenant
            ));
        }
        for address in [listener.udp, listener.tcp].into_iter().flatten() {
            if !seen_addresses.insert(address) {
                // The bind would fail at startup anyway. Catching it here means the
                // message names the tenants rather than being an errno.
                return Err(format!(
                    "{address} is bound by more than one listener; each tenant needs \
                     its own address or port, because the binding is what identifies \
                     the tenant"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(yaml: &str) -> File {
        serde_yaml_ng::from_str(yaml).expect("parse")
    }

    fn load(yaml: &str) -> Result<Config, String> {
        Config::from_parts(
            file(yaml),
            Some("postgres://u:secret@localhost/db".to_owned()),
        )
    }

    #[test]
    fn a_listener_file_reads_as_written() {
        let config = load(
            r"
listeners:
  - tenant: acme
    udp: 0.0.0.0:514
    tcp: 0.0.0.0:601
    vendor: cisco
  - tenant: globex
    udp: 0.0.0.0:1514
",
        )
        .expect("load");

        assert_eq!(config.listeners.len(), 2);
        assert_eq!(config.listeners[0].tenant, "acme");
        assert_eq!(config.listeners[0].vendor, "cisco");
        assert!(config.listeners[1].tcp.is_none(), "tcp is optional");
        assert_eq!(
            config.listeners[1].vendor, "",
            "no vendor is a blank, not a guess"
        );
    }

    #[test]
    fn a_daemon_with_no_listeners_refuses_to_start() {
        // It would otherwise sit there looking healthy and ingesting nothing, which is
        // the failure this product is least able to notice — there is no alert for
        // "logs stopped arriving from a device that has not sent any yet".
        let err = load("listeners: []").expect_err("must refuse");
        assert!(err.contains("no listeners"), "{err}");

        // And the same for a file with the key missing entirely.
        assert!(load("{}").is_err());
    }

    #[test]
    fn a_listener_that_binds_nothing_is_refused() {
        let err = load(
            r"
listeners:
  - tenant: acme
",
        )
        .expect_err("must refuse");
        assert!(err.contains("neither udp nor tcp"), "{err}");
    }

    #[test]
    fn two_listeners_for_one_tenant_are_refused() {
        // Not because it could not work, but because it is the shape of the mistake
        // where somebody copies a block and forgets to change the tenant — and the
        // consequence of that mistake is one customer's logs in another's account.
        let err = load(
            r"
listeners:
  - tenant: acme
    udp: 0.0.0.0:514
  - tenant: acme
    tcp: 0.0.0.0:601
",
        )
        .expect_err("must refuse");
        assert!(err.contains("more than one listener"), "{err}");
    }

    #[test]
    fn two_tenants_on_one_address_are_refused() {
        // The bind would fail at startup anyway; catching it here means the message
        // names the tenants rather than being an errno on a socket.
        let err = load(
            r"
listeners:
  - tenant: acme
    udp: 0.0.0.0:514
  - tenant: globex
    udp: 0.0.0.0:514
",
        )
        .expect_err("must refuse");
        assert!(err.contains("bound by more than one"), "{err}");

        // Across transports too: the same address cannot be udp for one and tcp for
        // another, because that is exactly as confusing and just as likely a typo.
        assert!(
            load(
                r"
listeners:
  - tenant: acme
    udp: 0.0.0.0:514
  - tenant: globex
    tcp: 0.0.0.0:514
",
            )
            .is_err()
        );
    }

    #[test]
    fn the_summary_has_nothing_secret_in_it() {
        // It goes in the startup log, which goes in support tickets.
        let config = load(
            r"
listeners:
  - tenant: acme
    udp: 0.0.0.0:514
",
        )
        .expect("load");
        let summary = config.summary();
        assert!(summary.contains("acme") && summary.contains("udp 0.0.0.0:514"));
        assert!(!summary.contains("postgres://"), "{summary}");
        assert!(
            !summary.contains("secret"),
            "the connection string's password must not reach the log: {summary}"
        );
    }

    #[test]
    fn a_missing_database_url_is_named() {
        let err = Config::from_parts(
            file(
                r"
listeners:
  - tenant: acme
    udp: 0.0.0.0:514
",
            ),
            None,
        )
        .expect_err("must refuse");
        assert!(err.contains("DATABASE_URL"), "{err}");
    }
}
