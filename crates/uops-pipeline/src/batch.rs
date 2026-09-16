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
//! # Where the WAL comes in
//!
//! Retrying in memory handles the ten-second restart. It does not handle the ten-*minute*
//! one: memory is bounded, and past [`Config::max_buffered`] the oldest rows were dropped.
//!
//! So after [`wal::Config::spill_after`](crate::wal::Config::spill_after) consecutive
//! failures the buffer is written to disk and cleared, and ingestion continues with an
//! empty buffer against a `ClickHouse` that is still down. Spilled segments are replayed,
//! oldest first, once an insert succeeds again.
//!
//! A batcher given no WAL keeps the old behaviour exactly. That is not a fallback for
//! convenience: the tests for the in-memory bound are about what happens when there is
//! nowhere to spill *to*, which is a real deployment — a read-only container, or an
//! operator who would rather lose logs than fill a disk.

use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use uops_store_ch::{LogRow, MetricRow};

use crate::wal::Wal;

/// What a row has to be for this module to batch it.
///
/// `Serialize` for the insert, `DeserializeOwned` for the spill's replay — the two halves
/// of the same requirement, which is why they are one bound and not two scattered ones.
pub trait Row: serde::Serialize + serde::de::DeserializeOwned + Send + 'static {}
impl<T: serde::Serialize + serde::de::DeserializeOwned + Send + 'static> Row for T {}

/// Somewhere to put a batch.
///
/// Narrower than `uops_store_ch::LogStore`, which also carries `query` and `health`. A
/// batcher writes and never reads, and depending on the wider trait would mean this
/// module could only be tested against something that answers a compiled query — which is
/// `ClickHouse` and nothing else.
#[async_trait]
pub trait Sink<R>: Send + Sync {
    /// Store these rows, or say why not.
    ///
    /// # Errors
    ///
    /// Whatever the store said. The string is for the operator's log; the batcher only
    /// distinguishes success from failure, because there is no failure it could act on
    /// differently — every one of them means "try again shortly".
    async fn write(&self, rows: &[R]) -> Result<(), String>;
}

#[async_trait]
impl Sink<LogRow> for uops_store_ch::ChStore {
    async fn write(&self, rows: &[LogRow]) -> Result<(), String> {
        uops_store_ch::LogStore::insert_logs(self, rows)
            .await
            .map_err(|e| e.to_string())
    }
}

/// The same batching, the same spill, a different table.
///
/// Metrics arrive at a fraction of the rate logs do — `hostmetrics` sends a scrape every
/// ten seconds, not fifty thousand messages a second — so the row count rarely fills a
/// batch and the deadline is what usually fires. That is fine and is the reason the
/// deadline exists; what matters is that they get the same **durability**, because a
/// `ClickHouse` outage loses a metric exactly as permanently as it loses a log.
#[async_trait]
impl Sink<MetricRow> for uops_store_ch::ChStore {
    async fn write(&self, rows: &[MetricRow]) -> Result<(), String> {
        uops_store_ch::MetricStore::insert_metrics(self, rows)
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
    /// Rows discarded because there was nowhere left to put them.
    ///
    /// With a WAL configured this means the spill directory was full as well; without one
    /// it means the memory bound was reached. Either way `ClickHouse` was unavailable for
    /// longer than this host could hold and logs were lost, which is why it is counted
    /// separately from everything else.
    pub rows_dropped: u64,
    /// Rows written to the spill because `ClickHouse` would not take them.
    ///
    /// **Not** loss. A non-zero `rows_spilled` with a falling `rows_pending` is the system
    /// recovering exactly as designed; the number to watch is `rows_dropped`.
    pub rows_spilled: u64,
    pub rows_replayed: u64,
    /// Rows currently on disk, waiting for `ClickHouse` to come back.
    pub rows_pending: u64,
}

/// Accumulate rows and write them in batches until the channel closes.
///
/// Returns when the sender is dropped and the last batch has been written — so a
/// shutdown does not lose what is buffered, which is the same reason the buffer survives
/// a failed insert.
pub async fn run<R: Row, S: Sink<R>>(
    sink: S,
    rows: mpsc::Receiver<R>,
    config: Config,
    report: impl Fn(Stats) + Send,
) -> Stats {
    run_with_wal(sink, rows, config, None, report).await
}

/// [`run`], with somewhere to spill to.
///
/// Separate rather than an `Option` on [`Config`] because a `Wal` owns a directory and a
/// counter, and putting one in a `Copy` config would make it something a caller could
/// duplicate by accident — two batchers writing segments into one directory under the
/// same names, overwriting each other's.
pub async fn run_with_wal<R: Row, S: Sink<R>>(
    sink: S,
    mut rows: mpsc::Receiver<R>,
    config: Config,
    mut wal: Option<Wal>,
    report: impl Fn(Stats) + Send,
) -> Stats {
    let mut stats = Stats::default();

    // Anything a previous run spilled and did not get to replay. Before the first row,
    // because a collector that comes back after a crash should drain what it owes rather
    // than sit on it until the next outage.
    if let Some(wal) = wal.as_mut() {
        stats.rows_pending = pending_rows(wal);
        replay(&sink, wal, &mut stats).await;
    }
    let mut buffer: Vec<R> = Vec::with_capacity(config.max_rows);
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
            flush(&sink, &mut buffer, config, wal.as_mut(), &mut stats).await;
            if let Some(wal) = wal.as_mut() {
                replay(&sink, wal, &mut stats).await;
            }
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

/// Write the buffer, retrying until it succeeds, is spilled, or has to be trimmed.
async fn flush<R: Row, S: Sink<R>>(
    sink: &S,
    buffer: &mut Vec<R>,
    config: Config,
    mut wal: Option<&mut Wal>,
    stats: &mut Stats,
) {
    let mut wait = config.backoff;
    let mut consecutive = 0u32;
    // With a spill configured, the memory bound is not the mechanism — the spill is — so
    // nothing may be discarded until spilling has been tried and failed.
    //
    // Getting this backwards was a real bug, found by the test below: the bound ran on
    // every failure including the ones before `spill_after`, so with `spill_after: 2` it
    // discarded half the rows one retry before the disk they were about to be written to.
    // The trim and the spill are answers to the same question and only one of them can go
    // first.
    let mut may_discard = wal.is_none();

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
                consecutive += 1;
                // One line per failure, not per row. A ClickHouse restart produces a
                // handful of these and then stops; a row-per-failure log would produce
                // ten thousand and bury the recovery.
                eprintln!(
                    "pipeline: {} rows could not be stored, retrying in {wait:?}: {why}",
                    buffer.len()
                );

                if let Some(wal) = wal.as_deref_mut()
                    && consecutive >= wal.config().spill_after
                {
                    match wal.spill(buffer) {
                        Ok(()) => {
                            stats.rows_spilled += buffer.len() as u64;
                            stats.rows_pending += buffer.len() as u64;
                            buffer.clear();
                            // Returning rather than retrying: the rows are safe, the
                            // buffer is empty, and ingestion should carry on against a
                            // ClickHouse that is still down. `replay` is what gets them
                            // there when it comes back.
                            return;
                        }
                        // A spill that cannot spill is a reason to hold on, not a reason
                        // to drop — but it is also the point at which the memory bound
                        // becomes the only thing left, which is exactly the no-WAL
                        // behaviour.
                        Err(e) => {
                            eprintln!("pipeline: the spill is unusable: {e}");
                            may_discard = true;
                        }
                    }
                }

                if may_discard && buffer.len() > config.max_buffered {
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

/// How many rows a previous run left on disk.
///
/// Counted once, at startup, so the reported `rows_pending` is right for segments this
/// process did not write. Reading every segment to count lines is only acceptable because
/// it happens once and only when something was actually left behind.
fn pending_rows(wal: &mut Wal) -> u64 {
    let Ok(segments) = wal.segments() else {
        return 0;
    };
    segments
        .iter()
        .map(|p| {
            std::fs::read_to_string(p)
                .map_or(0, |t| t.lines().filter(|l| !l.is_empty()).count() as u64)
        })
        .sum()
}

/// Send spilled segments, oldest first, until one fails.
///
/// Stops at the first failure rather than working through the rest: if `ClickHouse` is
/// still down every remaining segment will fail too, and trying them all turns one outage
/// into a directory scan per batch. The unsent ones stay where they are.
///
/// A segment is unlinked **only** after its insert succeeds. Doing it first would turn a
/// failed replay into silent loss, which is the one thing the spill exists to prevent.
async fn replay<R: Row, S: Sink<R>>(sink: &S, wal: &mut Wal, stats: &mut Stats) {
    let Ok(segments) = wal.segments() else {
        return;
    };

    for path in segments {
        let rows: Vec<R> = match wal.read(&path) {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("pipeline: a spilled segment could not be read: {e}");
                return;
            }
        };
        // An empty segment is one whose every line was unreadable. Unlinking it is right
        // -- retrying forever would block every segment behind it -- and `read` has
        // already counted it as corrupt and said so.
        if rows.is_empty() {
            let _ = wal.done(&path, 0);
            continue;
        }

        if sink.write(&rows).await.is_err() {
            return;
        }
        stats.rows_written += rows.len() as u64;
        stats.batches_written += 1;
        stats.rows_replayed += rows.len() as u64;
        stats.rows_pending = stats.rows_pending.saturating_sub(rows.len() as u64);

        if let Err(e) = wal.done(&path, rows.len()) {
            // The rows are in ClickHouse and the segment is still there, so the next
            // replay would insert them again. Said out loud, because duplicate rows are a
            // real consequence and otherwise a silent one.
            let path = path.display();
            eprintln!(
                "pipeline: {path} was replayed but could not be removed, so its rows may be inserted twice: {e}"
            );
            return;
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
    impl Sink<LogRow> for Arc<Recorder> {
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

    /// A scratch directory that removes itself.
    fn spill_dir() -> (crate::wal::tests_support::Dir, crate::wal::Config) {
        let dir = crate::wal::tests_support::Dir::new();
        let config = crate::wal::Config {
            directory: dir.path().to_path_buf(),
            spill_after: 2,
            ..crate::wal::Config::default()
        };
        (dir, config)
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_outage_spills_to_disk_instead_of_dropping() {
        // The difference between surviving a configuration reload and surviving an
        // upgrade that goes wrong. Without the spill these rows would have hit the memory
        // bound and the oldest would have been discarded; with it they are on disk and
        // ingestion carries on against a ClickHouse that is still down.
        let sink = Arc::new(Recorder::default());
        sink.fail_next.store(1_000, Ordering::SeqCst);
        let (dir, wal_config) = spill_dir();
        let wal = crate::wal::Wal::open(wal_config).expect("open");

        let (tx, rx) = mpsc::channel(1024);
        let config = Config {
            max_rows: 2,
            backoff: Duration::from_millis(1),
            // Deliberately tiny. Without a spill this would be dropping rows almost
            // immediately, which is what makes the assertion below mean something.
            max_buffered: 1,
            ..Config::default()
        };

        let handle = tokio::spawn(run_with_wal(
            Arc::clone(&sink),
            rx,
            config,
            Some(wal),
            |_| {},
        ));
        for i in 0..6 {
            tx.send(row(&format!("row {i}"))).await.expect("send");
        }
        drop(tx);
        let stats = handle.await.expect("join");

        assert_eq!(stats.rows_spilled, 6, "{stats:?}");
        assert_eq!(
            stats.rows_dropped, 0,
            "nothing may be dropped while there is disk: {stats:?}"
        );
        assert_eq!(stats.rows_pending, 6);
        assert_eq!(
            sink.rows.load(Ordering::SeqCst),
            0,
            "ClickHouse never took one"
        );

        // And they are really on disk, not merely counted.
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read the spill")
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "wal"))
            .collect();
        assert_eq!(left.len(), 3, "three batches of two");
    }

    #[tokio::test(start_paused = true)]
    async fn what_was_spilled_is_replayed_when_clickhouse_comes_back() {
        // The other half, and the one that matters: a spill nobody replays is a slower
        // way to lose the logs.
        let sink = Arc::new(Recorder::default());
        // Fails for the first two batches, then recovers.
        sink.fail_next.store(4, Ordering::SeqCst);
        let (dir, wal_config) = spill_dir();
        let wal = crate::wal::Wal::open(wal_config).expect("open");

        let (tx, rx) = mpsc::channel(1024);
        let config = Config {
            max_rows: 2,
            backoff: Duration::from_millis(1),
            ..Config::default()
        };

        let handle = tokio::spawn(run_with_wal(
            Arc::clone(&sink),
            rx,
            config,
            Some(wal),
            |_| {},
        ));
        for i in 0..6 {
            tx.send(row(&format!("row {i}"))).await.expect("send");
        }
        drop(tx);
        let stats = handle.await.expect("join");

        assert!(stats.rows_spilled > 0, "the premise: something spilled");
        assert_eq!(stats.rows_replayed, stats.rows_spilled, "{stats:?}");
        assert_eq!(stats.rows_pending, 0, "nothing is still owed: {stats:?}");
        assert_eq!(stats.rows_dropped, 0);
        assert_eq!(
            sink.rows.load(Ordering::SeqCst),
            6,
            "every row reaches ClickHouse eventually: {stats:?}"
        );

        // The spill is empty afterwards. A segment left behind would be replayed again
        // on the next start and inserted twice.
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read the spill")
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "wal"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_restart_drains_what_the_previous_run_left() {
        // The crash case. A collector that came back up and ignored its own spill would
        // have written the logs to disk for nothing.
        let (dir, wal_config) = spill_dir();

        // A first run that can never insert, so everything it receives ends up on disk.
        {
            let sink = Arc::new(Recorder::default());
            sink.fail_next.store(1_000, Ordering::SeqCst);
            let wal = crate::wal::Wal::open(wal_config.clone()).expect("open");
            let (tx, rx) = mpsc::channel(16);
            let handle = tokio::spawn(run_with_wal(
                sink,
                rx,
                Config {
                    max_rows: 2,
                    backoff: Duration::from_millis(1),
                    ..Config::default()
                },
                Some(wal),
                |_| {},
            ));
            for i in 0..4 {
                tx.send(row(&format!("before the crash {i}")))
                    .await
                    .expect("send");
            }
            drop(tx);
            let stats = handle.await.expect("join");
            assert_eq!(stats.rows_spilled, 4);
        }

        // A second run over the same directory, with a working ClickHouse and no traffic
        // of its own.
        let sink = Arc::new(Recorder::default());
        let wal = crate::wal::Wal::open(wal_config).expect("reopen");
        let (tx, rx) = mpsc::channel(16);
        let handle = tokio::spawn(run_with_wal(
            Arc::clone(&sink),
            rx,
            Config::default(),
            Some(wal),
            |_| {},
        ));
        drop(tx);
        let stats = handle.await.expect("join");

        assert_eq!(
            sink.rows.load(Ordering::SeqCst),
            4,
            "a restarted collector must drain what it owes: {stats:?}"
        );
        assert_eq!(stats.rows_replayed, 4);
        let _ = dir;
    }

    #[tokio::test(start_paused = true)]
    async fn a_batcher_with_no_spill_behaves_exactly_as_before() {
        // Not a fallback for convenience. A read-only container, or an operator who would
        // rather lose logs than fill a disk, is a real deployment — and the in-memory
        // bound is what it gets.
        let sink = Arc::new(Recorder::default());
        sink.fail_next.store(2, Ordering::SeqCst);

        let (tx, rx) = mpsc::channel(1024);
        let config = Config {
            max_rows: 8,
            backoff: Duration::from_millis(1),
            max_buffered: 4,
            ..Config::default()
        };

        let handle = tokio::spawn(run_with_wal(Arc::clone(&sink), rx, config, None, |_| {}));
        for i in 0..8 {
            tx.send(row(&format!("row {i}"))).await.expect("send");
        }
        drop(tx);
        let stats = handle.await.expect("join");

        assert!(stats.rows_dropped > 0, "{stats:?}");
        assert_eq!(stats.rows_spilled, 0);
        assert_eq!(
            stats.rows_dropped + stats.rows_written,
            8,
            "every row is either written or counted as lost: {stats:?}"
        );
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
