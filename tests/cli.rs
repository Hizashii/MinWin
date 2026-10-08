//! Tests that run the real `minwin` binary.
//!
//! These check the parts only a process boundary exercises: argument parsing,
//! exit codes, which stream output goes to, and `--json` being machine
//! readable.
//!
//! Every invocation is read-only. `apply` and `rollback` are only ever run with
//! `--dry-run`, or in a state where there is nothing to do, so running
//! `cargo test` cannot change the machine. Each test gets its own scratch
//! `--data-dir`, so the developer's real `%LOCALAPPDATA%\MinWin` is never
//! opened either.

use std::path::Path;
use std::process::{Command, Output};

/// Runs the binary with an isolated data directory.
fn run_in(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_minwin"))
        .args(args)
        .arg("--data-dir")
        .arg(data_dir)
        .output()
        .expect("the minwin binary should run")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

// ---------------------------------------------------------------------------
// help and version
// ---------------------------------------------------------------------------

#[test]
fn help_lists_every_documented_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_minwin"))
        .arg("--help")
        .output()
        .expect("run");
    assert!(output.status.success());

    let help = stdout_of(&output);
    for command in ["status", "benchmark", "apply", "diff", "rollback", "help"] {
        assert!(help.contains(command), "help should mention {command}");
    }
    assert!(help.contains("--verbose"));
}

#[test]
fn long_help_states_the_security_boundary_up_front() {
    let output = Command::new(env!("CARGO_BIN_EXE_minwin"))
        .arg("--help")
        .output()
        .expect("run");
    let help = stdout_of(&output);
    assert!(help.contains("Defender"));
    assert!(help.contains("Security is not treated as bloat"));
}

#[test]
fn the_version_flag_reports_the_crate_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_minwin"))
        .arg("--version")
        .output()
        .expect("run");
    assert!(output.status.success());
    assert!(stdout_of(&output).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn an_unknown_command_fails_with_a_message_on_stderr() {
    let output = Command::new(env!("CARGO_BIN_EXE_minwin"))
        .arg("debloat-everything")
        .output()
        .expect("run");
    assert!(!output.status.success());
    assert!(stderr_of(&output).contains("debloat-everything"));
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[test]
fn status_reports_real_machine_state() {
    let dir = scratch();
    let output = run_in(dir.path(), &["status"]);
    assert!(output.status.success(), "{}", stderr_of(&output));

    let text = stdout_of(&output);
    assert!(text.starts_with("MinWin "));
    assert!(text.contains("Windows"));
    assert!(text.contains("Build"));
    assert!(text.contains("Elevated"));
    assert!(text.contains("Supported"));

    // A fresh data directory means no history, and MinWin must say so rather
    // than inventing a baseline.
    assert!(text.contains("none - run `minwin benchmark`"));
    assert!(text.contains("none applied"));
    assert!(text.contains("nothing to restore"));
}

#[test]
fn status_json_is_parseable_and_carries_the_real_build_number() {
    let dir = scratch();
    let output = run_in(dir.path(), &["status", "--json"]);
    assert!(output.status.success(), "{}", stderr_of(&output));

    let parsed: serde_json::Value =
        serde_json::from_str(&stdout_of(&output)).expect("status --json must be valid JSON");

    assert_eq!(
        parsed["minwin_version"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    // Windows 11 builds are 22000 or higher; this asserts a real reading
    // rather than a placeholder.
    let build = parsed["windows_build"]
        .as_u64()
        .expect("windows_build should be a number");
    assert!(build > 10_000, "implausible build number {build}");
    assert!(parsed["elevated"].is_boolean());
    assert!(parsed["memory"]["total_physical_bytes"].as_u64().unwrap() > 0);
    assert_eq!(parsed["registered_change_count"].as_u64(), Some(4));
    assert!(parsed["baseline"].is_null());
}

#[test]
fn status_uses_the_data_directory_it_was_given() {
    let dir = scratch();
    run_in(dir.path(), &["status"]);
    assert!(
        dir.path().join("state.db").is_file(),
        "the state database should be created in the given directory"
    );
}

// ---------------------------------------------------------------------------
// benchmark
// ---------------------------------------------------------------------------

#[test]
fn benchmark_measures_the_machine_and_explains_its_own_limits() {
    let dir = scratch();
    // Deliberately tiny so the test suite stays fast.
    let output = run_in(
        dir.path(),
        &[
            "benchmark",
            "--samples",
            "3",
            "--interval-ms",
            "50",
            "--settle-ms",
            "50",
        ],
    );
    assert!(output.status.success(), "{}", stderr_of(&output));

    let text = stdout_of(&output);
    assert!(text.contains("never idle"));
    assert!(text.contains("not a performance benchmark"));
    assert!(text.contains("Benchmark complete"));
    assert!(text.contains("Available RAM"));
    assert!(text.contains("Process count"));
    assert!(text.contains("first recorded run"));
    // A first run cannot claim anything.
    assert!(!text.contains("improved"));
}

#[test]
fn benchmark_json_contains_real_summary_statistics() {
    let dir = scratch();
    let output = run_in(
        dir.path(),
        &[
            "benchmark",
            "--json",
            "--samples",
            "3",
            "--interval-ms",
            "50",
            "--settle-ms",
            "50",
        ],
    );
    assert!(output.status.success(), "{}", stderr_of(&output));

    let parsed: serde_json::Value =
        serde_json::from_str(&stdout_of(&output)).expect("benchmark --json must be valid JSON");

    assert_eq!(parsed["plan"]["samples"].as_u64(), Some(3));
    assert!(
        parsed["comparison"].is_null(),
        "first run has no comparison"
    );

    let metrics = parsed["summary"]["metrics"]
        .as_array()
        .expect("metrics array");
    assert!(!metrics.is_empty());

    let memory = metrics
        .iter()
        .find(|metric| metric["metric"] == "memory_available_bytes")
        .expect("available memory should be measured");
    assert!(
        memory["median"].as_f64().unwrap() > 0.0,
        "available memory must be a real reading"
    );
    assert!(memory["interquartile_range"].as_f64().is_some());
}

#[test]
fn a_second_benchmark_produces_a_comparison_rather_than_a_claim() {
    let dir = scratch();
    let args = [
        "benchmark",
        "--samples",
        "6",
        "--interval-ms",
        "50",
        "--settle-ms",
        "50",
    ];
    assert!(run_in(dir.path(), &args).status.success());
    let output = run_in(dir.path(), &args);
    assert!(output.status.success(), "{}", stderr_of(&output));

    let text = stdout_of(&output);
    assert!(text.contains("Compared with run 1"));
    assert!(text.contains("not a statistical significance test"));
    assert!(text.contains("metric(s) improved"));
}

#[test]
fn diagnostics_go_to_stderr_so_json_on_stdout_stays_parseable() {
    let dir = scratch();
    let output = run_in(
        dir.path(),
        &[
            "-vv",
            "benchmark",
            "--json",
            "--samples",
            "2",
            "--interval-ms",
            "50",
            "--settle-ms",
            "50",
        ],
    );
    assert!(output.status.success(), "{}", stderr_of(&output));
    // stdout must still be nothing but JSON.
    serde_json::from_str::<serde_json::Value>(&stdout_of(&output))
        .expect("stdout must remain pure JSON even with -vv");
}

// ---------------------------------------------------------------------------
// apply, dry run only
// ---------------------------------------------------------------------------

#[test]
fn a_dry_run_apply_changes_nothing_and_says_so() {
    let dir = scratch();
    let output = run_in(dir.path(), &["apply", "minimal", "--dry-run"]);
    assert!(output.status.success(), "{}", stderr_of(&output));

    let text = stdout_of(&output);
    assert!(text.contains("Profile: Minimal (minimal)"));
    assert!(text.contains("Dry run: nothing was changed and nothing was recorded"));
    assert!(text.contains("No security feature will be changed"));

    // Proof: no apply session was recorded.
    let diff = run_in(dir.path(), &["diff"]);
    assert!(stdout_of(&diff).contains("nothing to compare"));
}

#[test]
fn a_dry_run_shows_each_change_with_its_reason_tradeoff_and_undo() {
    let dir = scratch();
    let text = stdout_of(&run_in(dir.path(), &["apply", "minimal", "--dry-run"]));
    assert!(text.contains("Current:"));
    assert!(text.contains("Target:"));
    assert!(text.contains("Risk:"));
    assert!(text.contains("Why:"));
    assert!(text.contains("Tradeoff:"));
    assert!(text.contains("Undo:"));
}

#[test]
fn both_shipped_profiles_dry_run_successfully() {
    for profile in ["minimal", "gaming"] {
        let dir = scratch();
        let output = run_in(dir.path(), &["apply", profile, "--dry-run"]);
        assert!(
            output.status.success(),
            "apply {profile} --dry-run failed: {}",
            stderr_of(&output)
        );
        assert!(stdout_of(&output).contains("Dry run"));
    }
}

#[test]
fn an_unknown_profile_is_refused_by_name() {
    let dir = scratch();
    let output = run_in(dir.path(), &["apply", "turbo", "--dry-run"]);
    assert!(!output.status.success());
    let error = stderr_of(&output);
    assert!(error.contains("turbo"));
    assert!(error.contains("minimal, gaming"));
}

#[test]
fn apply_json_defaults_to_a_dry_run_because_a_prompt_cannot_be_answered() {
    let dir = scratch();
    let output = run_in(dir.path(), &["apply", "minimal", "--json"]);
    assert!(output.status.success(), "{}", stderr_of(&output));

    let parsed: serde_json::Value =
        serde_json::from_str(&stdout_of(&output)).expect("apply --json must be valid JSON");
    assert_eq!(parsed["profile_id"].as_str(), Some("minimal"));
    assert!(parsed["planned"].is_array());

    // Nothing was recorded, so this really was a preview.
    let diff = run_in(dir.path(), &["diff"]);
    assert!(stdout_of(&diff).contains("nothing to compare"));
}

#[test]
fn a_profile_file_that_names_an_unknown_change_is_rejected() {
    let dir = scratch();
    let profile = dir.path().join("hostile.toml");
    std::fs::write(
        &profile,
        r#"schema_version = 1
[profile]
id = "hostile"
name = "Hostile"
description = "Tries to name something MinWin does not implement."
[[changes]]
id = "security.disable_defender"
"#,
    )
    .expect("write profile");

    let output = run_in(
        dir.path(),
        &[
            "apply",
            "--profile-file",
            profile.to_str().expect("path"),
            "--dry-run",
        ],
    );
    assert!(!output.status.success());
    assert!(stderr_of(&output).contains("security.disable_defender"));
}

// ---------------------------------------------------------------------------
// diff and rollback with no history
// ---------------------------------------------------------------------------

#[test]
fn diff_with_no_history_says_so_plainly() {
    let dir = scratch();
    let output = run_in(dir.path(), &["diff"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(stdout_of(&output).contains("has not applied a profile"));
}

#[test]
fn rollback_with_nothing_to_restore_is_a_success_not_an_error() {
    let dir = scratch();
    let output = run_in(dir.path(), &["rollback"]);
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert!(stdout_of(&output).contains("nothing to restore"));
}

#[test]
fn diff_json_with_no_history_is_still_valid_json() {
    let dir = scratch();
    let output = run_in(dir.path(), &["diff", "--json"]);
    assert!(output.status.success());
    let parsed: serde_json::Value = serde_json::from_str(&stdout_of(&output)).expect("valid JSON");
    assert!(parsed["session"].is_null());
}

// ---------------------------------------------------------------------------
// explain
// ---------------------------------------------------------------------------

#[test]
fn explain_lists_the_supported_changes() {
    let dir = scratch();
    let output = run_in(dir.path(), &["explain"]);
    assert!(output.status.success(), "{}", stderr_of(&output));

    let text = stdout_of(&output);
    assert!(text.contains("MinWin supports 4 change(s)"));
    for id in [
        "telemetry.diagtrack_start_type",
        "update.delivery_optimization_download_mode",
        "memory.sysmain_start_type",
        "power.active_plan_high_performance",
    ] {
        assert!(text.contains(id), "explain should list {id}");
    }
}

#[test]
fn explaining_one_change_names_its_documented_mechanism() {
    let dir = scratch();
    let output = run_in(
        dir.path(),
        &["explain", "update.delivery_optimization_download_mode"],
    );
    assert!(output.status.success(), "{}", stderr_of(&output));

    let text = stdout_of(&output);
    assert!(text.contains("DODownloadMode"));
    assert!(text.contains("Why it may help"));
    assert!(text.contains("Tradeoff"));
    assert!(text.contains("Reversible"));
}

#[test]
fn explaining_an_unknown_change_fails_with_its_id() {
    let dir = scratch();
    let output = run_in(dir.path(), &["explain", "registry.magic_tweak"]);
    assert!(!output.status.success());
    assert!(stderr_of(&output).contains("registry.magic_tweak"));
}

// ---------------------------------------------------------------------------
// Non-interactive safety
// ---------------------------------------------------------------------------

#[test]
fn a_real_apply_without_yes_declines_rather_than_proceeding_unattended() {
    // stdin is not a terminal under `cargo test`, so MinWin cannot ask. It
    // must refuse rather than assume consent.
    let dir = scratch();
    let output = run_in(dir.path(), &["apply", "minimal"]);

    let text = stdout_of(&output);
    let combined = format!("{text}{}", stderr_of(&output));
    assert!(
        combined.contains("not a terminal") || combined.contains("Administrator"),
        "expected a refusal or a privilege message, got:\n{combined}"
    );
    assert!(
        !text.contains("applied and verified"),
        "MinWin must not apply anything without consent"
    );

    // And nothing was recorded.
    let diff = run_in(dir.path(), &["diff"]);
    assert!(stdout_of(&diff).contains("nothing to compare"));
}
