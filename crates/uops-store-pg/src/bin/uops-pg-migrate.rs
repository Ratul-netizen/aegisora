//! `uops-pg-migrate` — apply the `PostgreSQL` control-plane schema.
//!
//! ```text
//! uops-pg-migrate            apply everything outstanding
//! uops-pg-migrate --status   list what is applied and what is not, change nothing
//!
//! DATABASE_URL   default postgres://uops:uops@localhost:5432/uops
//! ```
//!
//! # Why this exists when `sqlx migrate run` already does it
//!
//! Not to replace sqlx-cli, which is what a developer uses. To keep it out of the
//! container image. sqlx-cli is a large build for a tool whose entire job here is one
//! function call, and the migrations are embedded in this binary at compile time by
//! `sqlx::migrate!`, so the deployed artefact cannot disagree with the code that was
//! built alongside it — there is no directory to forget to copy and no version skew
//! between the runner and the files.
//!
//! # Why the server does not call this itself
//!
//! Deliberately a separate process. A server that migrates on boot is N replicas racing
//! on a rolling deploy, and a rollback that has quietly become a data migration. This
//! runs once, as its own step, with output somebody reads — see the module docs in
//! `uops-server`'s `main.rs`.

use std::process::ExitCode;

use uops_store_pg::{Config, PgStore};

/// Embedded at compile time from the workspace's `migrations/` directory.
static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("uops-pg-migrate: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let status_only = std::env::args().any(|a| a == "--status");

    let config = Config::from_env();
    let store = PgStore::connect(&config)
        .await
        .map_err(|e| format!("cannot reach PostgreSQL: {e}"))?;

    if status_only {
        // Queried directly rather than through the Migrator, which has no public
        // accessor for this. An absent table means an unmigrated database, not a
        // failure: reporting "0 applied" is the correct answer to the question.
        let applied: Vec<i64> = sqlx::query_scalar(
            "SELECT version FROM _sqlx_migrations WHERE success ORDER BY version",
        )
        .fetch_all(store.pool())
        .await
        .unwrap_or_default();

        let mut outstanding = 0;
        for m in MIGRATIONS.iter() {
            let is_applied = applied.contains(&m.version);
            if !is_applied {
                outstanding += 1;
            }
            println!(
                "{:>4}  {}  {}",
                m.version,
                if is_applied { "applied" } else { "pending" },
                m.description
            );
        }
        println!(
            "{} migration(s) on disk, {} applied, {outstanding} outstanding",
            MIGRATIONS.iter().count(),
            applied.len()
        );
        return Ok(());
    }

    // sqlx takes an advisory lock for the duration, so two replicas starting together
    // serialise here rather than both applying the same DDL. Same reasoning as the
    // bootstrap lock, and the same mechanism.
    MIGRATIONS
        .run(store.pool())
        .await
        .map_err(|e| format!("migration failed: {e}"))?;

    println!("schema is up to date");
    Ok(())
}
