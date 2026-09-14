//! `uops-ch-migrate` — apply the `ClickHouse` schema.
//!
//! Runs at deploy time and at server start. The output is written for the person
//! reading an unattended upgrade's log afterwards: every statement it sent, how long
//! each took, and — when it refuses — what is wrong and what to do about it.
//!
//! ```text
//! uops-ch-migrate status            what is applied, what is outstanding
//! uops-ch-migrate apply             apply everything outstanding
//! uops-ch-migrate apply --dry-run   print the plan, send nothing
//!
//! --dir PATH        migration directory (default: ch-migrations)
//! CLICKHOUSE_URL    default http://localhost:8123
//! CLICKHOUSE_DB     default uops
//! CLICKHOUSE_USER / CLICKHOUSE_PASSWORD
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use uops_ch_migrate::http::{Config, HttpExecutor};
use uops_ch_migrate::{Event, Result, Runner, migration, plan};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // One line, on stderr, naming the file and the remedy. See error.rs.
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    command: Command,
    dir: PathBuf,
    dry_run: bool,
}

#[derive(PartialEq, Eq)]
enum Command {
    Status,
    Apply,
}

fn parse_args() -> std::result::Result<Args, String> {
    let mut command = None;
    let mut dir = PathBuf::from("ch-migrations");
    let mut dry_run = false;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "status" => command = Some(Command::Status),
            "apply" => command = Some(Command::Apply),
            "--dry-run" => dry_run = true,
            "--dir" => {
                dir = it
                    .next()
                    .ok_or_else(|| "--dir needs a path".to_owned())?
                    .into();
            }
            "-h" | "--help" => return Err(usage()),
            other => return Err(format!("unknown argument {other}\n\n{}", usage())),
        }
    }

    Ok(Args {
        command: command.unwrap_or(Command::Status),
        dir,
        dry_run,
    })
}

fn usage() -> String {
    "usage: uops-ch-migrate [status|apply] [--dry-run] [--dir PATH]".to_owned()
}

fn run() -> Result<()> {
    let args = match parse_args() {
        Ok(a) => a,
        Err(message) => {
            println!("{message}");
            return Ok(());
        }
    };

    let config = Config::from_env();
    println!(
        "clickhouse {} database {} as {}",
        config.url, config.database, config.user
    );

    let exec = HttpExecutor::new(config);
    let runner = Runner::new(&exec, runner_id());

    runner.ensure_ledger()?;
    let on_disk = migration::load_dir(&args.dir)?;
    let state = runner.state()?;
    let todo = plan::plan(&on_disk, &state)?;

    println!(
        "{} migration(s) on disk, {} applied, {} outstanding",
        on_disk.len(),
        todo.already_applied,
        todo.pending.len()
    );

    if todo.is_empty() {
        println!("schema is up to date");
        return Ok(());
    }

    for m in &todo.pending {
        let how = if m.is_resumed() {
            format!(
                " (resuming at statement {} of {})",
                m.first_step + 1,
                m.statements.len()
            )
        } else {
            String::new()
        };
        println!("  pending {:04} {}{}", m.version, m.name, how);
    }

    if args.command == Command::Status || args.dry_run {
        println!(
            "{} statement(s) would run; nothing was sent",
            todo.statements_to_run()
        );
        return Ok(());
    }

    let report = runner.apply(&todo, &mut |event| match event {
        Event::Starting {
            version,
            name,
            resumed,
            remaining,
        } => {
            let verb = if resumed { "resuming" } else { "applying" };
            println!("{verb} {version:04} {name} — {remaining} statement(s)");
        }
        Event::Step {
            index,
            total,
            summary,
        } => println!("  [{}/{}] {}", index + 1, total, summary),
        Event::Finished { version, ms } => println!("  {version:04} done in {ms} ms"),
    })?;

    println!(
        "applied {} migration(s), {} statement(s)",
        report.migrations, report.statements
    );
    if report.resumed_past > 0 {
        println!(
            "skipped {} statement(s) already applied by an earlier run",
            report.resumed_past
        );
    }
    Ok(())
}

/// Recorded on every ledger row. On-premise support asks "which node ran the upgrade"
/// often enough that guessing is worse than an environment variable.
fn runner_id() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
}
