//! The write-ahead log — where rows go when `ClickHouse` will not take them.
//!
//! SPEC §M3: *"On insert failure, retry with backoff and spill to a local WAL after N
//! failures; never drop in-memory batches on a transient `ClickHouse` restart."*
//!
//! The batcher already retried and kept its buffer, which handles the ten-second restart.
//! What it could not handle is the ten-*minute* one: memory is bounded, and past the
//! bound it dropped the oldest rows. This is the difference between surviving a
//! configuration reload and surviving an upgrade that goes wrong — and the second is when
//! somebody actually needs the logs.
//!
//! # Segments, not one file
//!
//! Each spill writes one segment and closes it. A single append-only file would mean
//! truncating the front of a file to acknowledge what has been replayed, which is not an
//! operation a filesystem offers; the alternatives are rewriting the file (O(n) per
//! batch) or tracking an offset that a crash can disagree with. A segment is replayed and
//! then unlinked, so "what is still owed" is `ls`.
//!
//! Names sort in the order they were written — a zero-padded counter and the process's
//! start time — so replay order is directory order and needs no index.
//!
//! # The format is the same bytes `ClickHouse` is sent
//!
//! JSON, one row per line. Not because it is compact — it is not — but because it is
//! exactly what `insert_logs` serialises, so a replayed segment cannot encode differently
//! from a live insert. A second encoder is a second thing to keep in step, and the way
//! that fails is that replayed rows are subtly wrong in a way nothing notices until
//! somebody queries the hour of the outage.
//!
//! # What durability this actually gives
//!
//! `fsync` once per segment, at close. **Not** per row, which would cap throughput far
//! below the 50 000 msg/s target and buy very little: the window it closes is the few
//! milliseconds between a row entering the buffer and the segment being written, and a
//! process that dies in that window has also lost whatever was in the socket buffer, the
//! channel, and the kernel's receive queue. Syslog over UDP has no delivery guarantee to
//! preserve in the first place.
//!
//! So the guarantee is: **a segment that exists on disk is complete and will be
//! replayed**, including across a crash. Not: every message that arrived is on disk.
//! Claiming the second would be a lie that an operator would plan around.
//!
//! # Bounded, like everything else
//!
//! Disk is finite and a `ClickHouse` outage can outlast it. Past [`Config::max_bytes`]
//! the **oldest segment** is unlinked and counted — the same rule the memory buffer
//! already used and for the same reason: during an incident the newest logs are the ones
//! being looked at. A spill directory that filled a disk would take the collector down,
//! and then nothing is being written at all.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use uops_store_ch::LogRow;

/// How the spill behaves.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where segments live. Created if it is not there.
    pub directory: PathBuf,
    /// How many consecutive failed inserts before the buffer is written to disk.
    ///
    /// Not one. A single failure is usually a `ClickHouse` restart that will be over
    /// before the third retry, and spilling immediately would turn every routine reload
    /// into disk traffic and a replay. Not twenty either: the point of spilling is to
    /// free the memory bound before it starts dropping rows.
    pub spill_after: u32,
    /// How much may sit on disk before the oldest segment is dropped.
    pub max_bytes: u64,
    /// How often to try replaying, once there is something to replay.
    pub replay_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("/var/lib/uops/wal"),
            spill_after: 3,
            // Two gigabytes is roughly forty minutes of the 50 000 msg/s target at a
            // realistic row size, which is longer than any ClickHouse restart and long
            // enough to cover an upgrade somebody has to think about.
            max_bytes: 2 * 1024 * 1024 * 1024,
            replay_interval: Duration::from_secs(5),
        }
    }
}

/// What the spill has done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub segments_written: u64,
    pub segments_replayed: u64,
    pub rows_spilled: u64,
    pub rows_replayed: u64,
    /// Rows lost because the spill directory was full through a long outage.
    ///
    /// The only number here that represents data loss, counted separately for that
    /// reason. Non-zero means `ClickHouse` was unavailable for longer than this host
    /// could hold.
    pub rows_dropped: u64,
    /// Segments that could not be read back and were moved aside rather than replayed.
    pub segments_corrupt: u64,
}

/// The on-disk spill.
#[derive(Debug)]
pub struct Wal {
    config: Config,
    /// Monotonic within a process. Combined with `epoch` so that a restart cannot reuse a
    /// name a previous run already used and have the two sort wrongly.
    next: u64,
    epoch: u64,
    stats: Stats,
}

/// The extension a complete segment has.
///
/// A segment is written as `.partial` and renamed when it is closed and synced, so a
/// crash mid-write leaves a file that replay knows to ignore rather than a truncated
/// segment it would read half of. The rename is the commit.
const SEGMENT: &str = "wal";
const PARTIAL: &str = "partial";

impl Wal {
    /// Open, or create, the spill directory.
    ///
    /// # Errors
    ///
    /// The directory could not be created or is not writable — which is a configuration
    /// error and should be found at startup rather than during the outage the spill
    /// exists for.
    pub fn open(config: Config) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config.directory)?;

        // Written and removed, so that a read-only mount or a wrong owner fails here
        // rather than at three in the morning. `create_dir_all` succeeds on a directory
        // that already exists and says nothing about whether it can be written to.
        let probe = config.directory.join(".writable");
        std::fs::write(&probe, b"")?;
        std::fs::remove_file(&probe)?;

        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        Ok(Self {
            config,
            next: 0,
            epoch,
            stats: Stats::default(),
        })
    }

    #[must_use]
    pub const fn stats(&self) -> Stats {
        self.stats
    }

    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// Write a batch to disk and forget it.
    ///
    /// # Errors
    ///
    /// The write failed, in which case the caller still has the rows and should keep
    /// them in memory — a spill that cannot spill is a reason to hold on, not a reason to
    /// drop.
    pub fn spill(&mut self, rows: &[LogRow]) -> std::io::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.enforce_limit()?;

        let stem = format!("{:020}-{:010}", self.epoch, self.next);
        self.next += 1;
        let partial = self.config.directory.join(format!("{stem}.{PARTIAL}"));
        let complete = self.config.directory.join(format!("{stem}.{SEGMENT}"));

        {
            let file = std::fs::File::create(&partial)?;
            let mut writer = std::io::BufWriter::new(file);
            for row in rows {
                // The same serialisation `insert_logs` uses. A row that cannot be encoded
                // could not have been inserted either, so skipping it loses nothing that
                // was ever going to arrive — and failing the whole segment for one row
                // would lose the other nine thousand.
                match serde_json::to_string(row) {
                    Ok(line) => {
                        writer.write_all(line.as_bytes())?;
                        writer.write_all(b"\n")?;
                    }
                    Err(e) => {
                        eprintln!("wal: a row could not be encoded and was not spilled: {e}");
                    }
                }
            }
            writer.flush()?;
            // Once, at close. See the module docs on what this does and does not promise.
            writer.get_ref().sync_all()?;
        }

        // The commit. A crash before this leaves a `.partial` that replay ignores; a
        // crash after it leaves a segment that is whole.
        std::fs::rename(&partial, &complete)?;

        self.stats.segments_written += 1;
        self.stats.rows_spilled += rows.len() as u64;
        Ok(())
    }

    /// Whether anything is waiting to be replayed.
    ///
    /// Reads the directory rather than trusting a counter, because segments written by a
    /// *previous* run of this process are exactly the ones that must not be forgotten.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.segments().is_ok_and(|s| !s.is_empty())
    }

    /// Complete segments, oldest first.
    ///
    /// `.partial` files are skipped: a crash mid-write leaves one, and reading half a
    /// segment would insert half a batch and then fail on a truncated line.
    pub fn segments(&self) -> std::io::Result<Vec<PathBuf>> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&self.config.directory)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == SEGMENT))
            .collect();
        // The names are zero-padded and start with the epoch, so lexical order is
        // chronological order across restarts as well as within a run.
        found.sort();
        Ok(found)
    }

    /// Read one segment back.
    ///
    /// # Errors
    ///
    /// The file could not be read at all. A segment with *some* unreadable lines is not
    /// an error — see the body.
    pub fn read(&mut self, path: &Path) -> std::io::Result<Vec<LogRow>> {
        let text = std::fs::read_to_string(path)?;
        let mut rows = Vec::new();
        let mut bad = 0usize;

        for line in text.lines().filter(|l| !l.is_empty()) {
            match serde_json::from_str::<LogRow>(line) {
                Ok(row) => rows.push(row),
                // One unreadable line loses one row. Failing the segment would lose all
                // of them, and the likeliest cause of a bad line is a partial last write
                // — which is the tail, not the body.
                Err(_) => bad += 1,
            }
        }

        if bad > 0 {
            eprintln!(
                "wal: {} unreadable line(s) in {}, {} row(s) recovered",
                bad,
                path.display(),
                rows.len()
            );
            self.stats.segments_corrupt += 1;
        }
        Ok(rows)
    }

    /// Forget a segment that has been written to `ClickHouse`.
    ///
    /// Called only after a successful insert. Doing it before would turn a failed replay
    /// into silent loss, which is the one thing this module exists to prevent.
    pub fn done(&mut self, path: &Path, rows: usize) -> std::io::Result<()> {
        std::fs::remove_file(path)?;
        self.stats.segments_replayed += 1;
        self.stats.rows_replayed += rows as u64;
        Ok(())
    }

    /// Total bytes on disk.
    pub fn bytes(&self) -> std::io::Result<u64> {
        let mut total = 0;
        for path in self.segments()? {
            total += std::fs::metadata(&path).map_or(0, |m| m.len());
        }
        Ok(total)
    }

    /// Drop the oldest segments until the spill fits.
    ///
    /// The same rule the memory buffer uses: during an incident the newest logs are the
    /// ones being looked at, and a spill directory that filled a disk would take the
    /// collector down — at which point nothing is being written at all.
    fn enforce_limit(&mut self) -> std::io::Result<()> {
        let mut total = self.bytes()?;
        if total <= self.config.max_bytes {
            return Ok(());
        }
        for path in self.segments()? {
            if total <= self.config.max_bytes {
                break;
            }
            let size = std::fs::metadata(&path).map_or(0, |m| m.len());
            let rows = std::fs::read_to_string(&path)
                .map_or(0, |t| t.lines().filter(|l| !l.is_empty()).count());
            std::fs::remove_file(&path)?;
            total = total.saturating_sub(size);
            self.stats.rows_dropped += rows as u64;
            eprintln!(
                "wal: the spill is full; {} row(s) lost from {}",
                rows,
                path.display()
            );
        }
        Ok(())
    }
}

/// A temporary directory that removes itself, without a dependency for it.
///
/// At file scope rather than inside `mod tests` because `batch`'s tests need one too, and
/// a second copy would be a second thing to keep right.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::path::{Path, PathBuf};

    #[derive(Debug)]
    pub struct Dir(PathBuf);

    impl Dir {
        pub fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("uops-wal-{}", uuid::Uuid::now_v7().simple()));
            std::fs::create_dir_all(&path).expect("a scratch directory");
            Self(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// What a `DateTime64(3)` column keeps.
    fn to_millis(t: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(t.timestamp_millis()).expect("in range")
    }

    fn scratch() -> (tempdir::Dir, Wal) {
        let dir = tempdir::Dir::new();
        let wal = Wal::open(Config {
            directory: dir.path().to_path_buf(),
            ..Config::default()
        })
        .expect("open");
        (dir, wal)
    }

    use super::tests_support as tempdir;

    #[test]
    fn a_spilled_batch_reads_back_as_what_clickhouse_would_have_stored() {
        // The whole promise, and the precision in the name is the point. The timestamp
        // columns are `DateTime64(3)` and this file's serialiser writes milliseconds, so
        // a replayed row is truncated to milliseconds — exactly as the row would have
        // been had the original insert succeeded.
        //
        // Asserting equality with the *untruncated* input would be asserting that the WAL
        // is more precise than its destination, which is not a property worth having and
        // not one it can keep.
        let (_dir, mut wal) = scratch();
        let batch = vec![row("first"), row("second")];

        wal.spill(&batch).expect("spill");
        let segments = wal.segments().expect("segments");
        assert_eq!(segments.len(), 1);

        let read = wal.read(&segments[0]).expect("read");
        let stored: Vec<LogRow> = batch
            .iter()
            .cloned()
            .map(|mut r| {
                r.observed_at = to_millis(r.observed_at);
                r.ingested_at = to_millis(r.ingested_at);
                r
            })
            .collect();
        assert_eq!(
            read, stored,
            "a replayed row must be the row ClickHouse would have stored"
        );
        assert_eq!(wal.stats().rows_spilled, 2);

        // And the truncation really is only sub-millisecond, rather than something larger
        // hiding behind a lenient comparison.
        let drift = (batch[0].observed_at - read[0].observed_at)
            .num_microseconds()
            .expect("a small difference");
        assert!((0..1_000).contains(&drift), "{drift} microseconds");
    }

    #[test]
    fn segments_replay_in_the_order_they_were_written() {
        // Not strictly required for correctness — ClickHouse sorts by observed_at and
        // every row carries its own — but a replay that interleaved would make the
        // insert's part boundaries meaningless, and "oldest first" is what makes a
        // partially-drained spill comprehensible to somebody looking at it with ls.
        let (_dir, mut wal) = scratch();
        for n in 0..12 {
            wal.spill(&[row(&format!("batch {n}"))]).expect("spill");
        }

        let segments = wal.segments().expect("segments");
        assert_eq!(segments.len(), 12);

        let bodies: Vec<String> = segments
            .iter()
            .map(|p| wal.read(p).expect("read")[0].body.clone())
            .collect();
        let expected: Vec<String> = (0..12).map(|n| format!("batch {n}")).collect();
        assert_eq!(
            bodies, expected,
            "zero padding is what keeps 10 after 9 rather than after 1"
        );
    }

    #[test]
    fn a_segment_survives_the_process_that_wrote_it() {
        // The case the whole module exists for: a crash mid-outage. A second `Wal` over
        // the same directory is what a restarted collector sees.
        let dir = tempdir::Dir::new();
        let config = Config {
            directory: dir.path().to_path_buf(),
            ..Config::default()
        };

        {
            let mut wal = Wal::open(config.clone()).expect("open");
            wal.spill(&[row("written before the crash")])
                .expect("spill");
        }

        let after = Wal::open(config).expect("reopen");
        assert!(
            after.has_pending(),
            "a restarted collector must find what the previous one spilled"
        );
        let mut after = after;
        let segments = after.segments().expect("segments");
        assert_eq!(
            after.read(&segments[0]).expect("read")[0].body,
            "written before the crash"
        );
    }

    #[test]
    fn a_half_written_segment_is_ignored_rather_than_half_read() {
        // A crash during a spill leaves a `.partial`. The rename is the commit, so a
        // file that never got renamed is one that was never promised.
        let (dir, mut wal) = scratch();
        std::fs::write(
            dir.path().join("00000000000000000000-0000000000.partial"),
            b"{\"not\": \"a complete row\"}\n",
        )
        .expect("write a partial");

        assert!(!wal.has_pending(), "a partial segment is not a segment");
        wal.spill(&[row("a real one")]).expect("spill");
        assert_eq!(
            wal.segments().expect("segments").len(),
            1,
            "and it is not counted alongside a real one"
        );
    }

    #[test]
    fn a_segment_is_only_forgotten_after_it_is_stored() {
        // Removing before the insert succeeds turns a failed replay into silent loss,
        // which is the one thing this module exists to prevent.
        let (_dir, mut wal) = scratch();
        wal.spill(&[row("owed")]).expect("spill");

        let segments = wal.segments().expect("segments");
        let rows = wal.read(&segments[0]).expect("read");
        // Reading does not remove.
        assert!(wal.has_pending(), "reading a segment must not consume it");

        wal.done(&segments[0], rows.len()).expect("done");
        assert!(!wal.has_pending());
        assert_eq!(wal.stats().rows_replayed, 1);
    }

    #[test]
    fn a_full_spill_drops_the_oldest_and_says_so() {
        // Disk is finite and an outage can outlast it. A spill directory that filled the
        // disk would take the collector down, and then nothing is being written at all.
        let dir = tempdir::Dir::new();
        let mut wal = Wal::open(Config {
            directory: dir.path().to_path_buf(),
            // Smaller than two segments, so the third spill has to evict.
            max_bytes: 1_200,
            ..Config::default()
        })
        .expect("open");

        for n in 0..6 {
            wal.spill(&[row(&format!("batch {n}"))]).expect("spill");
        }

        assert!(wal.stats().rows_dropped > 0, "{:?}", wal.stats());
        assert!(
            wal.bytes().expect("bytes") <= 1_200 + 700,
            "the spill must stay near its bound: {} bytes",
            wal.bytes().expect("bytes")
        );

        // And what survived is the newest, which is the whole point of dropping the
        // oldest.
        let segments = wal.segments().expect("segments");
        let last = wal.read(segments.last().expect("a segment")).expect("read");
        assert_eq!(last[0].body, "batch 5");
    }

    #[test]
    fn an_unreadable_line_loses_one_row_and_not_the_segment() {
        // The likeliest cause of a bad line is a partial last write, which is the tail
        // rather than the body. Failing the segment would lose the other nine thousand.
        let (dir, mut wal) = scratch();
        wal.spill(&[row("good one"), row("good two")])
            .expect("spill");

        let path = wal.segments().expect("segments")[0].clone();
        let mut text = std::fs::read_to_string(&path).expect("read");
        text.push_str("{ this is not json\n");
        std::fs::write(&path, text).expect("rewrite");

        let rows = wal.read(&path).expect("read");
        assert_eq!(rows.len(), 2, "the readable rows survive");
        assert_eq!(wal.stats().segments_corrupt, 1);

        let _ = dir;
    }

    #[test]
    fn an_unwritable_directory_fails_at_open_rather_than_during_the_outage() {
        // `create_dir_all` succeeds on a directory that already exists and says nothing
        // about whether it can be written to. Finding that out during the ClickHouse
        // outage the spill exists for is the worst possible time.
        let dir = tempdir::Dir::new();
        let path = dir.path().join("a-file-not-a-directory");
        std::fs::write(&path, b"x").expect("write");

        assert!(
            Wal::open(Config {
                directory: path,
                ..Config::default()
            })
            .is_err(),
            "a path that is a file must not open as a spill directory"
        );
    }

    #[test]
    fn spilling_nothing_writes_nothing() {
        let (_dir, mut wal) = scratch();
        wal.spill(&[]).expect("spill");
        assert!(!wal.has_pending());
        assert_eq!(wal.stats().segments_written, 0);
    }
}
