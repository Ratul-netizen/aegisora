//! The server.
//!
//! Everything below this file has tests; this file is the order those things happen in,
//! which is the part that cannot be unit-tested and the part an operator experiences.
//!
//! ```text
//!   read the environment          fail here, before anything is opened
//!   connect to PostgreSQL         fail here, saying which host
//!   connect to ClickHouse         fail here, saying which host
//!   bootstrap if empty            print the credential, once
//!   bind the port                 nothing is reachable before this line
//!   serve until told to stop      finish what was accepted
//! ```
//!
//! # Why it does not run migrations
//!
//! A server that migrates its own schema on boot is convenient exactly once. After that
//! it is N replicas racing to apply the same DDL on a rolling deploy, a rollback that
//! has become a data migration, and a schema change that happens at the least observable
//! moment in the deployment. Migrations are `scripts/db.sh migrate` and `uops-ch-migrate`
//! — steps a human or a pipeline runs, with output someone reads.
//!
//! What this does instead is check that both stores answer before it binds a port, and
//! say which one did not when they do not.
//!
//! # Why the port is bound last
//!
//! Between opening a listener and finishing bootstrap there would be a window in which
//! the API is reachable and the first administrator does not yet exist. Nothing terrible
//! is reachable through that window today — every route requires a session, and there
//! are no sessions — but "nothing terrible is reachable *today*" is a property of the
//! current route table rather than of the design. Binding last removes the window
//! instead of arguing about what is in it.

use uops_server::{config, firstrun, shutdown};

use std::net::SocketAddr;
use std::process::ExitCode;

use uops_api::AppState;
use uops_api::routes::router;
use uops_store_ch::{ChClient, ChStore, TelemetryStore};
use uops_store_pg::PgStore;

use crate::config::Config;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // One line, on stderr, saying what failed. Not a panic: a backtrace through
            // tokio's internals tells an operator nothing they can act on, and buries
            // the sentence that does.
            eprintln!("uops-server: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    println!("uops-server starting: {}", config.summary());

    let store = PgStore::connect(&config.postgres).await.map_err(|e| {
        // The URL is in the config summary above, already redacted. Repeating it here
        // unredacted is how a password ends up in a support ticket.
        format!("cannot reach PostgreSQL: {e}")
    })?;
    store
        .health()
        .await
        .map_err(|e| format!("PostgreSQL is reachable but not answering: {e}"))?;

    let telemetry = ChStore::new(ChClient::new(config.clickhouse.clone()));
    let ch = telemetry
        .health()
        .await
        .map_err(|e| format!("cannot reach ClickHouse: {e}"))?;
    println!("clickhouse {} ready", ch.version);

    firstrun::run(&store, &config.first_run).await?;

    let state = if config.secure_cookies {
        AppState::new(store, telemetry)
    } else {
        // Named to be visible in a diff, and announced to be visible in a log. A
        // deployment that has this on has it on for a reason someone can now find.
        eprintln!("warning: UOPS_INSECURE_COOKIES is set — cookies will not carry Secure");
        AppState::new(store, telemetry).allowing_insecure_cookies()
    };

    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|e| format!("cannot bind {}: {e}", config.bind))?;
    // Not config.bind: with a port of 0 the kernel chose one, and the chosen one is
    // what someone needs to connect to.
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    println!("listening on http://{addr}");

    // with_connect_info, so the audit layer can fall back to the socket's peer address
    // when there is no X-Forwarded-For. Without it a directly exposed server records no
    // client address at all, and the audit log's ip column is uniformly empty.
    let service = router(state).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, service)
        .with_graceful_shutdown(shutdown::signal())
        .await
        .map_err(|e| format!("server stopped: {e}"))?;

    println!("stopped cleanly");
    Ok(())
}
