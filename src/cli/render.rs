//! Turning reports into text.
//!
//! Every function here takes a finished report and writes to a `Write`. No
//! decisions, no measurements, no system access — so the output can be tested
//! by rendering into a string, and a GUI can ignore this module entirely.
//!
//! Output style: plain ASCII, aligned columns, no emoji, no colour escapes.
//! It has to be readable in a bare `cmd.exe` window and in a pasted log.

use std::fmt::Write as _;

use crate::benchmark::compare::{BenchmarkComparison, Classification};
use crate::benchmark::model::{MetricId, format_bytes};
use crate::changes::model::{Applicability, ChangeMetadata, PlanAction};
use crate::core::clock::format_local;
use crate::engine::apply::{ApplyOutcome, ApplyPlan};
use crate::engine::bench::BenchmarkReport;
use crate::engine::diff::DiffReport;
use crate::engine::rollback::{RestoreAssessment, RollbackOutcome, RollbackPlan};
use crate::engine::status::StatusReport;
use crate::state::models::{ChangeStatus, RollbackStatus};

/// The sentence MinWin prints before every plan, because it is the project's
/// central promise and should not be something a user has to go looking for.
pub const SECURITY_STATEMENT: &str = "No security feature will be changed. MinWin does not touch Windows Defender, the \
     firewall, Windows Update, UAC, SmartScreen, Memory Integrity or credential protections.";

const LABEL_WIDTH: usize = 12;

fn field(out: &mut String, label: &str, value: &str) {
    let _ = writeln!(out, "{label:<LABEL_WIDTH$}{value}");
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3600;
    let minutes = (seconds % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

pub fn status(report: &StatusReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "MinWin {}", report.minwin_version);
    out.push('\n');

    let build = match report.windows_revision {
        Some(revision) => format!("{}.{}", report.windows_build, revision),
        None => report.windows_build.to_string(),
    };
    field(&mut out, "Windows", &report.windows_label);
    field(&mut out, "Build", &build);
    field(&mut out, "Elevated", yes_no(report.elevated));
    if report.centrally_managed {
        field(&mut out, "Managed", &report.management_description);
    }
    field(&mut out, "Uptime", &duration(report.uptime_seconds));
    field(
        &mut out,
        "Memory",
        &format!(
            "{} available of {} ({}% in use)",
            format_bytes(report.memory.available_physical_bytes as f64),
            format_bytes(report.memory.total_physical_bytes as f64),
            report.memory.load_percent
        ),
    );
    out.push('\n');

    match &report.baseline {
        Some(baseline) => field(
            &mut out,
            "Baseline",
            &format!(
                "{} ({} samples, run {})",
                format_local(baseline.recorded_at),
                baseline.sample_count,
                baseline.run_id
            ),
        ),
        None => field(&mut out, "Baseline", "none - run `minwin benchmark`"),
    }
    field(
        &mut out,
        "Benchmarks",
        &report.benchmark_run_count.to_string(),
    );

    match &report.active_profile {
        Some(active) => {
            field(
                &mut out,
                "Profile",
                &format!(
                    "{} ({}, applied {})",
                    active.profile_id,
                    active.status.label(),
                    format_local(active.applied_at)
                ),
            );
        }
        None => field(&mut out, "Profile", "none applied"),
    }
    field(
        &mut out,
        "Changes",
        &format!("{} currently applied by MinWin", report.active_change_count),
    );
    field(
        &mut out,
        "Rollback",
        if report.rollback_available {
            "available"
        } else {
            "nothing to restore"
        },
    );
    out.push('\n');

    field(
        &mut out,
        "Supported",
        &format!("{} changes", report.registered_change_count),
    );
    field(&mut out, "State", &report.database_path);
    field(
        &mut out,
        "Schema",
        &format!("version {}", report.schema_version),
    );

    if report.needs_attention() {
        out.push('\n');
        let _ = writeln!(out, "Attention");
        for session in &report.incomplete_sessions {
            let _ = writeln!(
                out,
                "  Apply session {} ({}) started {} did not finish.",
                session.session_id,
                session.profile_id,
                format_local(session.started_at)
            );
            let _ = writeln!(
                out,
                "  {} change(s) were interrupted. MinWin recorded their original state before \
                 writing, so `minwin diff` will show what the machine looks like now and \
                 `minwin rollback` can still restore them.",
                session.counts.incomplete
            );
        }
    }

    out
}

// ---------------------------------------------------------------------------
// benchmark
// ---------------------------------------------------------------------------

/// The notice shown before sampling starts, so the user understands what the
/// numbers can and cannot mean.
pub fn benchmark_preamble(plan: &crate::benchmark::SamplingPlan) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Measuring {} samples, {}ms apart, after a {}ms settling period (about {}s total).",
        plan.samples,
        plan.interval_ms,
        plan.settle_ms,
        plan.estimated_duration_ms().div_ceil(1000)
    );
    let _ = writeln!(
        out,
        "A running Windows system is never idle, so these readings are affected by whatever \
         else the machine is doing. MinWin reports medians and spread rather than single \
         numbers, and this is a repeatable baseline of system state, not a performance \
         benchmark."
    );
    out
}

pub fn benchmark(report: &BenchmarkReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Benchmark complete (run {})", report.run_id);
    out.push('\n');

    field(&mut out, "Samples", &report.plan.samples.to_string());
    field(
        &mut out,
        "Total RAM",
        &format_bytes(report.total_physical_bytes as f64),
    );
    field(&mut out, "Uptime", &duration(report.uptime_seconds));
    out.push('\n');

    // Widths are generous enough for "21.40 GB" style values plus a
    // "min - max" range, and the range column is left-aligned last so a long
    // value extends the line instead of colliding with the previous column.
    let _ = writeln!(out, "{:<22}{:>12}{:>12}   Range", "Metric", "Median", "IQR");
    for metric in MetricId::ALL {
        let Some(summary) = report.summary.get(metric) else {
            let _ = writeln!(out, "{:<22}{:>12}", metric.label(), "not readable");
            continue;
        };
        let _ = writeln!(
            out,
            "{:<22}{:>12}{:>12}   {} - {}",
            metric.label(),
            metric.format_value(summary.median),
            metric.format_value(summary.interquartile_range),
            metric.format_value(summary.minimum),
            metric.format_value(summary.maximum)
        );
    }

    if !report.unreadable_metrics.is_empty() {
        out.push('\n');
        let _ = writeln!(
            out,
            "Could not read: {}. MinWin records the gap rather than substituting a value.",
            report.unreadable_metrics.join(", ")
        );
    }

    out.push('\n');
    match &report.comparison {
        None => {
            let _ = writeln!(
                out,
                "This is the first recorded run, so there is nothing to compare it with. \
                 Baseline saved."
            );
        }
        Some(comparison) => {
            out.push_str(&comparison_body(comparison));
        }
    }

    if report.stored_run_count > 1 {
        out.push('\n');
        let _ = writeln!(
            out,
            "{} runs stored. Every individual sample is kept, so a more rigorous analysis can \
             be applied to these runs later.",
            report.stored_run_count
        );
    }

    out
}

fn comparison_body(comparison: &BenchmarkComparison) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Compared with run {} from {}:",
        comparison
            .baseline_run_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "?".into()),
        format_local(comparison.baseline_started_at)
    );
    out.push('\n');

    for entry in &comparison.metrics {
        let _ = writeln!(out, "{}", entry.metric.label());
        match entry.classification {
            Classification::InsufficientData => {
                let _ = writeln!(
                    out,
                    "  not enough comparable samples in both runs to say anything"
                );
            }
            Classification::NoClearDifference => {
                let _ = writeln!(
                    out,
                    "  {} median - difference within measurement noise",
                    entry.describe_delta()
                );
                if let Some(threshold) = entry.threshold {
                    let _ = writeln!(
                        out,
                        "  (MinWin would need more than {} before calling this a change)",
                        entry.metric.format_value(threshold)
                    );
                }
            }
            Classification::Unknown => {
                let _ = writeln!(
                    out,
                    "  {} median - MinWin does not interpret a direction for this metric",
                    entry.describe_delta()
                );
            }
            Classification::Improved | Classification::Regressed => {
                let _ = writeln!(
                    out,
                    "  {} median - {}",
                    entry.describe_delta(),
                    entry.classification.label()
                );
            }
        }
        out.push('\n');
    }

    if comparison.windows_build_changed {
        let _ = writeln!(
            out,
            "Note: the Windows build changed between these two runs, so the comparison reflects \
             more than MinWin's changes."
        );
    }

    let _ = writeln!(
        out,
        "{} metric(s) improved, {} regressed. These are coarse thresholds, not a statistical \
         significance test; see docs/benchmarking.md.",
        comparison.improved_count(),
        comparison.regressed_count()
    );
    out
}

// ---------------------------------------------------------------------------
// apply
// ---------------------------------------------------------------------------

pub fn apply_plan(plan: &ApplyPlan, dry_run: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Profile: {} ({})", plan.profile_name, plan.profile_id);
    let _ = writeln!(out, "{}", plan.profile_description);
    if let Some(notes) = &plan.profile_notes {
        out.push('\n');
        for line in notes.trim().lines() {
            let _ = writeln!(out, "  {line}");
        }
    }
    out.push('\n');

    // A dry run shows everything an elevated run would do, so the preview does
    // not depend on how the terminal happened to be launched.
    let shown: Vec<&crate::engine::apply::PlannedChange> = if dry_run {
        plan.all_intended_changes()
    } else {
        plan.planned.iter().collect()
    };

    if shown.is_empty() {
        let _ = writeln!(out, "Nothing to change.");
    } else {
        let _ = writeln!(out, "Planned changes:");
        out.push('\n');
        for (index, change) in shown.iter().enumerate() {
            let blocked = plan
                .blocked_on_elevation
                .iter()
                .any(|other| other.plan.change_id == change.plan.change_id);
            let _ = writeln!(
                out,
                "{:02} {}{}",
                index + 1,
                change.metadata.name,
                if blocked {
                    "   [needs Administrator]"
                } else {
                    ""
                }
            );
            let _ = writeln!(out, "   Current:  {}", change.plan.current.summary);
            let _ = writeln!(out, "   Target:   {}", change.plan.target.summary);
            let _ = writeln!(
                out,
                "   Risk:     {}   Restart: {}   Admin: {}",
                change.metadata.risk.label(),
                change.plan.reboot.label(),
                yes_no(change.metadata.requires_admin)
            );
            let _ = writeln!(out, "   Why:      {}", change.metadata.rationale);
            let _ = writeln!(out, "   Tradeoff: {}", change.metadata.tradeoffs);
            let _ = writeln!(
                out,
                "   Undo:     restores {}",
                change.plan.rollback.summary
            );
            if let Some(note) = &change.profile_note {
                let _ = writeln!(out, "   Note:     {note}");
            }
            out.push('\n');
        }
    }

    // Outside a dry run the blocked changes are not listed above, so name them.
    if !dry_run && plan.needs_elevation() {
        let _ = writeln!(out, "Needs Administrator (not applied in this run):");
        for change in &plan.blocked_on_elevation {
            let _ = writeln!(
                out,
                "   {} - {} would become {}",
                change.metadata.name, change.plan.current.summary, change.plan.target.summary
            );
        }
        out.push('\n');
    }

    if !plan.already_compliant.is_empty() {
        let _ = writeln!(
            out,
            "Already in the target state (nothing will be written):"
        );
        for change in &plan.already_compliant {
            let _ = writeln!(
                out,
                "   {} - {}",
                change.metadata.name, change.plan.current.summary
            );
        }
        out.push('\n');
    }

    if !plan.skipped.is_empty() {
        let _ = writeln!(out, "Skipped:");
        for change in &plan.skipped {
            let _ = writeln!(out, "   {}", change.metadata.name);
            let _ = writeln!(out, "      {}", change.applicability.explain());
            if let Some(error) = &change.inspection_error {
                let _ = writeln!(out, "      {error}");
            }
        }
        out.push('\n');
    }

    if !plan.offered.is_empty() {
        let _ = writeln!(
            out,
            "Available in this profile but switched off (edit the profile to enable):"
        );
        for change in &plan.offered {
            let _ = writeln!(
                out,
                "   {} [{}] - risk {}",
                change.metadata.name,
                change.metadata.id,
                change.metadata.risk.label()
            );
            if let Some(note) = &change.profile_note {
                let _ = writeln!(out, "      {note}");
            }
        }
        out.push('\n');
    }

    let _ = writeln!(out, "{SECURITY_STATEMENT}");

    if plan.needs_elevation() {
        out.push('\n');
        let _ = writeln!(
            out,
            "{} change(s) require Administrator privileges. Reopen your terminal as \
             Administrator and run:",
            plan.blocked_on_elevation.len()
        );
        let _ = writeln!(out, "    minwin apply {}", plan.profile_id);
    }

    if dry_run {
        out.push('\n');
        let _ = writeln!(
            out,
            "Dry run: nothing was changed and nothing was recorded. {} change(s) would be \
             applied.",
            shown.len()
        );
    }

    out
}

pub fn apply_outcome(outcome: &ApplyOutcome) -> String {
    let mut out = String::new();

    for result in &outcome.results {
        let marker = match result.status {
            ChangeStatus::Verified => "ok  ",
            ChangeStatus::Failed => "FAIL",
            _ => "    ",
        };
        let _ = writeln!(out, "[{marker}] {}", result.name);
        match (&result.after, result.status) {
            (Some(after), ChangeStatus::Verified) => {
                let _ = writeln!(out, "         {} -> {}", result.before, after);
            }
            _ => {
                let _ = writeln!(out, "         was {}", result.before);
            }
        }
        if let Some(error) = &result.error {
            let _ = writeln!(out, "         {error}");
        }
        if let Some(note) = &result.note {
            let _ = writeln!(out, "         {note}");
        }
    }

    if !outcome.already_compliant.is_empty() {
        for name in &outcome.already_compliant {
            let _ = writeln!(out, "[    ] {name} - already in the target state");
        }
    }

    out.push('\n');
    let _ = writeln!(
        out,
        "{} change(s) applied and verified. {} failed.",
        outcome.verified_count(),
        outcome.failed_count()
    );

    if outcome.stopped_early {
        let _ = writeln!(
            out,
            "MinWin stopped after the failure rather than applying the remaining changes. \
             Changes already applied are recorded and can be restored with `minwin rollback`."
        );
    }

    let restarting = outcome.needing_restart();
    if !restarting.is_empty() {
        out.push('\n');
        let _ = writeln!(
            out,
            "{} change(s) need a restart to take full effect:",
            restarting.len()
        );
        for change in restarting {
            let _ = writeln!(out, "   {} ({})", change.name, change.reboot.label());
        }
        let _ = writeln!(
            out,
            "MinWin will not restart your machine. Restart when it suits you."
        );
    }

    out.push('\n');
    let _ = writeln!(
        out,
        "Recorded as session {}. Use `minwin diff` to check it, or `minwin rollback` to undo it.",
        outcome.session_id
    );
    out
}

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------

pub fn diff(report: &DiffReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "MinWin diff");
    out.push('\n');
    let _ = writeln!(
        out,
        "Session {} ({}) applied {} - {}",
        report.session_id,
        report.profile_id,
        format_local(report.applied_at),
        report.session_status.label()
    );
    out.push('\n');

    for entry in &report.entries {
        let _ = writeln!(out, "{}", entry.name);
        let _ = writeln!(out, "  before   {}", entry.before);
        let _ = writeln!(
            out,
            "  applied  {}",
            entry.applied.as_deref().unwrap_or("not applied")
        );
        let _ = writeln!(
            out,
            "  current  {}",
            entry.current.as_deref().unwrap_or("could not be read")
        );
        let _ = writeln!(out, "  status   {}", entry.status.label());
        if let Some(error) = &entry.inspection_error {
            let _ = writeln!(out, "           {error}");
        }
        out.push('\n');
    }

    if report.windows_changed() {
        let _ = writeln!(
            out,
            "Note: this session was applied on {} and the machine now reports {}.",
            report.session_windows_label, report.current_windows_label
        );
    }

    let surprising = report.surprising_count();
    if surprising == 0 {
        let _ = writeln!(
            out,
            "Every change MinWin applied is still in place. Rollback would restore all of them."
        );
    } else {
        let _ = writeln!(
            out,
            "{surprising} change(s) no longer match what MinWin applied. `minwin rollback` will \
             flag these and will not overwrite them unless you pass --allow-external-changes."
        );
    }
    out
}

// ---------------------------------------------------------------------------
// rollback
// ---------------------------------------------------------------------------

pub fn rollback_plan(plan: &RollbackPlan, dry_run: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Rolling back session {} ({}), applied {}",
        plan.session_id,
        plan.profile_id,
        format_local(plan.applied_at)
    );
    out.push('\n');

    for (index, candidate) in plan.candidates.iter().enumerate() {
        let _ = writeln!(out, "{:02} {}", index + 1, candidate.name);
        let _ = writeln!(out, "   MinWin set   {}", candidate.minwin_applied);
        let _ = writeln!(
            out,
            "   Current      {}",
            candidate.current.as_deref().unwrap_or("could not be read")
        );
        let _ = writeln!(out, "   Original     {}", candidate.original);
        let _ = writeln!(out, "   Assessment   {}", candidate.assessment.label());
        if let Some(error) = &candidate.inspection_error {
            let _ = writeln!(out, "                {error}");
        }
        if candidate.assessment == RestoreAssessment::ChangedOutsideMinWin {
            let _ = writeln!(
                out,
                "                Restoring this would discard whatever changed it."
            );
        }
        out.push('\n');
    }

    let needing = plan.needing_authorisation();
    if !needing.is_empty() {
        let _ = writeln!(
            out,
            "{} change(s) were modified after MinWin applied them, or cannot be read. MinWin \
             will skip these unless you pass --allow-external-changes.",
            needing.len()
        );
        out.push('\n');
    }

    if plan.restart_requirement().needs_restart() {
        let _ = writeln!(
            out,
            "Some restored settings need a restart to take full effect ({}).",
            plan.restart_requirement().label()
        );
        out.push('\n');
    }

    if dry_run {
        let _ = writeln!(
            out,
            "Dry run: nothing was changed. {} change(s) would be restored.",
            plan.straightforward_count()
        );
    }

    out
}

pub fn rollback_outcome(outcome: &RollbackOutcome) -> String {
    let mut out = String::new();

    for result in &outcome.results {
        let marker = match result.status {
            RollbackStatus::Restored => "ok  ",
            RollbackStatus::AlreadyOriginal => "same",
            RollbackStatus::Skipped => "skip",
            RollbackStatus::Failed => "FAIL",
        };
        let _ = writeln!(out, "[{marker}] {}", result.name);
        if let Some(restored) = &result.restored_to {
            let _ = writeln!(out, "         restored to {restored}");
        }
        if let Some(error) = &result.error {
            let _ = writeln!(out, "         {error}");
        }
        if let Some(note) = &result.note {
            let _ = writeln!(out, "         {note}");
        }
    }

    out.push('\n');
    let _ = writeln!(
        out,
        "{} restored, {} already original, {} skipped, {} failed.",
        outcome.restored_count(),
        outcome
            .results
            .iter()
            .filter(|r| r.status == RollbackStatus::AlreadyOriginal)
            .count(),
        outcome.skipped_count(),
        outcome.failed_count()
    );

    if outcome.session_fully_restored {
        let _ = writeln!(
            out,
            "Session {} is fully restored.",
            outcome.apply_session_id
        );
    } else {
        let _ = writeln!(
            out,
            "Session {} still has changes MinWin has not restored. `minwin diff` shows which.",
            outcome.apply_session_id
        );
    }
    out
}

// ---------------------------------------------------------------------------
// explain
// ---------------------------------------------------------------------------

pub fn explain_all(changes: &[&ChangeMetadata]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "MinWin supports {} change(s). Each one is inspected, verified and reversible.",
        changes.len()
    );
    out.push('\n');
    for metadata in changes {
        let _ = writeln!(out, "{}", metadata.id);
        let _ = writeln!(out, "  {}", metadata.name);
        let _ = writeln!(
            out,
            "  category {}   risk {}   admin {}   {}",
            metadata.category.label(),
            metadata.risk.label(),
            yes_no(metadata.requires_admin),
            metadata.reboot.label()
        );
        out.push('\n');
    }
    let _ = writeln!(out, "Run `minwin explain <id>` for the full reasoning.");
    out.push('\n');
    let _ = writeln!(out, "{SECURITY_STATEMENT}");
    out
}

pub fn explain_one(metadata: &ChangeMetadata) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", metadata.name);
    let _ = writeln!(out, "{}", metadata.id);
    out.push('\n');
    let _ = writeln!(out, "What it does");
    let _ = writeln!(out, "  {}", metadata.description);
    out.push('\n');
    let _ = writeln!(out, "Mechanism");
    let _ = writeln!(out, "  {}", metadata.mechanism);
    out.push('\n');
    let _ = writeln!(out, "Why it may help");
    let _ = writeln!(out, "  {}", metadata.rationale);
    out.push('\n');
    let _ = writeln!(out, "Tradeoff");
    let _ = writeln!(out, "  {}", metadata.tradeoffs);
    out.push('\n');
    field(&mut out, "Category", metadata.category.label());
    field(&mut out, "Risk", metadata.risk.label());
    field(&mut out, "Admin", yes_no(metadata.requires_admin));
    field(&mut out, "Restart", metadata.reboot.label());
    field(
        &mut out,
        "Minimum",
        &format!("Windows build {}", metadata.minimum_build),
    );
    field(&mut out, "Reversible", "yes");
    out
}

/// The confirmation question. Returned rather than printed so the caller owns
/// all terminal interaction.
pub fn confirmation_question(plan: &ApplyPlan) -> String {
    let risk = plan
        .highest_risk()
        .map(|risk| format!(", highest risk {}", risk.label()))
        .unwrap_or_default();
    format!("Apply {} change(s){risk}? [y/N] ", plan.write_count())
}

pub fn rollback_confirmation_question(plan: &RollbackPlan) -> String {
    format!("Restore {} change(s)? [y/N] ", plan.straightforward_count())
}

/// Describes a change's applicability for the JSON and human paths alike.
pub fn applicability_label(applicability: &Applicability) -> String {
    applicability.explain()
}

/// Whether a plan entry would write.
pub fn is_write(action: PlanAction) -> bool {
    action == PlanAction::Modify
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::benchmark::SamplingPlan;
    use crate::changes::ChangeRegistry;
    use crate::core::clock::FixedClock;
    use crate::engine::{
        apply, bench, diff as diff_engine, rollback as rollback_engine, status as status_engine,
    };
    use crate::profiles::load_builtin;
    use crate::state::Database;
    use crate::sys::SystemFacts;
    use crate::sys::fake::{FailOn, FakeMachine};
    use crate::sys::model::ServiceStartType;

    fn clock() -> FixedClock {
        FixedClock::stepping(chrono::Utc::now(), 1)
    }

    fn plan_for(
        machine: &FakeMachine,
        profile_id: &str,
    ) -> (ChangeRegistry, SystemFacts, ApplyPlan) {
        let registry = ChangeRegistry::load();
        let profile = load_builtin(profile_id, &registry).expect("profile");
        let facts = SystemFacts::gather(machine).expect("facts");
        let plan = apply::plan(machine, &facts, &registry, &profile).expect("plan");
        (registry, facts, plan)
    }

    // -- status --------------------------------------------------------------

    #[test]
    fn status_output_shows_real_values_and_says_when_there_is_no_baseline() {
        let machine = FakeMachine::windows_11().elevated();
        let database = Database::open_in_memory().expect("database");
        let facts = SystemFacts::gather(&machine).expect("facts");
        let report = status_engine::status(&facts, &machine, &database, 4).expect("status");
        let text = status(&report);

        assert!(text.starts_with("MinWin 0.1.0"));
        assert!(text.contains("Windows     Windows 11 24H2"));
        assert!(text.contains("Build       26100.1742"));
        assert!(text.contains("Elevated    yes"));
        assert!(text.contains("none - run `minwin benchmark`"));
        assert!(text.contains("none applied"));
        assert!(text.contains("nothing to restore"));
        assert!(text.contains("4 changes"));
        // No emoji, no ANSI escapes.
        assert!(text.is_ascii(), "output must be plain ASCII");
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn status_warns_clearly_about_an_interrupted_session() {
        let machine = FakeMachine::windows_11().elevated();
        let mut database = Database::open_in_memory().expect("database");
        let (_registry, facts, plan) = plan_for(&machine, "minimal");

        // Open a session and leave it unfinished.
        let pending: Vec<crate::state::PendingChange> = plan
            .planned
            .iter()
            .map(|change| crate::state::PendingChange {
                change_id: change.plan.change_id.clone(),
                risk: change.metadata.risk,
                reboot: change.plan.reboot,
                state_before: change.plan.current.clone(),
                state_planned: change.plan.target.clone(),
                rollback: change.plan.rollback.clone(),
                already_compliant: false,
            })
            .collect();
        database
            .begin_apply_session(
                &crate::state::ApplySessionHeader {
                    profile_id: "minimal".into(),
                    profile_name: "Minimal".into(),
                    profile_source: "test".into(),
                    windows_label: "Windows 11 24H2".into(),
                    windows_build: 26100,
                    elevated: true,
                },
                &pending,
                chrono::Utc::now(),
            )
            .expect("begin");

        let report = status_engine::status(&facts, &machine, &database, 4).expect("status");
        let text = status(&report);
        assert!(text.contains("Attention"));
        assert!(text.contains("did not finish"));
        assert!(text.contains("minwin rollback"));
    }

    // -- benchmark -----------------------------------------------------------

    #[test]
    fn the_benchmark_preamble_sets_expectations_before_measuring() {
        let text = benchmark_preamble(&SamplingPlan::default());
        assert!(text.contains("10 samples"));
        assert!(text.contains("never idle"));
        assert!(text.contains("not a performance"));
    }

    #[test]
    fn the_first_benchmark_says_there_is_nothing_to_compare() {
        let machine = FakeMachine::windows_11();
        let mut database = Database::open_in_memory().expect("database");
        let facts = SystemFacts::gather(&machine).expect("facts");
        let report = bench::run_benchmark(
            &machine,
            &facts,
            &mut database,
            SamplingPlan {
                samples: 10,
                interval_ms: 0,
                settle_ms: 0,
            },
            None,
            &clock(),
            &crate::benchmark::InstantPacer,
        )
        .expect("benchmark");

        let text = benchmark(&report);
        assert!(text.contains("Benchmark complete"));
        assert!(text.contains("Available RAM"));
        assert!(text.contains("first recorded run"));
        assert!(text.contains("Baseline saved"));
        // It must not claim any improvement.
        assert!(!text.contains("improved"));
    }

    #[test]
    fn an_unchanged_machine_is_reported_as_within_measurement_noise() {
        // The output that matters most: re-measuring must not read as a win.
        let machine = FakeMachine::windows_11();
        let mut database = Database::open_in_memory().expect("database");
        let facts = SystemFacts::gather(&machine).expect("facts");
        let plan = SamplingPlan {
            samples: 10,
            interval_ms: 0,
            settle_ms: 0,
        };
        bench::run_benchmark(
            &machine,
            &facts,
            &mut database,
            plan,
            None,
            &clock(),
            &crate::benchmark::InstantPacer,
        )
        .expect("first");
        let report = bench::run_benchmark(
            &machine,
            &facts,
            &mut database,
            plan,
            None,
            &clock(),
            &crate::benchmark::InstantPacer,
        )
        .expect("second");

        let text = benchmark(&report);
        assert!(text.contains("difference within measurement noise"));
        assert!(text.contains("0 metric(s) improved, 0 regressed"));
        assert!(text.contains("not a statistical significance test"));
    }

    #[test]
    fn a_neutral_metric_is_shown_without_a_verdict() {
        let mut database = Database::open_in_memory().expect("database");
        let plan = SamplingPlan {
            samples: 10,
            interval_ms: 0,
            settle_ms: 0,
        };
        let before = FakeMachine::windows_11();
        let facts = SystemFacts::gather(&before).expect("facts");
        bench::run_benchmark(
            &before,
            &facts,
            &mut database,
            plan,
            None,
            &clock(),
            &crate::benchmark::InstantPacer,
        )
        .expect("first");

        let after = FakeMachine::windows_11().with_process_count(120);
        let report = bench::run_benchmark(
            &after,
            &facts,
            &mut database,
            plan,
            None,
            &clock(),
            &crate::benchmark::InstantPacer,
        )
        .expect("second");

        let text = benchmark(&report);
        assert!(text.contains("-42"));
        assert!(text.contains("improved"));
    }

    // -- apply ---------------------------------------------------------------

    #[test]
    fn the_plan_shows_current_target_risk_reason_tradeoff_and_undo() {
        let machine = FakeMachine::windows_11().elevated();
        let (_registry, _facts, plan) = plan_for(&machine, "minimal");
        let text = apply_plan(&plan, false);

        assert!(text.contains("Profile: Minimal (minimal)"));
        assert!(text.contains("Planned changes:"));
        assert!(text.contains("Current:"));
        assert!(text.contains("Target:"));
        assert!(text.contains("Risk:"));
        assert!(text.contains("Why:"));
        assert!(text.contains("Tradeoff:"));
        assert!(text.contains("Undo:     restores"));
        // The security promise is always present.
        assert!(text.contains("No security feature will be changed"));
        assert!(text.contains("Defender"));
    }

    #[test]
    fn a_dry_run_says_plainly_that_nothing_was_changed_or_recorded() {
        let machine = FakeMachine::windows_11().elevated();
        let (_registry, _facts, plan) = plan_for(&machine, "minimal");
        let text = apply_plan(&plan, true);
        assert!(text.contains("Dry run: nothing was changed and nothing was recorded"));
        assert!(text.contains("2 change(s) would be applied"));
    }

    #[test]
    fn the_plan_lists_opt_in_changes_separately_from_planned_ones() {
        let machine = FakeMachine::windows_11().elevated();
        let (_registry, _facts, plan) = plan_for(&machine, "minimal");
        let text = apply_plan(&plan, true);
        assert!(text.contains("switched off"));
        assert!(text.contains("memory.sysmain_start_type"));
    }

    #[test]
    fn a_non_elevated_plan_tells_the_user_exactly_what_to_run() {
        let machine = FakeMachine::windows_11();
        let (_registry, _facts, plan) = plan_for(&machine, "minimal");
        let text = apply_plan(&plan, false);
        assert!(text.contains("require Administrator privileges"));
        assert!(text.contains("minwin apply minimal"));
        // The blocked changes are named with their before/after values, not
        // hidden behind the privilege message.
        assert!(text.contains("Needs Administrator (not applied in this run)"));
        assert!(text.contains("would become"));
    }

    #[test]
    fn a_dry_run_without_elevation_still_shows_the_whole_plan() {
        // The point of --dry-run: the preview must not depend on how the
        // terminal was launched.
        let unprivileged = FakeMachine::windows_11();
        let (_registry, _facts, plan) = plan_for(&unprivileged, "minimal");
        let text = apply_plan(&plan, true);

        assert!(text.contains("Planned changes:"));
        assert!(text.contains("Connected User Experiences and Telemetry service"));
        assert!(text.contains("Delivery Optimization peer-to-peer"));
        assert!(text.contains("[needs Administrator]"));
        assert!(text.contains("Current:"));
        assert!(text.contains("Target:"));
        assert!(text.contains("2 change(s) would be applied"));
        assert!(!text.contains("Nothing to change"));
    }

    #[test]
    fn an_elevated_dry_run_does_not_mention_administrator_at_all() {
        let elevated = FakeMachine::windows_11().elevated();
        let (_registry, _facts, plan) = plan_for(&elevated, "minimal");
        let text = apply_plan(&plan, true);
        assert!(!text.contains("[needs Administrator]"));
        assert!(!text.contains("Reopen your terminal"));
        assert!(text.contains("2 change(s) would be applied"));
    }

    #[test]
    fn skipped_changes_are_shown_with_their_reason() {
        let machine =
            FakeMachine::windows_11()
                .elevated()
                .managed(crate::sys::model::ManagementState {
                    domain_joined: true,
                    defender_for_endpoint_onboarded: false,
                });
        let (_registry, _facts, plan) = plan_for(&machine, "minimal");
        let text = apply_plan(&plan, true);
        assert!(text.contains("Skipped:"));
        assert!(text.contains("centrally managed"));
    }

    #[test]
    fn the_confirmation_question_names_the_count_and_the_highest_risk() {
        let machine = FakeMachine::windows_11().elevated();
        let (_registry, _facts, plan) = plan_for(&machine, "minimal");
        let question = confirmation_question(&plan);
        assert_eq!(question, "Apply 2 change(s), highest risk low? [y/N] ");
    }

    #[test]
    fn the_outcome_reports_restart_requirements_without_offering_to_restart() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, plan) = plan_for(&machine, "minimal");
        let mut database = Database::open_in_memory().expect("database");
        let outcome = apply::execute(&machine, &facts, &registry, &mut database, &plan, &clock())
            .expect("execute");

        let text = apply_outcome(&outcome);
        assert!(text.contains("2 change(s) applied and verified. 0 failed."));
        assert!(text.contains("need a restart to take full effect"));
        assert!(text.contains("will not restart your machine"));
        assert!(text.contains("minwin rollback"));
    }

    #[test]
    fn a_failure_is_rendered_as_a_failure_and_explains_the_stop() {
        let machine = FakeMachine::windows_11()
            .elevated()
            .failing(FailOn::ServiceWrite("DiagTrack".into()));
        let (registry, facts, plan) = plan_for(&machine, "minimal");
        let mut database = Database::open_in_memory().expect("database");
        let outcome = apply::execute(&machine, &facts, &registry, &mut database, &plan, &clock())
            .expect("execute");

        let text = apply_outcome(&outcome);
        assert!(text.contains("[FAIL]"));
        assert!(text.contains("Access is denied"));
        assert!(text.contains("stopped after the failure"));
        assert!(text.contains("1 change(s) applied and verified. 1 failed."));
    }

    // -- diff ----------------------------------------------------------------

    fn applied_state(machine: &FakeMachine) -> (ChangeRegistry, SystemFacts, Database) {
        let (registry, facts, plan) = plan_for(machine, "minimal");
        let mut database = Database::open_in_memory().expect("database");
        apply::execute(machine, &facts, &registry, &mut database, &plan, &clock())
            .expect("execute");
        (registry, facts, database)
    }

    #[test]
    fn a_clean_diff_says_everything_is_still_in_place() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, database) = applied_state(&machine);
        let report = diff_engine::diff_latest(&machine, &facts, &registry, &database)
            .expect("diff")
            .expect("report");

        let text = diff(&report);
        assert!(text.contains("MinWin diff"));
        assert!(text.contains("before   Automatic"));
        assert!(text.contains("applied  Manual"));
        assert!(text.contains("current  Manual"));
        assert!(text.contains("unchanged since apply"));
        assert!(text.contains("still in place"));
    }

    #[test]
    fn a_diff_with_an_external_change_points_at_the_authorisation_flag() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, database) = applied_state(&machine);
        machine.change_service_externally("DiagTrack", ServiceStartType::Disabled);

        let report = diff_engine::diff_latest(&machine, &facts, &registry, &database)
            .expect("diff")
            .expect("report");
        let text = diff(&report);
        assert!(text.contains("changed outside MinWin"));
        assert!(text.contains("--allow-external-changes"));
    }

    // -- rollback ------------------------------------------------------------

    #[test]
    fn the_rollback_plan_shows_all_three_values_for_each_change() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, database) = applied_state(&machine);
        let plan = rollback_engine::plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");

        let text = rollback_plan(&plan, false);
        assert!(text.contains("MinWin set   Manual"));
        assert!(text.contains("Current      Manual"));
        assert!(text.contains("Original     Automatic"));
        assert!(text.contains("safe to restore"));
    }

    #[test]
    fn a_rollback_plan_with_external_changes_warns_before_anything_is_written() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, database) = applied_state(&machine);
        machine.change_service_externally("DiagTrack", ServiceStartType::Disabled);

        let plan = rollback_engine::plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");
        let text = rollback_plan(&plan, false);
        assert!(text.contains("changed outside MinWin"));
        assert!(text.contains("would discard whatever changed it"));
        assert!(text.contains("--allow-external-changes"));
    }

    #[test]
    fn the_rollback_outcome_explains_a_deleted_policy_value() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, mut database) = applied_state(&machine);
        let plan = rollback_engine::plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");
        let outcome = rollback_engine::execute(
            &machine,
            &facts,
            &registry,
            &mut database,
            &plan,
            rollback_engine::RollbackAuthorisation::default(),
            &clock(),
        )
        .expect("rollback");

        let text = rollback_outcome(&outcome);
        assert!(text.contains("2 restored"));
        assert!(text.contains("fully restored"));
        assert!(text.contains("Windows' own default applies again"));
    }

    #[test]
    fn a_skipped_rollback_is_shown_as_skipped_not_as_success() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, mut database) = applied_state(&machine);
        machine.change_service_externally("DiagTrack", ServiceStartType::Disabled);

        let plan = rollback_engine::plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");
        let outcome = rollback_engine::execute(
            &machine,
            &facts,
            &registry,
            &mut database,
            &plan,
            rollback_engine::RollbackAuthorisation::default(),
            &clock(),
        )
        .expect("rollback");

        let text = rollback_outcome(&outcome);
        assert!(text.contains("[skip]"));
        assert!(text.contains("1 skipped"));
        assert!(text.contains("has not restored"));
    }

    // -- explain -------------------------------------------------------------

    #[test]
    fn explain_lists_every_registered_change() {
        let registry = ChangeRegistry::load();
        let metadata: Vec<&ChangeMetadata> =
            registry.iter().map(|change| change.metadata()).collect();
        let text = explain_all(&metadata);

        assert!(text.contains("MinWin supports 4 change(s)"));
        for change in registry.iter() {
            assert!(text.contains(change.id()), "{} missing", change.id());
        }
        assert!(text.contains("No security feature will be changed"));
    }

    #[test]
    fn explaining_one_change_covers_mechanism_reason_and_tradeoff() {
        let registry = ChangeRegistry::load();
        let change = registry
            .get(crate::changes::DIAGTRACK_START_TYPE)
            .expect("change");
        let text = explain_one(change.metadata());

        assert!(text.contains("Mechanism"));
        assert!(text.contains("ChangeServiceConfigW"));
        assert!(text.contains("Why it may help"));
        assert!(text.contains("Tradeoff"));
        assert!(text.contains("Reversible  yes"));
    }

    // -- formatting helpers ---------------------------------------------------

    #[test]
    fn durations_render_sensibly_across_scales() {
        assert_eq!(duration(0), "0m");
        assert_eq!(duration(90), "1m");
        assert_eq!(duration(3 * 3600 + 25 * 60), "3h 25m");
        assert_eq!(duration(2 * 86_400 + 3 * 3600), "2d 3h 0m");
    }
}
