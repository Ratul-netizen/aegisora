//! The order things happen in, and who waits for whom.
//!
//! ```text
//!   UdpReceiver ─┐
//!                ├─► mpsc<Received> ─► fan-out ─┬─► worker ─┐
//!   TcpReceiver ─┘                              ├─► worker ─┼─► mpsc<LogRow> ─► batch ─► ClickHouse
//!                                               └─► worker ─┘
//!   (one pair per tenant)                       (per listener)      (one, shared)
//! ```
//!
//! # Why the workers fan out and the batcher does not
//!
//! A worker's job is resolve → normalize, and resolution is a cache hit almost always —
//! a mutex and an LRU lookup, fast enough that one worker would do. The exception is what
//! matters: a **miss** awaits `PostgreSQL`, and with a single worker one slow lookup
//! stalls every message behind it. So each listener fans out to several.
//!
//! The batcher is the opposite. Its whole purpose is to make inserts *few and large*, and
//! a second batcher would halve the size of every insert while doubling the part count —
//! which is the failure mode `uops_pipeline::batch` exists to prevent. One batcher, shared
//! by every tenant, because a `LogRow` carries its own `tenant_id` and `ClickHouse` sorts
//! by it.
//!
//! # Where backpressure comes from, and where it stops
//!
//! Every channel here is bounded, and each hop uses `send` — which waits — rather than
//! `try_send`. So a slow `ClickHouse` slows the batcher, which fills the row channel,
//! which slows the workers, which fills the received channel, which stops the TCP receiver
//! reading, which shrinks the receive window, which slows the sender. The chain is
//! deliberate and it is the whole design.
//!
//! It stops at UDP, because UDP has no back channel. The receiver drops and counts, which
//! is `uops_syslog::receiver`'s decision and the reason it uses `try_send`: a drop in
//! userspace is a number somebody can see, and a drop in the kernel is not.
//!
//! # Shutdown
//!
//! In order, and the order is the point: receivers stop first, then the channels close as
//! each stage drains, and the batcher writes what it is holding before it returns. A
//! shutdown that dropped the buffer would lose up to a full batch on every deploy.

use std::sync::Arc;

use tokio::sync::mpsc;
use uops_core::TenantId;
use uops_identity::Resolver;
use uops_pipeline::{Attribution, Enrichment, Pipeline, batch};
use uops_store_ch::{ChStore, LogRow};
use uops_store_pg::{PgEnricher, PgStore};
use uops_syslog::receiver::{Received, TcpReceiver, UdpReceiver};

use crate::config::{Config, Listener};

/// A listener, resolved against the database.
#[derive(Clone, Debug)]
pub struct Bound {
    pub tenant_id: TenantId,
    pub listener: Listener,
}

/// Turn the configured slugs into tenant ids, failing on the first one that is not there.
///
/// At startup, before a socket is opened. A daemon that bound its ports and *then*
/// discovered a slug was mistyped would be accepting messages it had nowhere to put, and
/// the receivers would be counting drops that were really a configuration error.
///
/// # Errors
///
/// Names the slug. A mistyped tenant is the likeliest mistake in the file and the one
/// with the worst silent failure, which is why the file takes slugs at all: a mistyped
/// uuid would be a plausible-looking id that simply never matches.
pub async fn resolve_tenants(store: &PgStore, config: &Config) -> Result<Vec<Bound>, String> {
    let mut bound = Vec::with_capacity(config.listeners.len());
    for listener in &config.listeners {
        let tenant_id = store
            .tenant_by_slug(&listener.tenant)
            .await
            .map_err(|e| format!("cannot look up tenant {:?}: {e}", listener.tenant))?
            .ok_or_else(|| {
                format!(
                    "the listener file names tenant {:?}, which does not exist",
                    listener.tenant
                )
            })?;
        bound.push(Bound {
            tenant_id,
            listener: listener.clone(),
        });
    }
    Ok(bound)
}

/// Everything the workers share.
pub struct Ingest {
    pipeline: Arc<Pipeline<PgStore, PgEnricher>>,
    rows: mpsc::Sender<LogRow>,
}

impl std::fmt::Debug for Ingest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ingest")
            .field("pipeline", &self.pipeline)
            .finish_non_exhaustive()
    }
}

impl Ingest {
    /// One message, from the wire to a row in the queue.
    ///
    /// Returns `false` only when the row channel has closed, which means the batcher has
    /// stopped and there is nowhere left to put anything.
    async fn ingest(&self, bound: &Bound, received: &Received, vendor: &str) -> bool {
        let observed = uops_core::ObservedIdentity {
            identifiers: uops_syslog::normalize::identifiers(received),
            source: uops_syslog::normalize::SOURCE_KIND.to_owned(),
            site_hint: None,
        };

        let mut attribution: Attribution =
            self.pipeline.attribute(bound.tenant_id, &observed).await;

        // The listener's vendor is a fallback, not an override. What the resource says
        // about itself wins, because a per-listener guess would disagree with the
        // poller's own discovery for the same device and the two would alternate row by
        // row.
        if attribution.vendor.is_empty() {
            attribution.vendor = vendor.to_owned();
        }

        let row = uops_syslog::normalize::to_row(received, &attribution);
        self.rows.send(row).await.is_ok()
    }
}

/// Run until told to stop, then drain.
///
/// # Errors
///
/// A socket that cannot be bound, named — which is almost always a port under 1024 in a
/// container with no capability to bind it, and is worth saying plainly rather than as an
/// errno.
pub async fn serve(
    store: PgStore,
    telemetry: ChStore,
    config: &Config,
    bound: Vec<Bound>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    let pipeline = Arc::new(Pipeline::new(
        Resolver::new(store.clone()),
        Enrichment::new(PgEnricher::new(store)),
    ));

    // Opened before anything is bound, so an unwritable spill directory is a startup
    // error rather than a discovery made during the outage it exists for.
    let wal = match &config.spill {
        Some(directory) => Some(
            uops_pipeline::Wal::open(uops_pipeline::WalConfig {
                directory: directory.clone(),
                ..uops_pipeline::WalConfig::default()
            })
            .map_err(|e| {
                format!(
                    "the spill directory {} is unusable: {e}",
                    directory.display()
                )
            })?,
        ),
        None => None,
    };

    // One batcher. See the module docs: a second would halve every insert.
    let (rows_tx, rows_rx) = mpsc::channel::<LogRow>(config.queue);
    let batcher = tokio::spawn(batch::run_with_wal(
        telemetry,
        rows_rx,
        batch::Config::default(),
        wal,
        |stats| {
            // Only when something was lost. A line per batch at 50 000 msg/s is a log
            // nobody reads; a line when rows were dropped is the one somebody needs.
            if stats.rows_dropped > 0 {
                eprintln!(
                    "uops-collector-syslog: {} row(s) lost to a ClickHouse outage \
                     ({} written, {} retries)",
                    stats.rows_dropped, stats.rows_written, stats.retries
                );
            }
        },
    ));

    // A broadcast so every receiver stops on the one signal. `Notify` would wake one
    // waiter; this wakes all of them, which is what a shutdown means.
    let (stop_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    let mut receivers = Vec::new();
    let mut workers = Vec::new();

    for bound in bound {
        let ingest = Arc::new(Ingest {
            pipeline: Arc::clone(&pipeline),
            rows: rows_tx.clone(),
        });

        // Per listener, so one tenant's burst does not consume another's queue.
        let (received_tx, received_rx) = mpsc::channel::<Received>(config.queue);

        receivers.extend(bind_receivers(&bound, &received_tx, &stop_tx).await?);

        // Dropped so that the channel closes once the receivers are done, which is what
        // ends the fan-out, which is what ends the workers. Forgetting this is a
        // shutdown that hangs forever holding a sender nobody will ever use.
        drop(received_tx);

        workers.push(tokio::spawn(fan_out(
            bound,
            received_rx,
            ingest,
            config.workers,
        )));
    }

    // Same reason: the batcher returns when the last row sender is gone.
    drop(rows_tx);

    shutdown.await;
    println!("uops-collector-syslog: stopping, draining what is in flight");
    // Ignored: an error here means every receiver has already stopped, which is what was
    // being asked for.
    let _ = stop_tx.send(());

    for receiver in receivers {
        let _ = receiver.await;
    }
    for worker in workers {
        let _ = worker.await;
    }
    match batcher.await {
        Ok(stats) => {
            println!(
                "uops-collector-syslog: {} row(s) in {} insert(s), {} retries,                  {} spilled, {} replayed, {} still on disk, {} lost",
                stats.rows_written,
                stats.batches_written,
                stats.retries,
                stats.rows_spilled,
                stats.rows_replayed,
                stats.rows_pending,
                stats.rows_dropped
            );
            if stats.rows_pending > 0 {
                // Not loss, and worth distinguishing: the next start replays them. An
                // operator reading "still on disk" should not go looking for a backup.
                println!(
                    "uops-collector-syslog: {} row(s) are still spilled and will be                      replayed on the next start",
                    stats.rows_pending
                );
            }
        }
        Err(e) => return Err(format!("the batcher did not stop cleanly: {e}")),
    }
    Ok(())
}

/// Bind whatever this listener asked for, and spawn a task per socket.
///
/// Split out of `serve` so that the shutdown ordering there stays readable as a list —
/// which is the part that is easy to get wrong, and the part a reader needs to check.
async fn bind_receivers(
    bound: &Bound,
    received_tx: &mpsc::Sender<Received>,
    stop_tx: &tokio::sync::broadcast::Sender<()>,
) -> Result<Vec<tokio::task::JoinHandle<()>>, String> {
    let mut spawned = Vec::new();

    if let Some(address) = bound.listener.udp {
        let receiver = UdpReceiver::bind(address, uops_syslog::receiver::Config::default())
            .map_err(|e| bind_error("udp", address, &e))?;
        let granted = receiver.receive_buffer();
        let stats = receiver.stats();
        println!(
            "uops-collector-syslog: {} udp {address}, {granted} byte receive buffer",
            bound.listener.tenant
        );
        let sink = received_tx.clone();
        let mut stop = stop_tx.subscribe();
        let tenant = bound.listener.tenant.clone();
        spawned.push(tokio::spawn(async move {
            receiver
                .run(sink, async move {
                    let _ = stop.recv().await;
                })
                .await;
            // The drop count is the number SPEC cares about, and it is worth saying once
            // at shutdown even when it is zero — "0 dropped" is evidence, and an absent
            // line is not.
            println!(
                "uops-collector-syslog: {tenant} udp stopped, {} received, {} dropped",
                stats.received(),
                stats.dropped()
            );
        }));
    }

    if let Some(address) = bound.listener.tcp {
        let receiver = TcpReceiver::bind(address)
            .await
            .map_err(|e| bind_error("tcp", address, &e))?;
        let stats = receiver.stats();
        println!(
            "uops-collector-syslog: {} tcp {address}",
            bound.listener.tenant
        );
        let sink = received_tx.clone();
        let mut stop = stop_tx.subscribe();
        let tenant = bound.listener.tenant.clone();
        spawned.push(tokio::spawn(async move {
            receiver
                .run(sink, async move {
                    let _ = stop.recv().await;
                })
                .await;
            println!(
                "uops-collector-syslog: {tenant} tcp stopped, {} received",
                stats.received()
            );
        }));
    }

    Ok(spawned)
}

/// Read one listener's messages and hand them to N workers in turn.
///
/// Round-robin rather than "whoever is free", because knowing who is free needs a
/// multi-consumer channel and the difference only shows up when one worker is blocked —
/// at which point its queue fills, `send` waits, and the fan-out stalls for as long as
/// that worker is stuck. That is a real limitation and it is bounded: with `workers`
/// queues of `queue / workers` each, one slow lookup delays a fraction of the stream
/// rather than all of it.
async fn fan_out(
    bound: Bound,
    mut received: mpsc::Receiver<Received>,
    ingest: Arc<Ingest>,
    workers: usize,
) {
    let workers = workers.max(1);
    let depth = 1.max(1024 / workers);

    let mut senders = Vec::with_capacity(workers);
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let (tx, mut rx) = mpsc::channel::<Received>(depth);
        senders.push(tx);
        let ingest = Arc::clone(&ingest);
        let bound = bound.clone();
        handles.push(tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if !ingest
                    .ingest(&bound, &message, &bound.listener.vendor)
                    .await
                {
                    // The batcher has gone. Nothing this task does from here can reach
                    // storage, so stopping is the honest thing — and the messages still
                    // in the channel are lost either way.
                    break;
                }
            }
        }));
    }

    let mut next = 0usize;
    while let Some(message) = received.recv().await {
        if senders[next].send(message).await.is_err() {
            break;
        }
        next = (next + 1) % workers;
    }

    drop(senders);
    for handle in handles {
        let _ = handle.await;
    }
}

fn bind_error(transport: &str, address: std::net::SocketAddr, e: &std::io::Error) -> String {
    if address.port() < 1024 && e.kind() == std::io::ErrorKind::PermissionDenied {
        // The overwhelmingly common failure, and an errno alone sends an operator to
        // look at the wrong thing.
        return format!(
            "cannot bind {transport} {address}: ports below 1024 need CAP_NET_BIND_SERVICE. \
             Either grant it, or bind a high port and redirect — the compose file uses \
             the second, because a container that needs a capability is one more thing \
             to get right on every host"
        );
    }
    format!("cannot bind {transport} {address}: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_privileged_port_says_what_to_do_about_it() {
        // The overwhelmingly common failure. An errno alone sends an operator to look at
        // the firewall.
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let message = bind_error("udp", "0.0.0.0:514".parse().expect("an address"), &e);
        assert!(message.contains("CAP_NET_BIND_SERVICE"), "{message}");

        // A high port that fails for the same reason is a different problem and must not
        // get the same advice.
        let message = bind_error("udp", "0.0.0.0:1514".parse().expect("an address"), &e);
        assert!(!message.contains("CAP_NET_BIND_SERVICE"), "{message}");
    }

    #[test]
    fn an_address_already_in_use_is_reported_as_itself() {
        let e = std::io::Error::new(std::io::ErrorKind::AddrInUse, "address in use");
        let message = bind_error("tcp", "0.0.0.0:601".parse().expect("an address"), &e);
        assert!(message.contains("address in use"), "{message}");
        assert!(!message.contains("CAP_NET_BIND_SERVICE"), "{message}");
    }
}
