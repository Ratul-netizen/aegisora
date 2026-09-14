//! Applying a plan, and recording what was applied.

use std::time::Instant;

use crate::error::{Error, Result};
use crate::ledger::{
    AppliedMigration, AppliedStep, LEDGER_DDL, LedgerState, SELECT_COMPLETED, SELECT_STEPS,
};
use crate::plan::{Plan, PlannedMigration};

/// The one thing a transport has to do: send a statement and hand back the response.
///
/// Deliberately this small. Everything that decides *what* to send is pure and lives in
/// [`crate::plan`], so the interesting behaviour — resume, drift, ordering — is tested
/// against a fake that counts statements rather than against a server.
///
/// `params` are `ClickHouse` query parameters, referenced in the SQL as `{name:Type}`.
/// The runner never formats a value into a statement, for the same reason `uops-query`
/// never does: there is then no escaping to get wrong.
pub trait Executor {
    fn run(&self, sql: &str, params: &[(&str, String)]) -> Result<String>;
}

/// Progress, for whoever is watching. An unattended upgrade writes these to a log that
/// someone reads afterwards to find out how far it got.
#[derive(Clone, Debug)]
pub enum Event<'a> {
    Starting {
        version: u32,
        name: &'a str,
        resumed: bool,
        remaining: usize,
    },
    Step {
        index: usize,
        total: usize,
        summary: String,
    },
    Finished {
        version: u32,
        ms: u64,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub migrations: usize,
    pub statements: usize,
    /// Statements skipped because a previous run had already applied them.
    pub resumed_past: usize,
}

pub struct Runner<'a, E: Executor> {
    exec: &'a E,
    /// Recorded on every row: which process applied this. On-premise support asks
    /// "which node ran the upgrade" often enough to be worth a column.
    id: String,
}

/// Prints the runner's identity, not the executor's — the executor holds the
/// credentials, and a `Debug` that reaches into it is how a password ends up in a log.
impl<E: Executor> std::fmt::Debug for Runner<'_, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner").field("id", &self.id).finish()
    }
}

impl<'a, E: Executor> Runner<'a, E> {
    pub fn new(exec: &'a E, id: impl Into<String>) -> Self {
        Self {
            exec,
            id: id.into(),
        }
    }

    /// Create the ledger tables if they are not there. Safe to call every time.
    pub fn ensure_ledger(&self) -> Result<()> {
        for ddl in LEDGER_DDL {
            self.exec.run(ddl, &[])?;
        }
        Ok(())
    }

    /// Read what the database says it has already done.
    pub fn state(&self) -> Result<LedgerState> {
        let completed = self
            .exec
            .run(SELECT_COMPLETED, &[])?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| {
                let f = fields(line, 3, SELECT_COMPLETED)?;
                Ok(AppliedMigration {
                    version: parse_u32(&f[0])?,
                    name: f[1].clone(),
                    checksum: f[2].clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let steps = self
            .exec
            .run(SELECT_STEPS, &[])?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| {
                let f = fields(line, 3, SELECT_STEPS)?;
                Ok(AppliedStep {
                    version: parse_u32(&f[0])?,
                    step: parse_u32(&f[1])?,
                    checksum: f[2].clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(LedgerState { completed, steps })
    }

    /// Apply every pending migration, in order, recording each statement as it lands.
    ///
    /// Stops at the first failure and returns it. What has been applied stays applied
    /// and stays recorded, which is what makes the next invocation a resume rather than
    /// a restart.
    pub fn apply(&self, plan: &Plan, on_event: &mut dyn FnMut(Event<'_>)) -> Result<Report> {
        let mut report = Report::default();

        for m in &plan.pending {
            on_event(Event::Starting {
                version: m.version,
                name: &m.name,
                resumed: m.is_resumed(),
                remaining: m.remaining(),
            });
            report.resumed_past += m.first_step;

            let started = Instant::now();
            for (index, statement) in m.statements.iter().enumerate().skip(m.first_step) {
                on_event(Event::Step {
                    index,
                    total: m.statements.len(),
                    summary: statement.summary(),
                });

                let step_started = Instant::now();
                self.exec.run(&statement.sql, &[])?;
                let step_ms = elapsed_ms(step_started);

                // Recorded immediately after the statement, never batched at the end:
                // the whole point is to survive the process dying between statements.
                self.record_step(m, index, step_ms)?;
                report.statements += 1;
            }

            let ms = elapsed_ms(started);
            self.record_migration(m, ms)?;
            report.migrations += 1;
            on_event(Event::Finished {
                version: m.version,
                ms,
            });
        }

        Ok(report)
    }

    fn record_step(&self, m: &PlannedMigration, index: usize, ms: u64) -> Result<()> {
        const SQL: &str = "INSERT INTO schema_migration_steps
             (version, step, checksum, applied_at, duration_ms)
             VALUES ({version:UInt32}, {step:UInt32}, {checksum:String}, now64(3), {ms:UInt64})";

        let checksum = m.statements[index].sql.clone();
        self.exec.run(
            SQL,
            &[
                ("version", m.version.to_string()),
                ("step", index.to_string()),
                ("checksum", crate::migration::hash(checksum.as_bytes())),
                ("ms", ms.to_string()),
            ],
        )?;
        Ok(())
    }

    fn record_migration(&self, m: &PlannedMigration, ms: u64) -> Result<()> {
        const SQL: &str = "INSERT INTO schema_migrations
             (version, name, checksum, statements, applied_at, duration_ms, runner)
             VALUES ({version:UInt32}, {name:String}, {checksum:String}, {statements:UInt32},
                     now64(3), {ms:UInt64}, {runner:String})";

        self.exec.run(
            SQL,
            &[
                ("version", m.version.to_string()),
                ("name", m.name.clone()),
                ("checksum", m.checksum.clone()),
                ("statements", m.statements.len().to_string()),
                ("ms", ms.to_string()),
                ("runner", self.id.clone()),
            ],
        )?;
        Ok(())
    }
}

fn elapsed_ms(from: Instant) -> u64 {
    u64::try_from(from.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn fields(line: &str, expected: usize, query: &str) -> Result<Vec<String>> {
    let f: Vec<String> = line.split('\t').map(str::to_owned).collect();
    if f.len() != expected {
        return Err(Error::Protocol(format!(
            "expected {expected} tab-separated fields, got {} from: {query}",
            f.len()
        )));
    }
    Ok(f)
}

fn parse_u32(s: &str) -> Result<u32> {
    s.trim()
        .parse()
        .map_err(|_| Error::Protocol(format!("expected a number, got {s:?}")))
}

#[cfg(test)]
pub(crate) mod testing {
    use std::cell::RefCell;

    use super::{Error, Executor, Result};

    /// An in-memory `ClickHouse` that records statements and can be told to fail.
    ///
    /// Enough to drive the runner through its whole lifecycle — apply, crash, resume —
    /// with no server, which is what makes the resume path testable at all. Recreating
    /// a mid-migration crash against a real server means killing a process at exactly
    /// the right moment.
    #[derive(Debug, Default)]
    pub(crate) struct FakeCh {
        pub sent: RefCell<Vec<String>>,
        /// Rows the ledger SELECTs return, as TSV.
        pub completed_tsv: RefCell<String>,
        pub steps_tsv: RefCell<String>,
        /// Fail the Nth non-ledger statement (0-based), once.
        pub fail_at: RefCell<Option<usize>>,
        applied_count: RefCell<usize>,
    }

    impl FakeCh {
        pub(crate) fn schema_statements(&self) -> Vec<String> {
            self.sent
                .borrow()
                .iter()
                .filter(|s| !s.contains("schema_migration"))
                .cloned()
                .collect()
        }

        /// Feed a recorded INSERT back into the SELECT responses, the way a real
        /// database would.
        pub(crate) fn remember_step(&self, version: u32, step: u32, checksum: &str) {
            use std::fmt::Write as _;
            let _ = writeln!(self.steps_tsv.borrow_mut(), "{version}\t{step}\t{checksum}");
        }
    }

    impl Executor for FakeCh {
        fn run(&self, sql: &str, _params: &[(&str, String)]) -> Result<String> {
            self.sent.borrow_mut().push(sql.to_owned());

            if sql.contains("FROM schema_migrations FINAL") {
                return Ok(self.completed_tsv.borrow().clone());
            }
            if sql.contains("FROM schema_migration_steps FINAL") {
                return Ok(self.steps_tsv.borrow().clone());
            }
            if sql.contains("schema_migration") {
                return Ok(String::new());
            }

            let mut n = self.applied_count.borrow_mut();
            let this = *n;
            *n += 1;
            if self.fail_at.borrow().is_some_and(|f| f == this) {
                return Err(Error::Server(format!(
                    "injected failure at statement {this}"
                )));
            }
            Ok(String::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeCh;
    use super::*;
    use crate::migration::Migration;
    use crate::plan::plan;
    use crate::statement;

    fn mig(version: u32, name: &str, sql: &str) -> Migration {
        Migration {
            version,
            name: name.to_owned(),
            checksum: format!("sum-{version}"),
            statements: statement::split(sql),
            path: std::path::PathBuf::from(name),
        }
    }

    fn set() -> Vec<Migration> {
        vec![
            mig(1, "0001_a", "CREATE TABLE a;"),
            mig(
                2,
                "0002_b",
                "CREATE TABLE b; CREATE TABLE c; CREATE TABLE d;",
            ),
        ]
    }

    fn silent(_: Event<'_>) {}

    #[test]
    fn a_fresh_run_applies_every_statement_in_order() {
        let ch = FakeCh::default();
        let runner = Runner::new(&ch, "test");
        runner.ensure_ledger().unwrap();

        let p = plan(&set(), &runner.state().unwrap()).unwrap();
        let report = runner.apply(&p, &mut silent).unwrap();

        assert_eq!(report.migrations, 2);
        assert_eq!(report.statements, 4);
        assert_eq!(
            ch.schema_statements(),
            vec![
                "CREATE TABLE a",
                "CREATE TABLE b",
                "CREATE TABLE c",
                "CREATE TABLE d"
            ]
        );
    }

    #[test]
    fn a_failure_partway_leaves_the_completed_statements_recorded() {
        // The property the whole design exists for. ClickHouse has no transactional
        // DDL, so "it failed, therefore nothing happened" is never true — and a runner
        // that assumes it is will either redo expensive work or refuse to continue.
        let disk = set();
        let ch = FakeCh::default();
        *ch.fail_at.borrow_mut() = Some(2); // third statement overall: 0002's second

        let runner = Runner::new(&ch, "test");
        runner.ensure_ledger().unwrap();
        let p = plan(&disk, &runner.state().unwrap()).unwrap();
        let err = runner.apply(&p, &mut silent).unwrap_err();
        assert!(matches!(err, Error::Server(_)), "{err}");

        let recorded: Vec<String> = ch
            .sent
            .borrow()
            .iter()
            .filter(|s| s.contains("INSERT INTO schema_migration_steps"))
            .cloned()
            .collect();
        assert_eq!(
            recorded.len(),
            2,
            "the two statements that succeeded must be recorded"
        );
        assert!(
            !ch.sent
                .borrow()
                .iter()
                .any(|s| s.contains("INSERT INTO schema_migrations\n") && s.contains("0002")),
            "an unfinished migration must not be marked complete"
        );
    }

    #[test]
    fn the_next_run_resumes_instead_of_starting_over() {
        let disk = set();

        // First attempt: 0001 completes, 0002 fails on its second statement.
        let ch = FakeCh::default();
        *ch.fail_at.borrow_mut() = Some(2);
        let runner = Runner::new(&ch, "test");
        runner.ensure_ledger().unwrap();
        let p = plan(&disk, &runner.state().unwrap()).unwrap();
        assert!(runner.apply(&p, &mut silent).is_err());

        // Second attempt, against a database that remembers what landed.
        let ch2 = FakeCh::default();
        *ch2.completed_tsv.borrow_mut() = "1\t0001_a\tsum-1\n".to_owned();
        ch2.remember_step(2, 0, &disk[1].step_checksum(0));
        let runner2 = Runner::new(&ch2, "test");
        runner2.ensure_ledger().unwrap();

        let p2 = plan(&disk, &runner2.state().unwrap()).unwrap();
        assert_eq!(p2.pending.len(), 1);
        assert!(p2.pending[0].is_resumed());

        let report = runner2.apply(&p2, &mut silent).unwrap();
        assert_eq!(report.statements, 2, "only the unfinished tail re-runs");
        assert_eq!(report.resumed_past, 1);
        assert_eq!(
            ch2.schema_statements(),
            vec!["CREATE TABLE c", "CREATE TABLE d"],
            "the statement that already succeeded must not be sent again"
        );
    }

    #[test]
    fn values_are_bound_as_parameters_never_formatted_in() {
        // The migration name reaches the ledger as data. It comes from a filename today
        // and that is not a reason to interpolate it — the same rule as uops-query.
        let ch = FakeCh::default();
        let runner = Runner::new(&ch, "test");
        runner.ensure_ledger().unwrap();
        let p = plan(&set(), &runner.state().unwrap()).unwrap();
        runner.apply(&p, &mut silent).unwrap();

        let inserts: Vec<String> = ch
            .sent
            .borrow()
            .iter()
            .filter(|s| s.starts_with("INSERT INTO schema_migrations"))
            .cloned()
            .collect();
        assert!(!inserts.is_empty());
        for sql in inserts {
            assert!(sql.contains("{name:String}"), "{sql}");
            assert!(
                !sql.contains("0001_a"),
                "value reached the statement: {sql}"
            );
        }
    }

    #[test]
    fn ledger_rows_that_do_not_parse_are_a_protocol_error_not_a_panic() {
        let ch = FakeCh::default();
        *ch.completed_tsv.borrow_mut() = "not-a-version\tname\tsum\n".to_owned();
        let runner = Runner::new(&ch, "test");
        assert!(matches!(runner.state().unwrap_err(), Error::Protocol(_)));
    }
}
