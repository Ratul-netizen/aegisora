//! The poller.
//!
//! Everything below this file has tests; this file is the order those things happen in,
//! which is the part that cannot be unit-tested and the part an operator experiences:
//!
//! ```text
//!   read the environment          fail here, before anything is opened
//!   open the key ring             fail here — a poller with no KEK polls nothing
//!   connect to PostgreSQL         fail here, saying which host
//!   connect to ClickHouse         fail here, saying which host
//!   seed the built-in profiles    so a fresh database has something to poll under
//!   load the fleet                fail here if it cannot be read at all
//!   poll until told to stop
//! ```
//!
//! # Why it opens the key ring before it connects to anything
//!
//! A KEK that is missing, malformed, or in a file other accounts can read is a
//! configuration error, and a configuration error should be found before a single
//! connection is made. Doing it in this order means the failure names the variable
//! rather than arriving later as "every device refused the credentials", which sends an
//! operator to look at the network.
//!
//! # Why it seeds profiles and does not migrate
//!
//! Seeding is an upsert of definitions this build ships — data, idempotent, and safe for
//! N replicas to race on. Migrations are DDL and are `scripts/db.sh migrate` and
//! `uops-ch-migrate`, for the reasons `uops-server`'s `main` sets out.
//!
//! # Why more than one poller is not yet safe
//!
//! There is no lease. Two pollers against one database would both schedule every device
//! and poll it twice, which doubles the load on the fleet and writes each sample twice.
//! One process for now; the lease is M2's remaining scale work and is in STATUS.

use std::process::ExitCode;
use std::sync::Arc;

use uops_poller::{config::Config, credentials, run, shutdown};
use uops_store_ch::{ChClient, ChStore, TelemetryStore};
use uops_store_pg::PgStore;

#[tokio::main]
async fn main() -> ExitCode {
    match start().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // One line, on stderr, saying what failed. Not a panic: a backtrace through
            // tokio's internals tells an operator nothing they can act on, and buries
            // the sentence that does.
            eprintln!("uops-poller: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn start() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    println!("uops-poller starting: {}", config.summary());

    let store = PgStore::connect(&config.postgres)
        .await
        // The URL is in the summary above, already redacted. Repeating it here
        // unredacted is how a password ends up in a support ticket.
        .map_err(|e| format!("cannot reach PostgreSQL: {e}"))?;
    store
        .health()
        .await
        .map_err(|e| format!("PostgreSQL is reachable but not answering: {e}"))?;

    // After the connection, because it needs the store; before anything is polled,
    // because a poller that cannot open a credential has nothing to do. The error names
    // the variable — see config.rs.
    let vault = credentials::vault(store.clone(), &config)
        .map_err(|e| format!("the key ring could not be opened: {e}"))?;

    let metrics = ChStore::new(ChClient::new(config.clickhouse.clone()));
    let ch = metrics
        .health()
        .await
        .map_err(|e| format!("cannot reach ClickHouse: {e}"))?;
    println!("uops-poller: clickhouse {} ready", ch.version);

    let seeded = store
        .seed_builtin_profiles(&uops_profile::builtin::all()?)
        .await
        .map_err(|e| format!("the built-in profiles could not be seeded: {e}"))?;
    println!("uops-poller: {seeded} built-in profiles up to date");

    let runner = Arc::new(run::Runner::new(
        store,
        metrics,
        credentials::Transports::new(vault),
        config.limits.device_budget,
    ));

    run::serve(runner, &config, shutdown::signal()).await?;
    println!("uops-poller: stopped cleanly");
    Ok(())
}
