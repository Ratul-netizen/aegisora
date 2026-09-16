//! Stopping without dropping anything on the floor.
//!
//! A container runtime sends `SIGTERM` and then, some seconds later, `SIGKILL`. Between
//! those two the process decides what kind of shutdown this is.
//!
//! Without a handler, `SIGTERM` terminates immediately: every in-flight request becomes
//! a connection reset, and a client that was halfway through a write cannot tell a
//! deploy from a crash and retries — which, for a write, may not be safe. With one, the
//! listener stops accepting, the requests already running finish, and the process exits
//! having answered everything it accepted.
//!
//! `SIGINT` is handled the same way, because an operator pressing ctrl-C on a terminal
//! deserves the same treatment as an orchestrator, and because a second ctrl-C still
//! kills the process outright — the impatient path is already there and does not need
//! to be the only one.
//!
//! # Why this is a fourth copy
//!
//! `uops_server`, `uops_poller` and `uops_collector_syslog` are the other three. The
//! binaries
//! have no crate below them that is the right home for it: `uops-core` is deliberately
//! dependency-light — it builds for WASM, which is why it does not link a database
//! driver — and a tokio signal handler in it would be paid for by every consumer,
//! including the ones that have no process to signal. A dependency edge between the
//! binaries would pull axum, or the SNMP stack, in to reuse thirty lines.
//!
//! The risk of a copy is drift, so what differs is worth naming. The server drains
//! requests it accepted. The poller abandons a tick it has not finished. The syslog
//! daemon drains its receivers. **This one does both**: axum stops accepting and finishes
//! the requests in flight, and then the two batchers write what they are holding —
//! because an exporter whose request was cut mid-flight will retry, and a batch dropped
//! on shutdown will not.
//!
//! The shared part is *when*, which is the platform's and not something any of them gets
//! to decide. If that changes, all three change.

/// Resolves when the process is asked to stop.
///
/// On Unix, `SIGTERM` or `SIGINT`. On Windows there is no `SIGTERM`, so this is ctrl-C
/// alone; that is the platform's own shape and not a gap to apologise for, and a
/// developer on Windows is the only one who reaches it.
pub async fn signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            // A process that cannot listen for ctrl-C would otherwise "shut down
            // gracefully" the instant this future resolved. Never returning is the
            // honest behaviour: the operator still has SIGKILL.
            eprintln!("cannot listen for ctrl-c, shutting down on signal is disabled: {e}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    {
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(e) => {
                    eprintln!("cannot listen for SIGTERM, deploys will be ungraceful: {e}");
                    std::future::pending::<()>().await;
                }
            }
        };

        tokio::select! {
            () = ctrl_c => {}
            () = terminate => {}
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;
}
