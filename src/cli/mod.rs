//! The command-line interface.
//!
//! This module parses arguments, wires up the real machine and the state
//! database, asks for confirmation, and renders reports. It contains no
//! decisions about what to change or whether a benchmark improved — those live
//! in [`crate::engine`], which is why a GUI could replace this module wholesale.

pub mod args;
pub mod render;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use crate::changes::ChangeRegistry;
use crate::core::clock::SystemClock;
use crate::core::error::{MinWinError, Result};
use crate::core::lock::ProcessLock;
use crate::core::paths::{AppPaths, data_dir_override};
use crate::engine::{apply, bench, diff, rollback, status};
use crate::profiles::{self, Profile};
use crate::state::Database;
use crate::sys::{SystemFacts, live_machine};

use args::{
    ApplyArgs, BenchmarkArgs, Cli, Command, DiffArgs, ExplainArgs, GlobalOptions, RollbackArgs,
    StatusArgs,
};
use clap::Parser;

/// Exit codes, so scripts can distinguish "MinWin said no" from "MinWin broke".
mod exit {
    pub const OK: u8 = 0;
    pub const FAILED: u8 = 1;
    pub const NEEDS_ELEVATION: u8 = 3;
    pub const DECLINED: u8 = 4;
    pub const BUSY: u8 = 5;
}

/// The binary's entry point.
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.global.verbose);

    match run(&cli) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            // User-facing errors go to stderr so `--json` on stdout stays
            // parseable.
            eprintln!("error: {error}");
            // A Windows error's cause chain carries the detail; print it once.
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::from(exit_code_for(&error))
        }
    }
}

fn exit_code_for(error: &MinWinError) -> u8 {
    match error {
        MinWinError::RequiresElevation { .. } => exit::NEEDS_ELEVATION,
        MinWinError::AlreadyRunning { .. } => exit::BUSY,
        other if other.is_privilege_problem() => exit::NEEDS_ELEVATION,
        _ => exit::FAILED,
    }
}

fn init_tracing(verbosity: u8) {
    use tracing_subscriber::EnvFilter;

    // RUST_LOG wins when set, so a developer can be more specific than -vv.
    let filter = match std::env::var("RUST_LOG") {
        Ok(value) if !value.is_empty() => EnvFilter::new(value),
        _ => EnvFilter::new(match verbosity {
            0 => "warn",
            1 => "minwin=info",
            _ => "minwin=debug,info",
        }),
    };

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        // Diagnostics must never contaminate stdout, which carries --json.
        .with_writer(std::io::stderr)
        .with_target(verbosity > 1)
        .without_time()
        .try_init();
}

fn run(cli: &Cli) -> Result<u8> {
    match &cli.command {
        Command::Status(command_args) => run_status(&cli.global, command_args),
        Command::Benchmark(command_args) => run_benchmark(&cli.global, command_args),
        Command::Apply(command_args) => run_apply(&cli.global, command_args),
        Command::Diff(command_args) => run_diff(&cli.global, command_args),
        Command::Rollback(command_args) => run_rollback(&cli.global, command_args),
        Command::Explain(command_args) => run_explain(command_args),
    }
}

/// Resolves MinWin's data directory, honouring `--data-dir` then
/// `MINWIN_DATA_DIR` then the real per-user location.
fn paths(global: &GlobalOptions) -> Result<AppPaths> {
    match global.data_dir.clone().or_else(data_dir_override) {
        Some(directory) => AppPaths::rooted_at(directory),
        None => AppPaths::discover(),
    }
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    let rendered = serde_json::to_string_pretty(value)?;
    println!("{rendered}");
    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn run_status(global: &GlobalOptions, command_args: &StatusArgs) -> Result<u8> {
    let machine = live_machine()?;
    let facts = SystemFacts::gather(machine.as_ref())?;
    let paths = paths(global)?;
    let database = Database::open(&paths.database())?;
    let registry = ChangeRegistry::load();

    let report = status::status(&facts, machine.as_ref(), &database, registry.len())?;

    if command_args.json {
        print_json(&report)?;
    } else {
        print!("{}", render::status(&report));
    }
    Ok(exit::OK)
}

// ---------------------------------------------------------------------------
// benchmark
// ---------------------------------------------------------------------------

fn run_benchmark(global: &GlobalOptions, command_args: &BenchmarkArgs) -> Result<u8> {
    let machine = live_machine()?;
    let facts = SystemFacts::gather(machine.as_ref())?;
    let paths = paths(global)?;
    let mut database = Database::open(&paths.database())?;
    let plan = command_args.plan();

    if !command_args.json {
        print!("{}", render::benchmark_preamble(&plan));
        println!();
        let _ = std::io::stdout().flush();
    }

    let report = bench::run_benchmark(
        machine.as_ref(),
        &facts,
        &mut database,
        plan,
        command_args.label.as_deref(),
        &SystemClock,
        &crate::benchmark::SleepPacer,
    )?;

    if command_args.json {
        print_json(&report)?;
    } else {
        print!("{}", render::benchmark(&report));
    }
    Ok(exit::OK)
}

// ---------------------------------------------------------------------------
// apply
// ---------------------------------------------------------------------------

fn load_profile(command_args: &ApplyArgs, registry: &ChangeRegistry) -> Result<Profile> {
    match &command_args.profile_file {
        Some(path) => profiles::load_from_file(path, registry),
        None => profiles::load_builtin(&command_args.profile, registry),
    }
}

fn run_apply(global: &GlobalOptions, command_args: &ApplyArgs) -> Result<u8> {
    let machine = live_machine()?;
    let facts = SystemFacts::gather(machine.as_ref())?;
    let registry = ChangeRegistry::load();
    let profile = load_profile(command_args, &registry)?;

    // Planning is read-only, so it happens before the lock is taken and
    // regardless of elevation.
    let plan = apply::plan(machine.as_ref(), &facts, &registry, &profile)?;

    // JSON output cannot answer a prompt, so it is a dry run unless the user
    // has already said yes.
    let dry_run = command_args.dry_run || (command_args.json && !command_args.yes);

    if dry_run {
        if command_args.json {
            print_json(&plan)?;
        } else {
            print!("{}", render::apply_plan(&plan, true));
        }
        return Ok(exit::OK);
    }

    if !command_args.json {
        print!("{}", render::apply_plan(&plan, false));
        println!();
    }

    // Everything that needs administrator rights is blocked, so there is
    // nothing MinWin could do in this terminal. Fail with the guidance rather
    // than reporting a successful no-op.
    if !plan.has_work() && plan.needs_elevation() {
        return Err(MinWinError::RequiresElevation {
            suggestion: format!("apply {}", plan.profile_id),
        });
    }

    if !plan.has_work() {
        if !command_args.json {
            println!("Nothing to apply.");
        }
        return Ok(exit::OK);
    }

    if !command_args.yes && !confirm(&render::confirmation_question(&plan))? {
        println!("Cancelled. Nothing was changed.");
        return Ok(exit::DECLINED);
    }

    // Only now does anything become mutable, so the lock starts here.
    let paths = paths(global)?;
    let _lock = ProcessLock::acquire(&paths.lock_file())?;
    let mut database = Database::open(&paths.database())?;

    let outcome = apply::execute(
        machine.as_ref(),
        &facts,
        &registry,
        &mut database,
        &plan,
        &SystemClock,
    )?;

    if command_args.json {
        print_json(&outcome)?;
    } else {
        println!();
        print!("{}", render::apply_outcome(&outcome));
    }

    Ok(if outcome.failed_count() > 0 {
        exit::FAILED
    } else {
        exit::OK
    })
}

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------

fn run_diff(global: &GlobalOptions, command_args: &DiffArgs) -> Result<u8> {
    let machine = live_machine()?;
    let facts = SystemFacts::gather(machine.as_ref())?;
    let registry = ChangeRegistry::load();
    let paths = paths(global)?;
    let database = Database::open(&paths.database())?;

    match diff::diff_latest(machine.as_ref(), &facts, &registry, &database)? {
        None => {
            if command_args.json {
                print_json(&serde_json::json!({ "session": null }))?;
            } else {
                println!(
                    "MinWin has not applied a profile on this machine, so there is nothing to \
                     compare."
                );
            }
            Ok(exit::OK)
        }
        Some(report) => {
            if command_args.json {
                print_json(&report)?;
            } else {
                print!("{}", render::diff(&report));
            }
            Ok(exit::OK)
        }
    }
}

// ---------------------------------------------------------------------------
// rollback
// ---------------------------------------------------------------------------

fn run_rollback(global: &GlobalOptions, command_args: &RollbackArgs) -> Result<u8> {
    let machine = live_machine()?;
    let facts = SystemFacts::gather(machine.as_ref())?;
    let registry = ChangeRegistry::load();
    let paths = paths(global)?;

    // Planning is read-only.
    let plan = {
        let database = Database::open(&paths.database())?;
        rollback::plan_latest(machine.as_ref(), &facts, &registry, &database)?
    };

    let Some(plan) = plan else {
        if command_args.json {
            print_json(&serde_json::json!({ "session": null }))?;
        } else {
            println!("MinWin has nothing to restore.");
        }
        return Ok(exit::OK);
    };

    let dry_run = command_args.dry_run || (command_args.json && !command_args.yes);
    if dry_run {
        if command_args.json {
            print_json(&plan)?;
        } else {
            print!("{}", render::rollback_plan(&plan, true));
        }
        return Ok(exit::OK);
    }

    if !command_args.json {
        print!("{}", render::rollback_plan(&plan, false));
    }

    // `--yes` answers the ordinary question. It deliberately does not
    // authorise discarding somebody else's change; that needs its own flag,
    // and MinWin refuses rather than guessing.
    if plan.requires_explicit_authorisation() && !command_args.allow_external_changes {
        let affected = plan.needing_authorisation().len();
        if plan.straightforward_count() == 0 {
            println!(
                "All {affected} change(s) were modified outside MinWin, or cannot be read. \
                 MinWin will not overwrite them.\n\n\
                 Review them with `minwin diff`. If you want MinWin to restore its originals \
                 anyway, discarding whatever changed them, run:\n    \
                 minwin rollback --allow-external-changes"
            );
            return Ok(exit::DECLINED);
        }
        println!(
            "{affected} change(s) will be skipped because they were modified outside MinWin. \
             Pass --allow-external-changes to restore them anyway."
        );
    }

    if !command_args.yes && !confirm(&render::rollback_confirmation_question(&plan))? {
        println!("Cancelled. Nothing was changed.");
        return Ok(exit::DECLINED);
    }

    let _lock = ProcessLock::acquire(&paths.lock_file())?;
    let mut database = Database::open(&paths.database())?;

    let outcome = rollback::execute(
        machine.as_ref(),
        &facts,
        &registry,
        &mut database,
        &plan,
        rollback::RollbackAuthorisation {
            allow_external_changes: command_args.allow_external_changes,
        },
        &SystemClock,
    )?;

    if command_args.json {
        print_json(&outcome)?;
    } else {
        println!();
        print!("{}", render::rollback_outcome(&outcome));
    }

    Ok(if outcome.failed_count() > 0 {
        exit::FAILED
    } else {
        exit::OK
    })
}

// ---------------------------------------------------------------------------
// explain
// ---------------------------------------------------------------------------

fn run_explain(command_args: &ExplainArgs) -> Result<u8> {
    let registry = ChangeRegistry::load();

    match &command_args.change_id {
        Some(id) => {
            let change = registry.require(id, "the command line")?;
            if command_args.json {
                print_json(change.metadata())?;
            } else {
                print!("{}", render::explain_one(change.metadata()));
            }
        }
        None => {
            let metadata: Vec<&crate::changes::ChangeMetadata> =
                registry.iter().map(|change| change.metadata()).collect();
            if command_args.json {
                print_json(&metadata)?;
            } else {
                print!("{}", render::explain_all(&metadata));
            }
        }
    }
    Ok(exit::OK)
}

// ---------------------------------------------------------------------------
// confirmation
// ---------------------------------------------------------------------------

/// Asks a yes/no question.
///
/// A non-interactive stdin (a pipe, a scheduled task) is treated as "no".
/// Defaulting to yes there would let MinWin change a machine nobody was
/// watching; `--yes` exists for that case and has to be asked for.
fn confirm(question: &str) -> Result<bool> {
    let mut stdout = std::io::stdout();
    if !std::io::stdin().is_terminal() {
        println!(
            "{question}\nstdin is not a terminal, so MinWin cannot ask. Re-run with --yes to \
             confirm without a prompt."
        );
        return Ok(false);
    }

    print!("{question}");
    stdout
        .flush()
        .map_err(|e| MinWinError::io("write the confirmation prompt", e))?;

    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| MinWinError::io("read the confirmation response", e))?;

    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_distinguish_the_reasons_minwin_stopped() {
        assert_eq!(
            exit_code_for(&MinWinError::RequiresElevation {
                suggestion: "apply minimal".into()
            }),
            exit::NEEDS_ELEVATION
        );
        assert_eq!(
            exit_code_for(&MinWinError::AlreadyRunning {
                path: std::path::PathBuf::from("lock")
            }),
            exit::BUSY
        );
        // ERROR_ACCESS_DENIED from a Windows call is also a privilege problem.
        assert_eq!(
            exit_code_for(&MinWinError::windows("open a service", "denied", 5)),
            exit::NEEDS_ELEVATION
        );
        assert_eq!(exit_code_for(&MinWinError::NoBenchmark), exit::FAILED);
    }

    #[test]
    fn the_data_directory_can_be_overridden_by_the_flag() {
        let temp = tempfile::tempdir().expect("temp dir");
        let global = GlobalOptions {
            verbose: 0,
            data_dir: Some(temp.path().join("state")),
        };
        let resolved = paths(&global).expect("paths");
        assert!(resolved.root().starts_with(temp.path()));
        assert!(resolved.database().starts_with(temp.path()));
    }
}
