//! What the database remembers about migrations it has already run.
//!
//! Two tables, because there are two questions. `schema_migrations` answers "which
//! migrations are done" and `schema_migration_steps` answers "how far into this one did
//! we get" — and the second question only exists because `ClickHouse` has no
//! transactional DDL. A migration that fails at its third statement has applied two,
//! and a runner that does not record that either re-runs them (wasteful, and not always
//! safe) or refuses to continue (which is worse: the upgrade is now stuck).
//!
//! # Why `ReplacingMergeTree`
//!
//! `ClickHouse` has no unique constraint and no upsert. A retry after a network failure
//! that dropped the response — the statement ran, the acknowledgement did not arrive —
//! writes the row twice. `ReplacingMergeTree` ordered by the row's identity collapses
//! those duplicates on merge, and reads use `FINAL` so they are correct before the
//! merge happens.

/// Bootstrap DDL, applied before any migration is considered.
///
/// Idempotent, and deliberately not itself a migration: the ledger cannot be recorded
/// in a table that does not exist yet.
pub const LEDGER_DDL: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS schema_migrations
     (
         version     UInt32,
         name        String,
         checksum    String,
         statements  UInt32,
         applied_at  DateTime64(3, 'UTC'),
         duration_ms UInt64,
         runner      String
     )
     ENGINE = ReplacingMergeTree(applied_at)
     ORDER BY version",
    "CREATE TABLE IF NOT EXISTS schema_migration_steps
     (
         version     UInt32,
         step        UInt32,
         checksum    String,
         applied_at  DateTime64(3, 'UTC'),
         duration_ms UInt64
     )
     ENGINE = ReplacingMergeTree(applied_at)
     ORDER BY (version, step)",
];

/// A migration the database says it finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedMigration {
    pub version: u32,
    pub name: String,
    pub checksum: String,
}

/// One statement the database says it ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedStep {
    pub version: u32,
    pub step: u32,
    pub checksum: String,
}

/// Everything the ledger knows, read in one pass.
#[derive(Clone, Debug, Default)]
pub struct LedgerState {
    pub completed: Vec<AppliedMigration>,
    pub steps: Vec<AppliedStep>,
}

impl LedgerState {
    #[must_use]
    pub fn completed_version(&self, version: u32) -> Option<&AppliedMigration> {
        self.completed.iter().find(|m| m.version == version)
    }

    /// The highest finished version, or `None` on a fresh database.
    #[must_use]
    pub fn high_water_mark(&self) -> Option<u32> {
        self.completed.iter().map(|m| m.version).max()
    }

    /// Steps recorded for one version, in order.
    #[must_use]
    pub fn steps_of(&self, version: u32) -> Vec<&AppliedStep> {
        let mut v: Vec<&AppliedStep> = self.steps.iter().filter(|s| s.version == version).collect();
        v.sort_by_key(|s| s.step);
        v
    }
}

/// `FINAL` is required, not decorative: without it a retried write is visible as two
/// rows until the next merge, and the runner would see a migration as applied twice.
pub const SELECT_COMPLETED: &str =
    "SELECT version, name, checksum FROM schema_migrations FINAL ORDER BY version FORMAT TSV";

pub const SELECT_STEPS: &str = "SELECT version, step, checksum FROM schema_migration_steps FINAL \
     ORDER BY version, step FORMAT TSV";

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> LedgerState {
        LedgerState {
            completed: vec![
                AppliedMigration {
                    version: 1,
                    name: "0001_logs".into(),
                    checksum: "aa".into(),
                },
                AppliedMigration {
                    version: 2,
                    name: "0002_proj".into(),
                    checksum: "bb".into(),
                },
            ],
            steps: vec![
                AppliedStep {
                    version: 3,
                    step: 1,
                    checksum: "d2".into(),
                },
                AppliedStep {
                    version: 3,
                    step: 0,
                    checksum: "d1".into(),
                },
            ],
        }
    }

    #[test]
    fn the_high_water_mark_is_the_highest_finished_migration() {
        assert_eq!(state().high_water_mark(), Some(2));
        assert_eq!(LedgerState::default().high_water_mark(), None);
    }

    #[test]
    fn steps_come_back_in_order_whatever_order_the_rows_arrive_in() {
        // ClickHouse honours ORDER BY, but a partially-merged ReplacingMergeTree can
        // still interleave parts, and resume position is computed from this sequence.
        let s = state();
        let steps = s.steps_of(3);
        assert_eq!(steps.iter().map(|s| s.step).collect::<Vec<_>>(), vec![0, 1]);
        assert!(s.steps_of(9).is_empty());
    }

    #[test]
    fn reads_use_final_so_a_retried_write_is_not_counted_twice() {
        assert!(SELECT_COMPLETED.contains(" FINAL "), "{SELECT_COMPLETED}");
        assert!(SELECT_STEPS.contains(" FINAL "), "{SELECT_STEPS}");
    }

    #[test]
    fn the_ledger_ddl_is_idempotent() {
        // It runs on every invocation, including the ones where everything is already
        // applied. Without IF NOT EXISTS the runner would fail on its second run.
        for ddl in LEDGER_DDL {
            assert!(ddl.contains("IF NOT EXISTS"), "{ddl}");
        }
    }
}
