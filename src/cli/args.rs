//! Command-line surface.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::benchmark::SamplingPlan;

#[derive(Debug, Parser)]
#[command(
    name = "minwin",
    version,
    about = "MinWin — measure first. Change second.",
    long_about = "MinWin is an experimental Windows 11 optimisation tool. It measures the \
                  system before changing it, explains every supported change and its tradeoff, \
                  records exactly what it changed, and can roll those changes back.\n\n\
                  MinWin does not disable Windows Defender, the firewall, Windows Update, UAC, \
                  SmartScreen, Memory Integrity or credential protections. Security is not \
                  treated as bloat.",
    disable_help_subcommand = false,
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    #[command(flatten)]
    pub global: GlobalOptions,
}

#[derive(Debug, Args, Clone)]
pub struct GlobalOptions {
    /// Increase diagnostic detail. `-v` for MinWin's own activity, `-vv` to
    /// include dependencies. Diagnostics go to stderr, so piping stdout for
    /// `--json` stays clean.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Store MinWin's state in this directory instead of
    /// %LOCALAPPDATA%\MinWin. Affects only MinWin's own files.
    #[arg(long, value_name = "DIR", global = true)]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show MinWin and system state
    Status(StatusArgs),

    /// Measure the current system
    Benchmark(BenchmarkArgs),

    /// Apply an optimisation profile
    Apply(ApplyArgs),

    /// Compare MinWin's record with the current system
    Diff(DiffArgs),

    /// Restore the previous recorded state
    Rollback(RollbackArgs),

    /// Explain what MinWin changes, and what it refuses to change
    Explain(ExplainArgs),
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Emit machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct BenchmarkArgs {
    /// Number of samples to collect.
    #[arg(long, default_value_t = SamplingPlan::default().samples, value_parser = clap::value_parser!(u32).range(1..=600))]
    pub samples: u32,

    /// Milliseconds between samples.
    #[arg(long, default_value_t = SamplingPlan::default().interval_ms, value_parser = clap::value_parser!(u64).range(50..=60_000))]
    pub interval_ms: u64,

    /// Milliseconds to let the system settle before the first sample.
    #[arg(long, default_value_t = SamplingPlan::default().settle_ms, value_parser = clap::value_parser!(u64).range(0..=60_000))]
    pub settle_ms: u64,

    /// A short label stored with the run, such as "after apply minimal".
    #[arg(long, value_name = "TEXT")]
    pub label: Option<String>,

    /// Emit machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

impl BenchmarkArgs {
    pub fn plan(&self) -> SamplingPlan {
        SamplingPlan {
            samples: self.samples,
            interval_ms: self.interval_ms,
            settle_ms: self.settle_ms,
        }
    }
}

#[derive(Debug, Args)]
pub struct ApplyArgs {
    /// Profile to apply: minimal or gaming.
    #[arg(value_name = "PROFILE", default_value = "minimal")]
    pub profile: String,

    /// Show exactly what would change and exit without touching anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip the confirmation prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// Load a profile from a file instead of using a built-in one.
    #[arg(long, value_name = "PATH", conflicts_with = "profile")]
    pub profile_file: Option<PathBuf>,

    /// Emit machine-readable JSON. Implies --dry-run unless --yes is given,
    /// because a confirmation prompt cannot be answered in JSON mode.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct DiffArgs {
    /// Emit machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct RollbackArgs {
    /// Skip the confirmation prompt. This does NOT authorise restoring values
    /// that changed outside MinWin; that needs --allow-external-changes.
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// Allow MinWin to overwrite values that were changed after it applied
    /// them, discarding whatever changed them.
    #[arg(long)]
    pub allow_external_changes: bool,

    /// Show what would be restored and exit without touching anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ExplainArgs {
    /// A specific change id. Omit to list every registered change.
    #[arg(value_name = "CHANGE_ID")]
    pub change_id: Option<String>,

    /// Emit machine-readable JSON instead of prose.
    #[arg(long)]
    pub json: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn the_cli_definition_is_internally_consistent() {
        // Catches conflicting flags, duplicate short options and bad defaults.
        Cli::command().debug_assert();
    }

    #[test]
    fn status_defaults_to_human_output() {
        let cli = parse(&["minwin", "status"]);
        match cli.command {
            Command::Status(args) => assert!(!args.json),
            other => panic!("expected status, got {other:?}"),
        }
    }

    #[test]
    fn apply_defaults_to_the_minimal_profile_with_no_dry_run_and_no_yes() {
        let cli = parse(&["minwin", "apply"]);
        match cli.command {
            Command::Apply(args) => {
                assert_eq!(args.profile, "minimal");
                assert!(!args.dry_run);
                assert!(!args.yes, "confirmation must be required by default");
            }
            other => panic!("expected apply, got {other:?}"),
        }
    }

    #[test]
    fn apply_accepts_a_profile_name_and_the_dry_run_flag() {
        let cli = parse(&["minwin", "apply", "gaming", "--dry-run"]);
        match cli.command {
            Command::Apply(args) => {
                assert_eq!(args.profile, "gaming");
                assert!(args.dry_run);
            }
            other => panic!("expected apply, got {other:?}"),
        }
    }

    #[test]
    fn apply_accepts_the_short_yes_flag() {
        for args in [
            ["minwin", "apply", "minimal", "-y"],
            ["minwin", "apply", "minimal", "--yes"],
        ] {
            match parse(&args).command {
                Command::Apply(apply) => assert!(apply.yes),
                other => panic!("expected apply, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_profile_file_cannot_be_combined_with_a_profile_name() {
        // Otherwise it would be ambiguous which one MinWin applied.
        assert!(
            Cli::try_parse_from(["minwin", "apply", "gaming", "--profile-file", "x.toml"]).is_err()
        );
        // On its own it is fine.
        assert!(Cli::try_parse_from(["minwin", "apply", "--profile-file", "x.toml"]).is_ok());
    }

    #[test]
    fn rollback_keeps_yes_and_external_change_authorisation_separate() {
        // The safety property: --yes alone must not imply the other.
        let cli = parse(&["minwin", "rollback", "--yes"]);
        match cli.command {
            Command::Rollback(args) => {
                assert!(args.yes);
                assert!(
                    !args.allow_external_changes,
                    "--yes must not authorise overwriting external changes"
                );
            }
            other => panic!("expected rollback, got {other:?}"),
        }

        let cli = parse(&["minwin", "rollback", "--allow-external-changes"]);
        match cli.command {
            Command::Rollback(args) => {
                assert!(args.allow_external_changes);
                assert!(!args.yes);
            }
            other => panic!("expected rollback, got {other:?}"),
        }
    }

    #[test]
    fn benchmark_defaults_match_the_documented_sampling_plan() {
        let cli = parse(&["minwin", "benchmark"]);
        match cli.command {
            Command::Benchmark(args) => {
                assert_eq!(args.plan(), SamplingPlan::default());
                assert_eq!(args.samples, 10);
                assert_eq!(args.interval_ms, 1000);
            }
            other => panic!("expected benchmark, got {other:?}"),
        }
    }

    #[test]
    fn benchmark_sampling_can_be_configured() {
        let cli = parse(&[
            "minwin",
            "benchmark",
            "--samples",
            "30",
            "--interval-ms",
            "500",
            "--settle-ms",
            "0",
            "--label",
            "after apply minimal",
        ]);
        match cli.command {
            Command::Benchmark(args) => {
                assert_eq!(
                    args.plan(),
                    SamplingPlan {
                        samples: 30,
                        interval_ms: 500,
                        settle_ms: 0,
                    }
                );
                assert_eq!(args.label.as_deref(), Some("after apply minimal"));
            }
            other => panic!("expected benchmark, got {other:?}"),
        }
    }

    #[test]
    fn nonsensical_sampling_values_are_rejected_at_parse_time() {
        for args in [
            vec!["minwin", "benchmark", "--samples", "0"],
            vec!["minwin", "benchmark", "--interval-ms", "1"],
            vec!["minwin", "benchmark", "--samples", "100000"],
        ] {
            assert!(
                Cli::try_parse_from(&args).is_err(),
                "{args:?} should be rejected"
            );
        }
    }

    #[test]
    fn verbosity_counts_and_is_available_on_every_subcommand() {
        assert_eq!(parse(&["minwin", "status"]).global.verbose, 0);
        assert_eq!(parse(&["minwin", "status", "-v"]).global.verbose, 1);
        assert_eq!(parse(&["minwin", "-vv", "status"]).global.verbose, 2);
        assert_eq!(parse(&["minwin", "diff", "-v"]).global.verbose, 1);
    }

    #[test]
    fn the_data_directory_can_be_redirected() {
        let cli = parse(&["minwin", "status", "--data-dir", r"C:\temp\minwin"]);
        assert_eq!(cli.global.data_dir, Some(PathBuf::from(r"C:\temp\minwin")));
    }

    #[test]
    fn every_command_supports_json() {
        for args in [
            vec!["minwin", "status", "--json"],
            vec!["minwin", "benchmark", "--json"],
            vec!["minwin", "diff", "--json"],
            vec!["minwin", "apply", "--json"],
            vec!["minwin", "rollback", "--json"],
            vec!["minwin", "explain", "--json"],
        ] {
            assert!(Cli::try_parse_from(&args).is_ok(), "{args:?} should parse");
        }
    }

    #[test]
    fn explain_works_with_and_without_a_change_id() {
        match parse(&["minwin", "explain"]).command {
            Command::Explain(args) => assert!(args.change_id.is_none()),
            other => panic!("expected explain, got {other:?}"),
        }
        match parse(&["minwin", "explain", "telemetry.diagtrack_start_type"]).command {
            Command::Explain(args) => assert_eq!(
                args.change_id.as_deref(),
                Some("telemetry.diagtrack_start_type")
            ),
            other => panic!("expected explain, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["minwin", "debloat"]).is_err());
        assert!(Cli::try_parse_from(["minwin"]).is_err());
    }

    #[test]
    fn the_help_text_states_the_security_boundary() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("Defender"));
        assert!(help.contains("Security is not"));
    }
}
