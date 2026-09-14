//! Deciding what to run — pure, and the part worth testing hardest.
//!
//! Everything the runner does that can go badly wrong is decided here, against no
//! server and no I/O: which migrations are outstanding, where a half-finished one
//! resumes, and which of four kinds of drift mean "refuse to touch this database".
//!
//! The refusals matter more than the applications. On-premise customers upgrade
//! unattended, across several versions at once, and the two worst outcomes are applying
//! a migration twice and applying one that no longer matches what is recorded. Both are
//! decided here, before anything is sent.

use crate::error::{Error, Result};
use crate::ledger::LedgerState;
use crate::migration::Migration;
use crate::statement::Statement;

/// One migration to run, and where to start in it.
#[derive(Clone, Debug)]
pub struct PlannedMigration {
    pub version: u32,
    pub name: String,
    pub checksum: String,
    /// Index of the first statement to send. Non-zero when resuming.
    pub first_step: usize,
    pub statements: Vec<Statement>,
}

impl PlannedMigration {
    /// Whether this is picking up a migration that failed partway.
    #[must_use]
    pub const fn is_resumed(&self) -> bool {
        self.first_step > 0
    }

    #[must_use]
    pub fn remaining(&self) -> usize {
        self.statements.len().saturating_sub(self.first_step)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub pending: Vec<PlannedMigration>,
    /// How many migrations the database has already finished.
    pub already_applied: usize,
}

impl Plan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    #[must_use]
    pub fn statements_to_run(&self) -> usize {
        self.pending.iter().map(PlannedMigration::remaining).sum()
    }
}

/// Compare the migration directory against the ledger.
pub fn plan(on_disk: &[Migration], state: &LedgerState) -> Result<Plan> {
    // Every finished migration must still be on disk, unchanged. Checked first and for
    // all of them, so that a drifted database reports the drift rather than reporting
    // whatever pending work happens to look fine.
    for applied in &state.completed {
        let Some(found) = on_disk.iter().find(|m| m.version == applied.version) else {
            return Err(Error::MissingOnDisk {
                version: applied.version,
                name: applied.name.clone(),
            });
        };
        if found.checksum != applied.checksum {
            return Err(Error::Changed {
                version: applied.version,
                name: found.name.clone(),
                recorded: short(&applied.checksum),
                found: short(&found.checksum),
            });
        }
    }

    let high_water = state.high_water_mark();
    let mut pending = Vec::new();

    for m in on_disk {
        if state.completed_version(m.version).is_some() {
            continue;
        }

        // A new migration numbered below one already applied means two branches each
        // added one and the merge produced an order nobody tested. Renumbering is a
        // rename; guessing is a schema built in the wrong sequence.
        if let Some(applied) = high_water
            && m.version < applied
        {
            return Err(Error::OutOfOrder {
                version: m.version,
                name: m.name.clone(),
                applied,
            });
        }

        let first_step = resume_point(m, state)?;
        pending.push(PlannedMigration {
            version: m.version,
            name: m.name.clone(),
            checksum: m.checksum.clone(),
            first_step,
            statements: m.statements.clone(),
        });
    }

    Ok(Plan {
        pending,
        already_applied: state.completed.len(),
    })
}

/// Where to restart a migration that was interrupted.
///
/// Only a *contiguous* run of recorded steps from the beginning counts. A gap means the
/// ledger is describing something the runner cannot reconstruct, and restarting from
/// the beginning is the safe reading — every statement is required to be individually
/// idempotent precisely so that this is allowed.
fn resume_point(m: &Migration, state: &LedgerState) -> Result<usize> {
    let recorded = state.steps_of(m.version);
    if recorded.is_empty() {
        return Ok(0);
    }

    let mut point = 0usize;
    for (expected, step) in recorded.iter().enumerate() {
        if step.step as usize != expected {
            break;
        }
        // The statement that was run must be the statement that is there now. A file
        // edited between a failure and the retry would otherwise have its new
        // statements skipped on the strength of the old ones having run.
        let current = m.step_checksum(expected);
        if current != step.checksum {
            return Err(Error::Changed {
                version: m.version,
                name: m.name.clone(),
                recorded: short(&step.checksum),
                found: short(&current),
            });
        }
        point = expected + 1;
    }

    Ok(point.min(m.statements.len()))
}

fn short(checksum: &str) -> String {
    checksum.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{AppliedMigration, AppliedStep};
    use crate::statement;

    fn mig(version: u32, name: &str, sql: &str) -> Migration {
        Migration {
            version,
            name: name.to_owned(),
            checksum: format!("sum-of-{name}"),
            statements: statement::split(sql),
            path: std::path::PathBuf::from(name),
        }
    }

    fn set() -> Vec<Migration> {
        vec![
            mig(1, "0001_logs", "SELECT 1;"),
            mig(2, "0002_proj", "SELECT 2; SELECT 3;"),
            mig(3, "0003_counts", "SELECT 4; SELECT 5; SELECT 6;"),
        ]
    }

    fn applied(m: &Migration) -> AppliedMigration {
        AppliedMigration {
            version: m.version,
            name: m.name.clone(),
            checksum: m.checksum.clone(),
        }
    }

    #[test]
    fn a_fresh_database_runs_everything() {
        let p = plan(&set(), &LedgerState::default()).unwrap();
        assert_eq!(p.pending.len(), 3);
        assert_eq!(p.statements_to_run(), 6);
        assert!(p.pending.iter().all(|m| !m.is_resumed()));
    }

    #[test]
    fn an_up_to_date_database_runs_nothing() {
        // The common case on restart, and the one that must be cheap and silent.
        let disk = set();
        let state = LedgerState {
            completed: disk.iter().map(applied).collect(),
            steps: Vec::new(),
        };
        let p = plan(&disk, &state).unwrap();
        assert!(p.is_empty());
        assert_eq!(p.already_applied, 3);
    }

    #[test]
    fn an_interrupted_migration_resumes_where_it_stopped() {
        // The reason step records exist. 0003 adds a projection, which on a populated
        // table rewrites the data — re-running it because of a network blip on the next
        // statement would be an expensive way to achieve nothing.
        let disk = set();
        let state = LedgerState {
            completed: disk[..2].iter().map(applied).collect(),
            steps: vec![
                AppliedStep {
                    version: 3,
                    step: 0,
                    checksum: disk[2].step_checksum(0),
                },
                AppliedStep {
                    version: 3,
                    step: 1,
                    checksum: disk[2].step_checksum(1),
                },
            ],
        };

        let p = plan(&disk, &state).unwrap();
        assert_eq!(p.pending.len(), 1);
        assert_eq!(p.pending[0].first_step, 2);
        assert_eq!(p.pending[0].remaining(), 1);
        assert!(p.pending[0].is_resumed());
    }

    #[test]
    fn a_gap_in_the_recorded_steps_restarts_from_the_beginning() {
        // Step 1 without step 0 describes a state the runner cannot reconstruct.
        // Restarting is safe only because every statement must be idempotent — which is
        // the rule this behaviour depends on, and why it is stated in the README.
        let disk = set();
        let state = LedgerState {
            completed: disk[..2].iter().map(applied).collect(),
            steps: vec![AppliedStep {
                version: 3,
                step: 1,
                checksum: disk[2].step_checksum(1),
            }],
        };
        assert_eq!(plan(&disk, &state).unwrap().pending[0].first_step, 0);
    }

    #[test]
    fn an_edited_applied_migration_stops_the_run() {
        // The schema the database has is not the schema the file describes, and no
        // amount of running will reconcile them. Refusing is the whole point.
        let mut disk = set();
        let state = LedgerState {
            completed: disk.iter().map(applied).collect(),
            steps: Vec::new(),
        };
        disk[1].checksum = "edited".into();

        let err = plan(&disk, &state).unwrap_err();
        assert!(matches!(err, Error::Changed { version: 2, .. }), "{err}");
    }

    #[test]
    fn editing_a_half_applied_migration_stops_the_run_too() {
        // Otherwise the new statements would be skipped on the strength of the old ones
        // having been applied — a schema that matches neither version of the file.
        let disk = set();
        let state = LedgerState {
            completed: disk[..2].iter().map(applied).collect(),
            steps: vec![AppliedStep {
                version: 3,
                step: 0,
                checksum: "what-used-to-be-there".into(),
            }],
        };
        assert!(matches!(
            plan(&disk, &state).unwrap_err(),
            Error::Changed { version: 3, .. }
        ));
    }

    #[test]
    fn a_deleted_applied_migration_stops_the_run() {
        // Usually means the binary is older than the database — a rollback in progress.
        // Continuing would apply an old migration set over a newer schema.
        let disk = set();
        let state = LedgerState {
            completed: vec![
                applied(&disk[0]),
                AppliedMigration {
                    version: 9,
                    name: "0009_from_the_future".into(),
                    checksum: "x".into(),
                },
            ],
            steps: Vec::new(),
        };
        assert!(matches!(
            plan(&disk, &state).unwrap_err(),
            Error::MissingOnDisk { version: 9, .. }
        ));
    }

    #[test]
    fn a_new_migration_numbered_below_an_applied_one_stops_the_run() {
        // Two branches, two migrations, one merge. Running 0002 after 0003 is an order
        // that was never tested, and on a schema this shape it is usually a table being
        // altered before it is created.
        let mut disk = set();
        let state = LedgerState {
            completed: disk.iter().map(applied).collect(),
            steps: Vec::new(),
        };
        disk.insert(0, mig(0, "0000_late_arrival", "SELECT 0;"));

        let err = plan(&disk, &state).unwrap_err();
        assert!(
            matches!(
                err,
                Error::OutOfOrder {
                    version: 0,
                    applied: 3,
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn drift_is_reported_before_pending_work_is_considered() {
        // A database that is both drifted and behind must report the drift. Reporting
        // the pending work first would invite an operator to run it.
        let mut disk = set();
        disk.push(mig(4, "0004_new", "SELECT 7;"));
        let state = LedgerState {
            completed: vec![AppliedMigration {
                version: 1,
                name: "0001_logs".into(),
                checksum: "not-what-is-on-disk".into(),
            }],
            steps: Vec::new(),
        };
        assert!(matches!(
            plan(&disk, &state).unwrap_err(),
            Error::Changed { version: 1, .. }
        ));
    }
}
