//! End-to-end workflow tests.
//!
//! These drive the full product loop — benchmark, dry-run, apply, diff,
//! rollback — through the same engine functions the CLI calls, against a fake
//! machine and an in-memory database.
//!
//! None of this touches the developer's Windows installation. The fake machine
//! records every mutation, and several tests assert that the recorded list is
//! *empty*, which is how "`cargo test` does not modify my machine" is enforced
//! rather than merely intended.

use minwin::benchmark::compare::Classification;
use minwin::benchmark::{InstantPacer, MetricId, SamplingPlan};
use minwin::changes::{
    ChangeRegistry, DELIVERY_OPTIMIZATION_DOWNLOAD_MODE, DIAGTRACK_START_TYPE,
    POWER_PLAN_HIGH_PERFORMANCE, SYSMAIN_START_TYPE,
};
use minwin::cli::render;
use minwin::core::clock::FixedClock;
use minwin::engine::diff::DiffStatus;
use minwin::engine::rollback::{RestoreAssessment, RollbackAuthorisation};
use minwin::engine::{apply, bench, diff, rollback, status};
use minwin::profiles;
use minwin::state::Database;
use minwin::state::models::{ChangeStatus, SessionStatus};
use minwin::sys::SystemFacts;
use minwin::sys::fake::{FailOn, FakeMachine};
use minwin::sys::model::{PowerSchemeId, RegistryRoot, ServiceStartType};

const DO_SUBKEY: &str = r"SOFTWARE\Policies\Microsoft\Windows\DeliveryOptimization";
const DO_VALUE: &str = "DODownloadMode";

fn clock() -> FixedClock {
    FixedClock::stepping(chrono::Utc::now(), 1)
}

fn fast_plan(samples: u32) -> SamplingPlan {
    SamplingPlan {
        samples,
        interval_ms: 0,
        settle_ms: 0,
    }
}

/// The context a MinWin command operates in.
struct Harness {
    machine: FakeMachine,
    registry: ChangeRegistry,
    facts: SystemFacts,
    database: Database,
}

impl Harness {
    fn new(machine: FakeMachine) -> Self {
        let facts = SystemFacts::gather(&machine).expect("facts");
        Self {
            machine,
            registry: ChangeRegistry::load(),
            facts,
            database: Database::open_in_memory().expect("database"),
        }
    }

    fn elevated() -> Self {
        Self::new(FakeMachine::windows_11().elevated())
    }

    fn benchmark(&mut self, samples: u32) -> bench::BenchmarkReport {
        bench::run_benchmark(
            &self.machine,
            &self.facts,
            &mut self.database,
            fast_plan(samples),
            None,
            &clock(),
            &InstantPacer,
        )
        .expect("benchmark")
    }

    fn plan(&self, profile_id: &str) -> apply::ApplyPlan {
        let profile = profiles::load_builtin(profile_id, &self.registry).expect("profile");
        apply::plan(&self.machine, &self.facts, &self.registry, &profile).expect("plan")
    }

    fn apply(&mut self, profile_id: &str) -> apply::ApplyOutcome {
        let plan = self.plan(profile_id);
        apply::execute(
            &self.machine,
            &self.facts,
            &self.registry,
            &mut self.database,
            &plan,
            &clock(),
        )
        .expect("apply")
    }

    fn diff(&self) -> Option<diff::DiffReport> {
        diff::diff_latest(&self.machine, &self.facts, &self.registry, &self.database).expect("diff")
    }

    fn rollback_plan(&self) -> Option<rollback::RollbackPlan> {
        rollback::plan_latest(&self.machine, &self.facts, &self.registry, &self.database)
            .expect("rollback plan")
    }

    fn rollback(&mut self, authorisation: RollbackAuthorisation) -> rollback::RollbackOutcome {
        let plan = self.rollback_plan().expect("something to roll back");
        rollback::execute(
            &self.machine,
            &self.facts,
            &self.registry,
            &mut self.database,
            &plan,
            authorisation,
            &clock(),
        )
        .expect("rollback")
    }

    fn status(&self) -> status::StatusReport {
        status::status(
            &self.facts,
            &self.machine,
            &self.database,
            self.registry.len(),
        )
        .expect("status")
    }
}

// ---------------------------------------------------------------------------
// The full documented workflow
// ---------------------------------------------------------------------------

#[test]
fn the_documented_workflow_runs_end_to_end_and_leaves_the_machine_as_it_started() {
    let mut harness = Harness::elevated();

    // The original state, captured so the test can assert MinWin restored it
    // exactly rather than approximately.
    let original_service = harness.machine.service_start_type("DiagTrack");
    let original_policy =
        harness
            .machine
            .registry_dword(RegistryRoot::LocalMachine, DO_SUBKEY, DO_VALUE);
    assert_eq!(original_service, Some(ServiceStartType::Automatic));
    assert_eq!(original_policy, None, "the policy starts unconfigured");

    // 1. minwin benchmark
    let baseline = harness.benchmark(10);
    assert!(baseline.is_first_run());
    assert!(
        harness.machine.writes().is_empty(),
        "benchmarking must not write"
    );

    // 2. minwin apply minimal --dry-run
    let dry = harness.plan("minimal");
    assert_eq!(dry.write_count(), 2);
    assert!(
        harness.machine.writes().is_empty(),
        "a dry run must not write"
    );
    assert_eq!(
        harness.database.latest_apply_session().expect("session"),
        None,
        "a dry run must not record a session"
    );

    // 3. minwin apply minimal
    let applied = harness.apply("minimal");
    assert_eq!(applied.status, SessionStatus::Completed);
    assert_eq!(applied.verified_count(), 2);
    assert_eq!(
        harness.machine.service_start_type("DiagTrack"),
        Some(ServiceStartType::Manual)
    );
    assert_eq!(
        harness
            .machine
            .registry_dword(RegistryRoot::LocalMachine, DO_SUBKEY, DO_VALUE),
        Some(0)
    );

    // 4. minwin benchmark (again)
    let after = harness.benchmark(10);
    assert!(!after.is_first_run());
    // The fake machine's readings did not change, so MinWin must not claim a
    // win just because a profile was applied in between.
    let comparison = after.comparison.expect("comparison");
    assert_eq!(comparison.improved_count(), 0);
    assert_eq!(comparison.regressed_count(), 0);

    // 5. minwin diff
    let report = harness.diff().expect("a diff report");
    assert_eq!(report.entries.len(), 2);
    assert_eq!(report.surprising_count(), 0);
    for entry in &report.entries {
        assert_eq!(entry.status, DiffStatus::UnchangedSinceApply);
    }

    // minwin status, mid-workflow
    let state = harness.status();
    assert_eq!(state.active_change_count, 2);
    assert!(state.rollback_available);
    assert!(state.has_baseline());
    assert_eq!(
        state.active_profile.as_ref().map(|p| p.profile_id.as_str()),
        Some("minimal")
    );

    // 6. minwin rollback
    let restored = harness.rollback(RollbackAuthorisation::default());
    assert_eq!(restored.restored_count(), 2);
    assert_eq!(restored.failed_count(), 0);
    assert!(restored.session_fully_restored);

    // The machine is byte-for-byte where it started, including the policy
    // value being absent rather than zero.
    assert_eq!(
        harness.machine.service_start_type("DiagTrack"),
        original_service
    );
    assert_eq!(
        harness
            .machine
            .registry_dword(RegistryRoot::LocalMachine, DO_SUBKEY, DO_VALUE),
        original_policy
    );

    // And MinWin knows there is nothing left to do.
    let final_state = harness.status();
    assert_eq!(final_state.active_change_count, 0);
    assert!(!final_state.rollback_available);
    assert!(!final_state.needs_attention());
    assert_eq!(harness.rollback_plan(), None);
}

#[test]
fn the_gaming_workflow_also_restores_the_power_plan() {
    let mut harness = Harness::elevated();
    let original_plan = harness.machine.active_power_scheme_id();

    let applied = harness.apply("gaming");
    assert_eq!(applied.verified_count(), 3);
    assert_eq!(
        harness.machine.active_power_scheme_id(),
        PowerSchemeId::parse("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c")
    );

    harness.rollback(RollbackAuthorisation::default());
    assert_eq!(harness.machine.active_power_scheme_id(), original_plan);
}

// ---------------------------------------------------------------------------
// Dry run
// ---------------------------------------------------------------------------

#[test]
fn a_dry_run_is_identical_to_the_real_plan_but_changes_nothing() {
    let mut harness = Harness::elevated();

    let preview = harness.plan("minimal");
    let preview_text = render::apply_plan(&preview, true);

    // Running for real must apply exactly what the preview described.
    let applied = harness.apply("minimal");
    let applied_ids: Vec<&str> = applied
        .results
        .iter()
        .map(|result| result.change_id.as_str())
        .collect();
    let previewed_ids: Vec<&str> = preview
        .planned
        .iter()
        .map(|change| change.plan.change_id.as_str())
        .collect();
    assert_eq!(applied_ids, previewed_ids);
    assert!(preview_text.contains("nothing was changed and nothing was recorded"));
}

#[test]
fn a_dry_run_without_elevation_still_previews_every_change() {
    // Inspection needs no privileges, so the preview must be complete even
    // from an ordinary terminal.
    let harness = Harness::new(FakeMachine::windows_11());
    let plan = harness.plan("minimal");

    assert_eq!(plan.all_intended_changes().len(), 2);
    assert!(plan.needs_elevation());
    assert!(plan.skipped.is_empty());
    for change in plan.all_intended_changes() {
        assert!(!change.plan.current.summary.is_empty());
        assert!(!change.plan.target.summary.is_empty());
    }
    assert!(harness.machine.writes().is_empty());
}

// ---------------------------------------------------------------------------
// Applicability
// ---------------------------------------------------------------------------

#[test]
fn a_managed_device_keeps_its_telemetry_service_and_says_why() {
    let harness = Harness::new(FakeMachine::windows_11().elevated().managed(
        minwin::sys::model::ManagementState {
            domain_joined: true,
            defender_for_endpoint_onboarded: true,
        },
    ));
    let plan = harness.plan("minimal");

    let planned: Vec<&str> = plan
        .planned
        .iter()
        .map(|change| change.plan.change_id.as_str())
        .collect();
    assert!(!planned.contains(&DIAGTRACK_START_TYPE));

    let skipped = plan
        .skipped
        .iter()
        .find(|change| change.metadata.id == DIAGTRACK_START_TYPE)
        .expect("the telemetry change must be skipped with a reason");
    let reason = skipped.applicability.explain();
    assert!(reason.contains("centrally managed"));
    assert!(reason.contains("Defender for Endpoint"));
}

#[test]
fn a_machine_without_the_target_power_plan_skips_it_and_applies_the_rest() {
    let high = PowerSchemeId::parse("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c").expect("guid");
    let mut harness = Harness::new(
        FakeMachine::windows_11()
            .elevated()
            .without_power_scheme(&high),
    );

    let applied = harness.apply("gaming");
    assert_eq!(applied.verified_count(), 2);
    assert!(
        !applied
            .results
            .iter()
            .any(|result| result.change_id == POWER_PLAN_HIGH_PERFORMANCE)
    );
}

#[test]
fn a_missing_service_is_skipped_rather_than_failing_the_session() {
    let mut harness = Harness::new(
        FakeMachine::windows_11()
            .elevated()
            .without_service("DiagTrack"),
    );
    let applied = harness.apply("minimal");

    assert_eq!(applied.status, SessionStatus::Completed);
    assert_eq!(applied.verified_count(), 1);
    assert_eq!(applied.failed_count(), 0);
}

#[test]
fn an_unsupported_windows_build_blocks_every_change() {
    let mut harness = Harness::elevated();
    // Windows 10, which MinWin has not validated these changes against.
    harness.facts.windows.build = 19045;

    let profile = profiles::load_builtin("minimal", &harness.registry).expect("profile");
    let plan = apply::plan(
        &harness.machine,
        &harness.facts,
        &harness.registry,
        &profile,
    )
    .expect("plan");

    assert!(!plan.has_work());
    assert_eq!(plan.skipped.len(), 2);
    for skipped in &plan.skipped {
        assert!(skipped.applicability.explain().contains("19045"));
    }
}

// ---------------------------------------------------------------------------
// Failure and crash handling
// ---------------------------------------------------------------------------

#[test]
fn a_failed_change_stops_the_session_but_earlier_work_stays_reversible() {
    let mut harness = Harness::new(
        FakeMachine::windows_11()
            .elevated()
            .failing(FailOn::ServiceWrite("DiagTrack".into())),
    );

    let applied = harness.apply("minimal");
    assert_eq!(applied.status, SessionStatus::CompletedWithFailures);
    assert!(applied.stopped_early);
    assert_eq!(applied.verified_count(), 1);
    assert_eq!(applied.failed_count(), 1);

    // The change that did succeed is recorded and reversible.
    let plan = harness.rollback_plan().expect("something to roll back");
    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(
        plan.candidates[0].change_id,
        DELIVERY_OPTIMIZATION_DOWNLOAD_MODE
    );

    let restored = harness.rollback(RollbackAuthorisation::default());
    assert_eq!(restored.restored_count(), 1);
    assert_eq!(
        harness
            .machine
            .registry_dword(RegistryRoot::LocalMachine, DO_SUBKEY, DO_VALUE),
        None
    );

    // The failed change never altered the machine, so the diff says so.
    let report = harness.diff().expect("diff");
    let failed = report
        .entries
        .iter()
        .find(|entry| entry.change_id == DIAGTRACK_START_TYPE)
        .expect("entry");
    assert_eq!(failed.status, DiffStatus::NotApplied);
}

#[test]
fn an_interrupted_apply_is_detected_and_remains_fully_reversible() {
    // The crash scenario: the write happened, MinWin died before recording the
    // outcome. Because rollback data is committed before any write, the change
    // is still restorable.
    let mut harness = Harness::elevated();
    let plan = harness.plan("minimal");

    let pending: Vec<minwin::state::PendingChange> = plan
        .planned
        .iter()
        .map(|change| minwin::state::PendingChange {
            change_id: change.plan.change_id.clone(),
            risk: change.metadata.risk,
            reboot: change.plan.reboot,
            state_before: change.plan.current.clone(),
            state_planned: change.plan.target.clone(),
            rollback: change.plan.rollback.clone(),
            already_compliant: false,
        })
        .collect();

    let session = harness
        .database
        .begin_apply_session(
            &minwin::state::ApplySessionHeader {
                profile_id: "minimal".into(),
                profile_name: "Minimal".into(),
                profile_source: "built-in profile 'minimal'".into(),
                windows_label: "Windows 11 24H2".into(),
                windows_build: 26100,
                elevated: true,
            },
            &pending,
            chrono::Utc::now(),
        )
        .expect("begin session");

    harness
        .database
        .mark_change_applying(session, DIAGTRACK_START_TYPE)
        .expect("mark applying");
    // The write lands, then the process dies.
    harness
        .machine
        .change_service_externally("DiagTrack", ServiceStartType::Manual);

    // `minwin status` must surface this rather than looking healthy.
    let state = harness.status();
    assert!(state.needs_attention());
    assert_eq!(state.incomplete_sessions.len(), 1);
    assert!(state.rollback_available);

    // And the change is still recorded as needing a rollback.
    let records = harness
        .database
        .change_records(session)
        .expect("change records");
    let interrupted = records
        .iter()
        .find(|record| record.change_id == DIAGTRACK_START_TYPE)
        .expect("record");
    assert_eq!(interrupted.status, ChangeStatus::Applying);
    assert!(interrupted.status.is_restorable());
    assert_eq!(interrupted.rollback.summary, "Automatic");

    let restored = harness.rollback(RollbackAuthorisation::default());
    assert!(restored.restored_count() >= 1);
    assert_eq!(
        harness.machine.service_start_type("DiagTrack"),
        Some(ServiceStartType::Automatic)
    );
}

// ---------------------------------------------------------------------------
// External modification
// ---------------------------------------------------------------------------

#[test]
fn a_change_made_outside_minwin_is_detected_and_not_silently_overwritten() {
    let mut harness = Harness::elevated();
    harness.apply("minimal");

    // Somebody sets the service to something MinWin never chose.
    harness
        .machine
        .change_service_externally("DiagTrack", ServiceStartType::Disabled);

    // diff reports it.
    let report = harness.diff().expect("diff");
    let entry = report
        .entries
        .iter()
        .find(|entry| entry.change_id == DIAGTRACK_START_TYPE)
        .expect("entry");
    assert_eq!(entry.status, DiffStatus::ChangedOutsideMinWin);
    assert_eq!(report.surprising_count(), 1);

    // rollback flags it and needs explicit authorisation.
    let plan = harness.rollback_plan().expect("plan");
    assert!(plan.requires_explicit_authorisation());
    let flagged = plan
        .candidates
        .iter()
        .find(|candidate| candidate.change_id == DIAGTRACK_START_TYPE)
        .expect("candidate");
    assert_eq!(flagged.assessment, RestoreAssessment::ChangedOutsideMinWin);
    assert_eq!(flagged.minwin_applied, "Manual");
    assert_eq!(flagged.current.as_deref(), Some("Disabled"));
    assert_eq!(flagged.original, "Automatic");

    // Without that authorisation the external value survives untouched.
    let outcome = harness.rollback(RollbackAuthorisation::default());
    assert_eq!(outcome.skipped_count(), 1);
    assert_eq!(
        harness.machine.service_start_type("DiagTrack"),
        Some(ServiceStartType::Disabled),
        "MinWin must not discard somebody else's change without being asked"
    );
}

#[test]
fn explicit_authorisation_restores_an_externally_changed_value() {
    let mut harness = Harness::elevated();
    harness.apply("minimal");
    harness
        .machine
        .change_service_externally("DiagTrack", ServiceStartType::Disabled);

    let outcome = harness.rollback(RollbackAuthorisation {
        allow_external_changes: true,
    });
    assert_eq!(outcome.restored_count(), 2);
    assert_eq!(outcome.skipped_count(), 0);
    assert_eq!(
        harness.machine.service_start_type("DiagTrack"),
        Some(ServiceStartType::Automatic)
    );
}

#[test]
fn a_value_someone_already_reverted_is_reported_and_not_rewritten() {
    let mut harness = Harness::elevated();
    harness.apply("minimal");
    harness
        .machine
        .change_service_externally("DiagTrack", ServiceStartType::Automatic);

    let report = harness.diff().expect("diff");
    let entry = report
        .entries
        .iter()
        .find(|entry| entry.change_id == DIAGTRACK_START_TYPE)
        .expect("entry");
    assert_eq!(entry.status, DiffStatus::RevertedOutsideMinWin);

    let writes_before = harness.machine.writes().len();
    let outcome = harness.rollback(RollbackAuthorisation::default());
    assert!(outcome.session_fully_restored);
    // Only the policy value needed writing.
    assert_eq!(harness.machine.writes().len(), writes_before + 1);
}

// ---------------------------------------------------------------------------
// Idempotence and repetition
// ---------------------------------------------------------------------------

#[test]
fn applying_the_same_profile_twice_writes_nothing_the_second_time() {
    let mut harness = Harness::elevated();
    harness.apply("minimal");
    let writes_after_first = harness.machine.writes().len();

    let second = harness.apply("minimal");
    assert_eq!(second.verified_count(), 0);
    assert_eq!(second.already_compliant.len(), 2);
    assert_eq!(
        harness.machine.writes().len(),
        writes_after_first,
        "a second apply must be a no-op"
    );
}

#[test]
fn rolling_back_twice_is_safe() {
    let mut harness = Harness::elevated();
    harness.apply("minimal");
    harness.rollback(RollbackAuthorisation::default());

    assert_eq!(harness.rollback_plan(), None);
    assert!(!harness.status().rollback_available);
}

#[test]
fn applying_minimal_then_gaming_leaves_both_sessions_recorded_and_reversible() {
    let mut harness = Harness::elevated();
    harness.apply("minimal");
    let gaming = harness.apply("gaming");

    // Only the power plan is left to change; the shared two are compliant.
    assert_eq!(gaming.verified_count(), 1);
    assert_eq!(gaming.already_compliant.len(), 2);

    // Rolling back reverses the most recent session first.
    let outcome = harness.rollback(RollbackAuthorisation::default());
    assert_eq!(outcome.apply_session_id, gaming.session_id);
    assert_eq!(
        harness.machine.active_power_scheme_id(),
        PowerSchemeId::parse("381b4222-f694-41f0-9685-ff5bb260df2e")
    );

    // The earlier session's changes are still in place and still reversible.
    assert_eq!(
        harness.machine.service_start_type("DiagTrack"),
        Some(ServiceStartType::Manual)
    );
    let remaining = harness.rollback_plan().expect("the earlier session");
    assert_eq!(remaining.candidates.len(), 2);

    harness.rollback(RollbackAuthorisation::default());
    assert_eq!(
        harness.machine.service_start_type("DiagTrack"),
        Some(ServiceStartType::Automatic)
    );
}

// ---------------------------------------------------------------------------
// Benchmarking honesty
// ---------------------------------------------------------------------------

#[test]
fn benchmarking_an_unchanged_machine_never_reports_an_improvement() {
    let mut harness = Harness::elevated();
    harness.benchmark(10);

    // Five more runs on an identical machine. Not one may claim a win.
    for _ in 0..5 {
        let report = harness.benchmark(10);
        let comparison = report.comparison.expect("comparison");
        assert_eq!(comparison.improved_count(), 0);
        assert_eq!(comparison.regressed_count(), 0);
        for metric in &comparison.metrics {
            assert_eq!(metric.classification, Classification::NoClearDifference);
        }
    }
}

#[test]
fn a_neutral_metric_is_never_classified_as_better_or_worse() {
    let mut harness = Harness::new(FakeMachine::windows_11());
    harness.benchmark(10);

    // Change the running-service count substantially.
    let mut harness = Harness {
        machine: FakeMachine::windows_11(),
        ..harness
    };
    let report = harness.benchmark(10);
    let services = report
        .comparison
        .expect("comparison")
        .get(MetricId::ServiceRunningCount)
        .cloned()
        .expect("services");
    assert_ne!(services.classification, Classification::Improved);
    assert_ne!(services.classification, Classification::Regressed);
}

#[test]
fn raw_samples_survive_so_a_better_analysis_can_be_applied_later() {
    let mut harness = Harness::elevated();
    let report = harness.benchmark(12);

    let stored = harness
        .database
        .benchmark_run(report.run_id)
        .expect("stored run");
    assert_eq!(stored.samples.len(), 12 * MetricId::ALL.len());
    assert_eq!(stored.plan.samples, 12);
    assert_eq!(stored.environment.windows_build, 26100);
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

#[test]
fn the_shipped_profiles_only_reference_registered_changes() {
    let registry = ChangeRegistry::load();
    for profile_id in profiles::BUILT_IN_PROFILE_IDS {
        let profile = profiles::load_builtin(profile_id, &registry).expect(profile_id);
        for selection in &profile.changes {
            assert!(
                registry.get(&selection.id).is_some(),
                "profile {profile_id} references unregistered change {}",
                selection.id
            );
        }
    }
}

#[test]
fn neither_shipped_profile_enables_a_medium_risk_change() {
    let registry = ChangeRegistry::load();
    for profile_id in profiles::BUILT_IN_PROFILE_IDS {
        let profile = profiles::load_builtin(profile_id, &registry).expect(profile_id);
        for id in profile.enabled_change_ids() {
            let change = registry.get(id).expect("registered");
            assert_eq!(
                change.metadata().risk,
                minwin::changes::Risk::Low,
                "profile {profile_id} enables {id}, which is not low risk"
            );
        }
        assert!(!profile.enabled_change_ids().contains(&SYSMAIN_START_TYPE));
    }
}

#[test]
fn a_hostile_profile_cannot_make_minwin_do_anything_new() {
    // A profile's whole vocabulary is "enable or disable a known change".
    let registry = ChangeRegistry::load();
    for hostile in [
        r#"schema_version = 1
[profile]
id = "evil"
name = "Evil"
description = "d"
[[changes]]
id = "telemetry.diagtrack_start_type"
command = "powershell -c Remove-Item C:\\ -Recurse""#,
        r#"schema_version = 1
[profile]
id = "evil"
name = "Evil"
description = "d"
[[changes]]
id = "security.disable_defender""#,
        r#"schema_version = 1
[profile]
id = "evil"
name = "Evil"
description = "d"
[[changes]]
id = "telemetry.diagtrack_start_type"
service = "WinDefend""#,
    ] {
        assert!(
            profiles::parse(hostile, "hostile.toml", &registry).is_err(),
            "this profile should have been rejected:\n{hostile}"
        );
    }
}

// ---------------------------------------------------------------------------
// The guarantee that gives this file its reason to exist
// ---------------------------------------------------------------------------

#[test]
fn read_only_commands_never_write_to_the_machine() {
    let mut harness = Harness::elevated();

    harness.benchmark(10);
    let _ = harness.plan("minimal");
    let _ = harness.plan("gaming");
    let _ = harness.status();
    let _ = harness.diff();
    let _ = harness.rollback_plan();

    assert!(
        harness.machine.writes().is_empty(),
        "status, benchmark, diff and planning must all be read-only; saw {:?}",
        harness.machine.writes()
    );
}

#[test]
fn every_write_minwin_performs_comes_from_an_applied_change() {
    let mut harness = Harness::elevated();
    harness.apply("gaming");

    // Exactly three writes, each attributable to a registered change.
    let writes = harness.machine.writes();
    assert_eq!(writes.len(), 3, "unexpected writes: {writes:?}");
    assert!(writes.iter().any(|write| write.starts_with("power:")));
    assert!(writes.iter().any(|write| write.starts_with("registry:")));
    assert!(
        writes
            .iter()
            .any(|write| write == "service:DiagTrack=Manual")
    );
}
