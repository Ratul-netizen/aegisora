//! The OTLP receiver.
//!
//! Everything below this file has tests; this file is the order those things happen in:
//!
//! ```text
//!   read the listener file        fail here -- a receiver with nothing bound ingests nothing
//!   connect to PostgreSQL         fail here, saying which host
//!   resolve the tenant slugs      fail here, naming the slug that is not there
//!   connect to ClickHouse         fail here, saying which host
//!   bind the sockets              fail here, saying which address
//!   receive until told to stop
//!   drain
//! ```
//!
//! # Why it does not migrate, and why replicas are safe
//!
//! Both for the same reasons as `uops-collector-syslog`: migrations are DDL and belong to
//! `scripts/db.sh migrate`, and this schedules nothing, so two instances behind a load
//! balancer each resolve independently and `UNIQUE (tenant_id, kind, value)` makes the
//! second creation of a provisional resource lose rather than duplicate.
//!
//! # No KEK
//!
//! An OTLP receiver opens no credentials. It reads a socket and writes rows, so it is not
//! given the key that decrypts every credential in the installation.

use std::process::ExitCode;

use uops_collector_otlp::{config::Config, run, shutdown};
use uops_store_ch::{ChClient, ChStore, TelemetryStore};
use uops_store_pg::PgStore;

#[tokio::main]
async fn main() -> ExitCode {
    match start().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("uops-collector-otlp: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn start() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;
    println!("uops-collector-otlp starting: {}", config.summary());

    let store = PgStore::connect(&config.postgres)
        .await
        .map_err(|e| format!("cannot reach PostgreSQL: {e}"))?;

    // Before any socket is bound.
    let bound = run::resolve_tenants(&store, &config).await?;

    let telemetry = ChStore::new(ChClient::new(config.clickhouse.clone()));
    let ch = telemetry
        .health()
        .await
        .map_err(|e| format!("cannot reach ClickHouse: {e}"))?;
    println!("uops-collector-otlp: clickhouse {} ready", ch.version);

    run::serve(store, telemetry, &config, bound, shutdown::signal()).await?;
    println!("uops-collector-otlp: stopped cleanly");
    Ok(())
}
