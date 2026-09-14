//! `ClickHouse` schema migrations — versioned, idempotent, resumable.
//!
//! PLAN §0b is the reason this is a component rather than a folder of SQL: on-premise
//! customers upgrade **unattended, across several versions at once**, and nobody is
//! watching when it happens. A hand-applied directory of DDL does not survive that.
//!
//! # What `ClickHouse` makes different
//!
//! `sqlx migrate` can wrap a migration in a transaction and say "it failed, therefore
//! nothing happened". `ClickHouse` has no transactional DDL, so that sentence is never
//! true here. A file that fails at its third statement has applied two, and the design
//! follows from admitting that:
//!
//! | consequence | what the runner does |
//! |---|---|
//! | A migration can be half-applied | records **every statement** as it lands, and resumes from there |
//! | A resumed statement may have been half-done | requires every statement to be individually idempotent |
//! | There is no unique constraint or upsert | ledger tables are `ReplacingMergeTree`, read with `FINAL` |
//! | One statement per HTTP request | files are split properly, not on `;` |
//!
//! # Four refusals
//!
//! Refusing is more important than applying, so all four are decided in [`plan`],
//! purely, before anything is sent: an applied migration that was **edited**, an
//! applied migration that is **missing from disk** (usually a rollback in progress), a
//! **new migration numbered below** one already applied (a branch merge nobody tested),
//! and **two files claiming one version**.
//!
//! # Example
//!
//! ```no_run
//! use std::path::Path;
//! use uops_ch_migrate::{HttpExecutor, Runner, http::Config, migration, plan};
//!
//! let exec = HttpExecutor::new(Config::from_env());
//! let runner = Runner::new(&exec, "example");
//! runner.ensure_ledger()?;
//!
//! let on_disk = migration::load_dir(Path::new("ch-migrations"))?;
//! let todo = plan::plan(&on_disk, &runner.state()?)?;
//!
//! let report = runner.apply(&todo, &mut |_event| {})?;
//! println!("{} migrations, {} statements", report.migrations, report.statements);
//! # Ok::<(), uops_ch_migrate::Error>(())
//! ```

pub mod error;
pub mod ledger;
pub mod migration;
pub mod plan;
pub mod runner;
pub mod statement;

#[cfg(feature = "http")]
pub mod http;

pub use error::{Error, Result};
pub use ledger::LedgerState;
pub use migration::Migration;
pub use plan::{Plan, PlannedMigration};
pub use runner::{Event, Executor, Report, Runner};
pub use statement::Statement;

#[cfg(feature = "http")]
pub use http::HttpExecutor;
