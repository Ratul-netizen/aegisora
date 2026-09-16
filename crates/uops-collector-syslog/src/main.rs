//! The syslog daemon.
//!
//! Everything below this file has tests; this file is the order those things happen in,
//! which is the part an operator experiences:
//!
//! ```text
//!   read the listener file        fail here — a daemon with nothing bound ingests nothing
//!   connect to PostgreSQL         fail here, saying which host
//!   resolve the tenant slugs      fail here, naming the slug that is not there
//!   connect to ClickHouse         fail here, saying which host
//!   bind the sockets              fail here, saying which port and what to do about it
//!   receive until told to stop
//!   drain
//! ```
//!
//! # Why the slugs are resolved before a socket is opened
//!
//! A daemon that bound its ports and *then* found a slug was mistyped would be accepting
//! messages it had nowhere to put, and its receivers would be counting drops that were
//! really a configuration error. Failing before the first bind means the message names
//! the slug, which is what somebody can act on.
//!
//! # Why it does not migrate
//!
//! Same reason as `uops-server` and `uops-poller`: migrations are DDL and belong to
//! `scripts/db.sh migrate` and `uops-ch-migrate`. A collector that migrated on startup
//! would make N replicas race to alter a schema, and the first thing a syslog daemon does
//! under load is get more replicas.
//!
//! # Why more than one replica *is* safe here, unlike the poller
//!
//! The poller has no lease, so two of them would poll every device twice. This has
//! nothing to schedule: each replica owns its own sockets, and a sender reaches one of
//! them. Two replicas behind a load balancer each resolve identity independently — which
//! costs a second cache warm-up and, briefly, can have both create a provisional resource
//! for the same unknown device. `UNIQUE (tenant_id, kind, value)` makes the second one
//! lose, so the outcome is one resource and one extra review item, not two resources.

use std::process::ExitCode;

use uops_collector_syslog::{config::Config, run, shutdown};
use uops_store_ch::{ChClient, ChStore, TelemetryStore};
use uops_store_pg::PgStore;

#[tokio::main]
async fn main() -> ExitCode {
    match start().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // One line, on stderr. Not a panic: a backtrace through tokio's internals
            // tells an operator nothing they can act on, and buries the sentence that
            // does.
            eprintln!("uops-collector-syslog: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn start() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    println!("uops-collector-syslog starting: {}", config.summary());

    let store = PgStore::connect(&config.postgres)
        .await
        // The URL is not repeated here: it carries a password, and the summary above
        // already said everything that is safe to say.
        .map_err(|e| format!("cannot reach PostgreSQL: {e}"))?;

    // Before any socket is bound. See the module docs.
    let bound = run::resolve_tenants(&store, &config).await?;

    let telemetry = ChStore::new(ChClient::new(config.clickhouse.clone()));
    let ch = telemetry
        .health()
        .await
        .map_err(|e| format!("cannot reach ClickHouse: {e}"))?;
    println!("uops-collector-syslog: clickhouse {} ready", ch.version);

    run::serve(store, telemetry, &config, bound, shutdown::signal()).await?;
    println!("uops-collector-syslog: stopped cleanly");
    Ok(())
}
