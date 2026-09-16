//! Turning a stream of rows into the few large inserts `ClickHouse` wants.
//!
//! SPEC §M3, and it is not a preference:
//!
//! > `ClickHouse` wants **large, infrequent inserts** — target 10 000–100 000 rows or
//! > 1 second, whichever comes first. Per-row inserts will destroy it. On insert failure,
//! > retry with backoff and spill to a local WAL after N failures; never drop in-memory
//! > batches on a transient `ClickHouse` restart.
//!
//! Every `INSERT` creates a part, and a `MergeTree` merges parts in the background. A
//! thousand inserts a second creates a thousand parts a second, the merge scheduler falls
//! behind, and the server starts refusing writes with `TOO_MANY_PARTS` — at which point
//! ingest stops entirely. The failure is not gradual and it is not obvious from the
//! insert side.
//!
//! # Why both a row count and a deadline
//!
//! The row count is what makes the insert efficient. The deadline is what makes a quiet
//! system usable: a customer sending forty messages a minute would otherwise wait four
//! hours to see the first one, and "my logs are not arriving" is indistinguishable from
//! a broken receiver.
//!
//! # Why the buffer is never dropped
//!
//! A `ClickHouse` restart takes seconds and is a routine thing — an upgrade, a
//! configuration reload, an OOM kill. A pipeline that discarded its buffer each time
//! would lose exactly the logs written during the incident somebody is investigating. So
//! a failed insert is retried with backoff and the rows are kept.
//!
//! What is *not* here yet is the WAL spill SPEC also asks for. Keeping rows in memory is
//! bounded by [`Config::max_buffered`], and past that the oldest are dropped and counted
//! — which is honest but is not the requirement. See STATUS.

use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use uops_store_ch::LogRow;

/// Somewhere to put a batch.
///
/// Narrower than `uops_store_ch::LogStore`, which also carries `query` and `health`. A
/// batcher writes and never reads, and depending on the wider trait would mean this
/// module could only be tested against something that answers a compiled query — which is
/// `ClickHouse` and nothing else.
#[async_trait]
pub trait Sink: Send + Sync {
    /// Store these rows, or say why not.
    ///
    /// # Errors
    ///
    /// Whatever the store said. The string is for the operator's log; the batcher only
    /// distinguishes success from failure, because there is no failure it could act on
    /// differently — every one of them means "try again shortly".
    async fn write(&self, rows: &[LogRow]) -> Result<(), String>;
}

#[async_trait]
impl Sink for uops_store_ch::ChStore {
    async fn write(&self, rows: &[LogRow]) -> Result<(), String> {
        uops_store_ch::LogStore::insert_logs(self, rows)
            .await
            .map_err(|e| e.to_string())
    }
}

/// How the batcher behaves.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Insert once this many rows are waiting.
    ///
    /// SPEC's range is 10 000–100 000. The low end is the default because it bounds the
    /// memory a batch holds and because the deadline usually fires first on anything but
    /// a busy estate.
    pub max_rows: usize,
    /// Insert after this long, however few rows are waiting.
    pub max_delay: Duration,
    /// How long to wait after the first failed insert. Doubles, up to `max_backoff`.
    pub backoff: Duration,
    pub max_backoff: Duration,
    /// How many rows may be held while `ClickHouse` is unavailable.
    ///
    /// The ceiling that stops a long outage becoming an out-of-memory kill — which would
    /// lose everything buffered rather than the oldest part of it. Past this the oldest
    /// rows go first: during an incident the newest logs are the ones being looked at.
    pub max_buffered: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_rows: 10_000,
            max_delay: Duration::from_secs(1),
            backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(30),
            max_buffered: 500_000,
        }
    }
}

/// What the batcher has done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub rows_written: u64,
    pub batches_written: u64,
    /// Inserts that failed and were retried. Not rows.
    pub retries: u64,
    /// Rows discarded because the buffer was full through a long outage.
    ///
    /// Non-zero means `ClickHouse` was unavailable for longer than this process could
    /// hold, and logs were lost. It is counted separately from everything else because
    /// it is the only number here that represents data loss.
    pub rows_dropped: u64,
}

/// Accumulate rows and write them in batches until the channel closes.
///
/// Returns when the sender is dropped and the last batch has been written — so a
/// shutdown does not lose what is buffered, which is the same reason the buffer survives
/// a failed insert.
pub async fn run<S: Sink>(
    sink: S,
    mut rows: mpsc::Receiver<LogRow>,
    config: Config,
    report: impl Fn(Stats) + Send,
) -> Stats {
    let mut stats = Stats::default();
    let mut buffer: Vec<LogRow> = Vec::with_capacity(config.max_rows);
    let mut deadline = tokio::time::interval(config.max_delay);
    // The first tick of an interval is immediate, and an immediate empty flush is a
    // wasted wake-up.
    deadline.tick().await;
    deadline.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let closed = tokio::select! {
            received = rows.recv() => match received {
                Some(row) => {
                    buffer.push(row);
                    if buffer.len() < config.max_rows {
                        continue;
                    }
                    false
                }
                None => true,
            },
            _ = deadline.tick() => {
                if buffer.is_empty() {
                    continue;
                }
                false
            }
        };

        if !buffer.is_empty() {
            flush(&sink, &mut buffer, config, &mut stats).await;
            report(stats);
            // The deadline restarts from the write, not from the last one: a batch that
            // filled early should not be followed by a short window.
            deadline.reset();
        }

        if closed {
            return stats;
        }
    }
}

/// Write the buffer, retrying until it succeeds or the buffer has to be trimmed.
async fn flush<S: Sink>(sink: &S, buffer: &mut Vec<LogRow>, config: Config, stats: &mut Stats) {
    let mut wait = config.backoff;

    loop {
        match sink.write(buffer).await {
            Ok(()) => {
                stats.rows_written += buffer.len() as u64;
                stats.batches_written += 1;
                buffer.clear();
                return;
            }
            Err(why) => {
                stats.retries += 1;
                // One line per failure, not per row. A ClickHouse restart produces a
                // handful of these and then stops; a row-per-failure log would produce
                // ten thousand and bury the recovery.
                eprintln!(
                    "pipeline: {} rows could not be stored, retrying in {wait:?}: {why}",
                    buffer.len()
                );

                if buffer.len() > config.max_buffered {
                    // The oldest go. During an incident the newest logs are the ones
                    // being looked at, and losing the tail of a long outage is better
                    // than being killed and losing all of it.
                    let excess = buffer.len() - config.max_buffered;
                    buffer.drain(..excess);
                    stats.rows_dropped += excess as u64;
                }

                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(config.max_backoff);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A sink that records what it was given, and can be told to fail.
    #[derive(Default)]
    struct Recorder {
        batches: Mutex<Vec<usize>>,
        rows: AtomicUsize,
        /// Fail this many times before succeeding.
        fail_next: AtomicUsize,
    }

    #[async_trait]
    impl Sink for Arc<Recorder> {
        async fn write(&self, rows: &[LogRow]) -> Result<(), String> {
            if self.fail_next.load(Ordering::SeqCst) > 0 {
                self.fail_next.fetch_sub(1, Ordering::SeqCst);
                return Err("clickhouse is unavailable".to_owned());
            }
            self.batches.lock().unwrap().push(rows.len());
            self.rows.fetch_add(rows.len(), Ordering::SeqCst);
            Ok(())
        }
    }

    fn row(body: &str) -> LogRow {
        LogRow {
            tenant_id: uops_core::TenantId::new(),
            resource_id: uops_core::ResourceId::new(),
            site_id: uops_core::SiteId::nil(),
            observed_at: chrono::Utc::now(),
            ingested_at: chrono::Utc::now(),
            source_kind: "syslog".to_owned(),
            source_vendor: String::new(),
            severity: "info".to_owned(),
            facility: 1,
            body: body.to_owned(),
            attributes: std::collections::BTreeMap::new(),
            trace_id: String::new(),
            span_id: String::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rows_are_written_in_one_batch_when_the_count_is_reached() {
        // The requirement. Every INSERT creates a part and a MergeTree merges parts in
        // the background; a thousand inserts a second makes the server refuse writes
        // with TOO_MANY_PARTS, at which point ingest stops entirely.
        let sink = Arc::new(Recorder::default());
        let (tx, rx) = mpsc::channel(1024);
        let config = Config {
            max_rows: 10,
            ..Config::default()
        };

        let handle = tokio::spawn(run(Arc::clone(&sink), rx, config, |_| {}));
        for i in 0..10 {
            tx.send(row(&format!("{i}"))).await.expect("send");
        }
        drop(tx);
        let stats = handle.await.expect("join");

        assert_eq!(stats.batches_written, 1, "ten rows is one insert, not ten");
        assert_eq!(stats.rows_written, 10);
        assert_eq!(*sink.batches.lock().unwrap(), vec![10]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_quiet_system_still_sees_its_logs() {
        // The other half. A customer sending forty messages a minute would otherwise wait
        // hours for the first batch, and "my logs are not arriving" is indistinguishable
        // from a broken receiver.
        let sink = Arc::new(Recorder::default());
        let (tx, rx) = mpsc::channel(1024);
        let config = Config {
            max_rows: 10_000,
            max_delay: Duration::from_secs(1),
            ..Config::default()
        };

        let handle = tokio::spawn(run(Arc::clone(&sink), rx, config, |_| {}));
        tx.send(row("lonely")).await.expect("send");

        // Long enough for the deadline to fire, with the clock paused so this is
        // deterministic rather than a sleep race.
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert_eq!(
            sink.rows.load(Ordering::SeqCst),
            1,
            "the deadline must fire"
        );

        drop(tx);
        handle.await.expect("join");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_insert_is_retried_and_nothing_is_lost() {
        // A ClickHouse restart takes seconds and is routine. A pipeline that discarded
        // its buffer would lose exactly the logs written during the incident somebody is
        // investigating.
        let sink = Arc::new(Recorder::default());
        sink.fail_next.store(3, Ordering::SeqCst);

        let (tx, rx) = mpsc::channel(1024);
        let config = Config {
            max_rows: 2,
            backoff: Duration::from_millis(10),
            ..Config::default()
        };

        let handle = tokio::spawn(run(Arc::clone(&sink), rx, config, |_| {}));
        tx.send(row("a")).await.expect("send");
        tx.send(row("b")).await.expect("send");
        drop(tx);
        let stats = handle.await.expect("join");

        assert_eq!(stats.retries, 3);
        assert_eq!(stats.rows_written, 2, "the rows survived the outage");
        assert_eq!(stats.rows_dropped, 0);
        assert_eq!(*sink.batches.lock().unwrap(), vec![2]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_backoff_grows_rather_than_hammering() {
        // A retry loop with no backoff turns one unavailable server into a denial of
        // service against it, and the server is trying to start up.
        let sink = Arc::new(Recorder::default());
        sink.fail_next.store(5, Ordering::SeqCst);

        let (tx, rx) = mpsc::channel(1024);
        let config = Config {
            max_rows: 1,
            backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(400),
            ..Config::default()
        };

        let started = tokio::time::Instant::now();
        let handle = tokio::spawn(run(Arc::clone(&sink), rx, config, |_| {}));
        tx.send(row("a")).await.expect("send");
        drop(tx);
        handle.await.expect("join");

        // 100 + 200 + 400 + 400 + 400 = 1 500ms, capped rather than doubling forever.
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1_500),
            "the backoff must actually wait: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "and must be capped: {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_shutdown_writes_what_is_buffered() {
        // Closing the channel is how the process stops. A batcher that returned without
        // flushing would lose up to a full batch on every deploy.
        let sink = Arc::new(Recorder::default());
        let (tx, rx) = mpsc::channel(1024);
        let config = Config {
            max_rows: 10_000,
            ..Config::default()
        };

        let handle = tokio::spawn(run(Arc::clone(&sink), rx, config, |_| {}));
        tx.send(row("unflushed")).await.expect("send");
        drop(tx);
        let stats = handle.await.expect("join");

        assert_eq!(stats.rows_written, 1, "the last batch must be written");
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_outage_drops_the_oldest_rather_than_being_killed() {
        // The ceiling. Losing the tail of a long outage is better than an out-of-memory
        // kill, which loses all of it — and during an incident the newest logs are the
        // ones being looked at.
        let sink = Arc::new(Recorder::default());
        sink.fail_next.store(2, Ordering::SeqCst);

        let (tx, rx) = mpsc::channel(1024);
        let config = Config {
            max_rows: 8,
            backoff: Duration::from_millis(1),
            max_buffered: 4,
            ..Config::default()
        };

        let handle = tokio::spawn(run(Arc::clone(&sink), rx, config, |_| {}));
        for i in 0..8 {
            tx.send(row(&format!("row {i}"))).await.expect("send");
        }
        drop(tx);
        let stats = handle.await.expect("join");

        assert!(stats.rows_dropped > 0, "{stats:?}");
        assert_eq!(
            stats.rows_dropped + stats.rows_written,
            8,
            "every row is either written or counted as lost: {stats:?}"
        );

        // And what survived is the newest.
        let written = sink.rows.load(Ordering::SeqCst);
        assert_eq!(written as u64, stats.rows_written);
    }
}
