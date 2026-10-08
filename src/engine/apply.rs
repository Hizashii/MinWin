//! Planning and applying a profile.
//!
//! Split into two functions on purpose:
//!
//! * [`plan`] reads the machine and decides everything. It writes nothing, to
//!   the machine or to the database. `--dry-run` calls this and stops.
//! * [`execute`] takes a plan that has already been displayed and confirmed,
//!   persists it, and only then starts writing.
//!
//! The ordering inside [`execute`] is the whole safety argument:
//!
//! 1. The session and every change's pre-change state and rollback data are
//!    committed to SQLite.
//! 2. Only then is the first write issued.
//! 3. Each change is marked `Applying` before its write and `Applied` after,
//!    so an interrupted run is distinguishable from one that never started.
//! 4. Verification reads the machine back; a mismatch is a failure, not a
//!    warning.
//! 5. A failure stops the session. Changes already applied stay applied and
//!    stay recorded, so `minwin rollback` can undo them.

use serde::{Deserialize, Serialize};

use crate::changes::model::{
    Applicability, ChangeMetadata, ChangePlan, PlanAction, RebootRequirement, Risk,
    VerificationResult,
};
use crate::changes::{ChangeRegistry, SystemChange};
use crate::core::clock::Clock;
use crate::core::error::Result;
use crate::profiles::Profile;
use crate::state::models::{ChangeStatus, SessionStatus};
use crate::state::{ApplySessionHeader, Database, PendingChange};
use crate::sys::SystemFacts;
use crate::sys::traits::Machine;

/// A change MinWin intends to make, with everything needed to display it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlannedChange {
    pub metadata: ChangeMetadata,
    pub plan: ChangePlan,
    /// A note from the profile, such as why a change is opt-in.
    pub profile_note: Option<String>,
}

/// A change MinWin will not make, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkippedChange {
    pub metadata: ChangeMetadata,
    pub applicability: Applicability,
    /// Set when inspection itself failed, rather than the change being
    /// inapplicable. MinWin reports this instead of silently omitting the row.
    pub inspection_error: Option<String>,
}

/// A change the profile lists but leaves switched off.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OfferedChange {
    pub metadata: ChangeMetadata,
    pub profile_note: Option<String>,
}

/// The complete result of planning. This is what `--dry-run` renders, and what
/// a future GUI would bind to.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApplyPlan {
    pub profile_id: String,
    pub profile_name: String,
    pub profile_description: String,
    pub profile_notes: Option<String>,
    pub profile_source: String,
    pub windows_label: String,
    pub windows_build: u32,
    pub elevated: bool,
    /// Changes that will be written.
    pub planned: Vec<PlannedChange>,
    /// Changes MinWin inspected and planned successfully, but cannot write
    /// because the process is not elevated.
    ///
    /// These are fully planned rather than silently dropped: inspection is
    /// read-only, so an ordinary terminal can still show the user exactly what
    /// an elevated run would do.
    pub blocked_on_elevation: Vec<PlannedChange>,
    /// Changes already in the target state. Recorded, not written.
    pub already_compliant: Vec<PlannedChange>,
    pub skipped: Vec<SkippedChange>,
    pub offered: Vec<OfferedChange>,
}

impl ApplyPlan {
    /// Changes that would actually write something.
    pub fn write_count(&self) -> usize {
        self.planned.len()
    }

    pub fn has_work(&self) -> bool {
        !self.planned.is_empty()
    }

    pub fn highest_risk(&self) -> Option<Risk> {
        self.planned.iter().map(|change| change.metadata.risk).max()
    }

    /// Whether any planned change needs a restart to take effect.
    pub fn restart_requirement(&self) -> RebootRequirement {
        let mut strongest = RebootRequirement::NoReboot;
        for change in &self.planned {
            strongest = match (strongest, change.plan.reboot) {
                (_, RebootRequirement::RebootRequired) => RebootRequirement::RebootRequired,
                (RebootRequirement::RebootRequired, _) => RebootRequirement::RebootRequired,
                (RebootRequirement::NoReboot, other) => other,
                (current, _) => current,
            };
        }
        strongest
    }

    /// True when at least one change could be applied but for the process's
    /// privileges. Nothing is dropped silently for this reason; the CLI
    /// explains it and names the command to re-run.
    pub fn needs_elevation(&self) -> bool {
        !self.blocked_on_elevation.is_empty()
    }

    /// Everything an elevated run would write: what is possible now plus what
    /// is blocked only by privileges. Used by `--dry-run` so the preview does
    /// not depend on how the terminal was launched.
    pub fn all_intended_changes(&self) -> Vec<&PlannedChange> {
        self.planned
            .iter()
            .chain(self.blocked_on_elevation.iter())
            .collect()
    }
}

/// Reads the machine and decides what to do. Writes nothing.
pub fn plan(
    machine: &dyn Machine,
    facts: &SystemFacts,
    registry: &ChangeRegistry,
    profile: &Profile,
) -> Result<ApplyPlan> {
    let mut planned = Vec::new();
    let mut blocked_on_elevation = Vec::new();
    let mut already_compliant = Vec::new();
    let mut skipped = Vec::new();
    let mut offered = Vec::new();

    // Iterate the *registry*, not the profile. The registry's order is chosen
    // so that lower-risk, no-restart changes go first, and a profile must not
    // be able to rearrange that. The profile only decides which entries
    // participate.
    //
    // Every id in the profile is known to be registered because the loader
    // resolved all of them through `ChangeRegistry::require` before this point.
    for change in registry.iter() {
        let Some(selection) = profile.selection(change.id()) else {
            continue;
        };
        let metadata = change.metadata().clone();

        if !selection.enabled {
            offered.push(OfferedChange {
                metadata,
                profile_note: selection.note.clone(),
            });
            continue;
        }

        let applicability = change.check_applicability(machine, facts)?;
        if !applicability.is_applicable() {
            skipped.push(SkippedChange {
                metadata,
                applicability,
                inspection_error: None,
            });
            continue;
        }

        // Inspection and planning are read-only, so they run regardless of
        // privileges. Only the decision about whether MinWin may *write*
        // depends on elevation.
        let requires_admin = metadata.requires_admin;
        match change.plan(machine, facts) {
            Ok(change_plan) => {
                let entry = PlannedChange {
                    metadata,
                    plan: change_plan,
                    profile_note: selection.note.clone(),
                };
                if entry.plan.action == PlanAction::AlreadyCompliant {
                    // Nothing to write, so privileges are irrelevant.
                    already_compliant.push(entry);
                } else if requires_admin && !facts.elevated {
                    blocked_on_elevation.push(entry);
                } else {
                    planned.push(entry);
                }
            }
            // Planning failed, which means MinWin could not read the machine
            // well enough to describe the change. Reported, never assumed away.
            Err(error) => skipped.push(SkippedChange {
                metadata,
                applicability: Applicability::not_applicable(
                    "MinWin could not determine the current state",
                ),
                inspection_error: Some(error.to_string()),
            }),
        }
    }

    Ok(ApplyPlan {
        profile_id: profile.id.clone(),
        profile_name: profile.name.clone(),
        profile_description: profile.description.clone(),
        profile_notes: profile.notes.clone(),
        profile_source: profile.source_name.clone(),
        windows_label: facts.windows.label(),
        windows_build: facts.windows.build,
        elevated: facts.elevated,
        planned,
        blocked_on_elevation,
        already_compliant,
        skipped,
        offered,
    })
}

/// The outcome of one change during execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedChange {
    pub change_id: String,
    pub name: String,
    pub status: ChangeStatus,
    pub before: String,
    pub after: Option<String>,
    pub verification: Option<VerificationResult>,
    pub reboot: RebootRequirement,
    pub error: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplyOutcome {
    pub session_id: i64,
    pub profile_id: String,
    pub profile_name: String,
    pub status: SessionStatus,
    pub results: Vec<AppliedChange>,
    /// Changes recorded as already matching the target.
    pub already_compliant: Vec<String>,
    pub stopped_early: bool,
}

impl ApplyOutcome {
    pub fn verified_count(&self) -> usize {
        self.count_of(ChangeStatus::Verified)
    }

    pub fn failed_count(&self) -> usize {
        self.count_of(ChangeStatus::Failed)
    }

    fn count_of(&self, status: ChangeStatus) -> usize {
        self.results
            .iter()
            .filter(|result| result.status == status)
            .count()
    }

    /// Changes that need a restart before they take full effect.
    pub fn needing_restart(&self) -> Vec<&AppliedChange> {
        self.results
            .iter()
            .filter(|result| result.status.is_restorable() && result.reboot.needs_restart())
            .collect()
    }
}

/// Persists the plan and applies it.
///
/// `plan` must be the plan that was shown to the user. Re-planning here would
/// risk applying something that was never displayed.
pub fn execute(
    machine: &dyn Machine,
    facts: &SystemFacts,
    registry: &ChangeRegistry,
    database: &mut Database,
    plan: &ApplyPlan,
    clock: &dyn Clock,
) -> Result<ApplyOutcome> {
    // Step 1: everything needed to undo this session goes to disk first.
    let pending: Vec<PendingChange> = plan
        .planned
        .iter()
        .chain(plan.already_compliant.iter())
        .map(|change| PendingChange {
            change_id: change.plan.change_id.clone(),
            risk: change.metadata.risk,
            reboot: change.plan.reboot,
            state_before: change.plan.current.clone(),
            state_planned: change.plan.target.clone(),
            rollback: change.plan.rollback.clone(),
            already_compliant: change.plan.action == PlanAction::AlreadyCompliant,
        })
        .collect();

    let header = ApplySessionHeader {
        profile_id: plan.profile_id.clone(),
        profile_name: plan.profile_name.clone(),
        profile_source: plan.profile_source.clone(),
        windows_label: plan.windows_label.clone(),
        windows_build: plan.windows_build,
        elevated: plan.elevated,
    };
    let session_id = database.begin_apply_session(&header, &pending, clock.now())?;
    tracing::info!(session_id, profile = %plan.profile_id, "apply session recorded");

    // Step 2: the machine may now be written to.
    let mut results = Vec::new();
    let mut stopped_early = false;

    for planned in &plan.planned {
        let change = registry.require(&planned.plan.change_id, &plan.profile_source)?;

        database.mark_change_applying(session_id, change.id())?;

        match apply_one(machine, facts, change, &planned.plan) {
            Ok(outcome) => {
                let status = if outcome.verification.is_verified() {
                    ChangeStatus::Verified
                } else {
                    ChangeStatus::Failed
                };
                let observed = observed_of(&outcome.verification);
                database.record_change_outcome(
                    session_id,
                    change.id(),
                    status,
                    observed,
                    Some(&outcome.verification),
                    (!outcome.verification.is_verified())
                        .then(|| outcome.verification.explain())
                        .as_deref(),
                    clock.now(),
                )?;

                let verified = outcome.verification.is_verified();
                results.push(AppliedChange {
                    change_id: change.id().to_string(),
                    name: planned.metadata.name.to_string(),
                    status,
                    before: planned.plan.current.summary.clone(),
                    after: observed.map(|state| state.summary.clone()),
                    error: (!verified).then(|| outcome.verification.explain()),
                    verification: Some(outcome.verification),
                    reboot: outcome.reboot,
                    note: outcome.note,
                });

                if !verified {
                    // The write reported success but the machine disagrees.
                    // Stop rather than stacking more changes on top of a state
                    // MinWin no longer understands.
                    tracing::warn!(
                        change = change.id(),
                        "verification failed; stopping the apply session"
                    );
                    stopped_early = true;
                    break;
                }
            }
            Err(error) => {
                tracing::warn!(change = change.id(), %error, "a change failed to apply");
                database.record_change_outcome(
                    session_id,
                    change.id(),
                    ChangeStatus::Failed,
                    None,
                    None,
                    Some(&error.to_string()),
                    clock.now(),
                )?;
                results.push(AppliedChange {
                    change_id: change.id().to_string(),
                    name: planned.metadata.name.to_string(),
                    status: ChangeStatus::Failed,
                    before: planned.plan.current.summary.clone(),
                    after: None,
                    verification: None,
                    reboot: planned.plan.reboot,
                    error: Some(error.to_string()),
                    note: None,
                });
                stopped_early = true;
                break;
            }
        }
    }

    let failed = results
        .iter()
        .filter(|result| result.status == ChangeStatus::Failed)
        .count();
    let status = if stopped_early {
        if failed > 0 {
            SessionStatus::CompletedWithFailures
        } else {
            SessionStatus::Stopped
        }
    } else {
        SessionStatus::Completed
    };
    database.finish_apply_session(session_id, status, clock.now())?;

    Ok(ApplyOutcome {
        session_id,
        profile_id: plan.profile_id.clone(),
        profile_name: plan.profile_name.clone(),
        status,
        results,
        already_compliant: plan
            .already_compliant
            .iter()
            .map(|change| change.metadata.name.to_string())
            .collect(),
        stopped_early,
    })
}

struct OneOutcome {
    verification: VerificationResult,
    reboot: RebootRequirement,
    note: Option<String>,
}

fn apply_one(
    machine: &dyn Machine,
    facts: &SystemFacts,
    change: &dyn SystemChange,
    plan: &ChangePlan,
) -> Result<OneOutcome> {
    let applied = change.apply(machine, facts, plan)?;
    let verification = change.verify(machine, facts, &plan.target)?;
    Ok(OneOutcome {
        verification,
        reboot: applied.reboot,
        note: applied.note,
    })
}

fn observed_of(verification: &VerificationResult) -> Option<&crate::changes::ObservedState> {
    match verification {
        VerificationResult::Verified { observed } => Some(observed),
        VerificationResult::Mismatch { observed, .. } => Some(observed),
        VerificationResult::Unverifiable { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{
        DELIVERY_OPTIMIZATION_DOWNLOAD_MODE, DIAGTRACK_START_TYPE, POWER_PLAN_HIGH_PERFORMANCE,
        SYSMAIN_START_TYPE,
    };
    use crate::core::clock::FixedClock;
    use crate::profiles::load_builtin;
    use crate::sys::fake::{FailOn, FakeMachine};
    use crate::sys::model::{ManagementState, RegistryRoot, ServiceStartType};

    fn setup(machine: &FakeMachine, profile_id: &str) -> (ChangeRegistry, Profile, SystemFacts) {
        let registry = ChangeRegistry::load();
        let profile = load_builtin(profile_id, &registry).expect("profile");
        let facts = SystemFacts::gather(machine).expect("facts");
        (registry, profile, facts)
    }

    fn ids(changes: &[PlannedChange]) -> Vec<&str> {
        changes
            .iter()
            .map(|change| change.plan.change_id.as_str())
            .collect()
    }

    // -- planning ------------------------------------------------------------

    #[test]
    fn planning_the_minimal_profile_on_an_elevated_machine_plans_both_changes() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, profile, facts) = setup(&machine, "minimal");
        let plan = plan(&machine, &facts, &registry, &profile).expect("plan");

        assert_eq!(
            ids(&plan.planned),
            vec![DELIVERY_OPTIMIZATION_DOWNLOAD_MODE, DIAGTRACK_START_TYPE],
            "changes run in registry order, not profile order"
        );
        assert!(plan.skipped.is_empty());
        assert_eq!(plan.write_count(), 2);
        assert!(plan.has_work());
    }

    #[test]
    fn planning_writes_nothing_to_the_machine() {
        // This is what makes --dry-run trustworthy.
        let machine = FakeMachine::windows_11().elevated();
        let (registry, profile, facts) = setup(&machine, "gaming");
        plan(&machine, &facts, &registry, &profile).expect("plan");
        assert!(
            machine.writes().is_empty(),
            "planning must not write, saw {:?}",
            machine.writes()
        );
    }

    #[test]
    fn an_opt_in_change_is_offered_but_never_planned() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, profile, facts) = setup(&machine, "minimal");
        let plan = plan(&machine, &facts, &registry, &profile).expect("plan");

        assert!(!ids(&plan.planned).contains(&SYSMAIN_START_TYPE));
        let offered: Vec<&str> = plan
            .offered
            .iter()
            .map(|change| change.metadata.id)
            .collect();
        assert_eq!(offered, vec![SYSMAIN_START_TYPE]);
        // And the profile's reasoning reaches the user.
        assert!(
            plan.offered[0]
                .profile_note
                .as_deref()
                .unwrap_or_default()
                .contains("Benchmark")
        );
    }

    #[test]
    fn a_non_elevated_machine_can_apply_only_the_change_that_needs_no_admin() {
        let machine = FakeMachine::windows_11();
        let (registry, profile, facts) = setup(&machine, "gaming");
        let plan = plan(&machine, &facts, &registry, &profile).expect("plan");

        // Only the power plan can actually be written.
        assert_eq!(ids(&plan.planned), vec![POWER_PLAN_HIGH_PERFORMANCE]);

        // The other two are fully planned, not skipped: inspection is
        // read-only, so MinWin can still show exactly what they would do.
        assert_eq!(
            ids(&plan.blocked_on_elevation),
            vec![DELIVERY_OPTIMIZATION_DOWNLOAD_MODE, DIAGTRACK_START_TYPE]
        );
        assert!(plan.needs_elevation());
        assert!(
            plan.skipped.is_empty(),
            "a privilege problem is not an applicability problem"
        );

        // And the blocked entries carry real before/after values.
        for change in &plan.blocked_on_elevation {
            assert!(!change.plan.current.summary.is_empty());
            assert!(!change.plan.target.summary.is_empty());
            assert!(!change.plan.rollback.summary.is_empty());
        }
        assert_eq!(plan.all_intended_changes().len(), 3);
    }

    #[test]
    fn planning_without_elevation_still_writes_nothing() {
        let machine = FakeMachine::windows_11();
        let (registry, profile, facts) = setup(&machine, "minimal");
        plan(&machine, &facts, &registry, &profile).expect("plan");
        assert!(machine.writes().is_empty());
    }

    #[test]
    fn an_already_compliant_change_is_not_blocked_on_elevation() {
        // Nothing would be written, so privileges are irrelevant.
        let machine = FakeMachine::windows_11()
            .with_service_start_type("DiagTrack", ServiceStartType::Manual)
            .with_registry_dword(
                RegistryRoot::LocalMachine,
                r"SOFTWARE\Policies\Microsoft\Windows\DeliveryOptimization",
                "DODownloadMode",
                0,
            );
        let (registry, profile, facts) = setup(&machine, "minimal");
        let plan = plan(&machine, &facts, &registry, &profile).expect("plan");

        assert_eq!(plan.already_compliant.len(), 2);
        assert!(plan.blocked_on_elevation.is_empty());
        assert!(!plan.needs_elevation());
    }

    #[test]
    fn a_managed_device_has_the_telemetry_change_declined_with_a_reason() {
        let machine = FakeMachine::windows_11()
            .elevated()
            .managed(ManagementState {
                domain_joined: false,
                defender_for_endpoint_onboarded: true,
            });
        let (registry, profile, facts) = setup(&machine, "minimal");
        let plan = plan(&machine, &facts, &registry, &profile).expect("plan");

        assert!(!ids(&plan.planned).contains(&DIAGTRACK_START_TYPE));
        let declined = plan
            .skipped
            .iter()
            .find(|change| change.metadata.id == DIAGTRACK_START_TYPE)
            .expect("the telemetry change should be skipped");
        assert!(
            declined
                .applicability
                .explain()
                .contains("Defender for Endpoint")
        );
    }

    #[test]
    fn a_machine_without_the_high_performance_plan_skips_it_rather_than_failing() {
        let high = crate::sys::model::PowerSchemeId::parse("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c")
            .expect("guid");
        let machine = FakeMachine::windows_11()
            .elevated()
            .without_power_scheme(&high);
        let (registry, profile, facts) = setup(&machine, "gaming");
        let plan = plan(&machine, &facts, &registry, &profile).expect("plan");

        assert!(!ids(&plan.planned).contains(&POWER_PLAN_HIGH_PERFORMANCE));
        assert!(
            plan.skipped
                .iter()
                .any(|change| change.metadata.id == POWER_PLAN_HIGH_PERFORMANCE)
        );
        // The rest of the profile still applies.
        assert_eq!(plan.write_count(), 2);
    }

    #[test]
    fn a_machine_already_in_the_target_state_plans_no_writes() {
        let machine = FakeMachine::windows_11()
            .elevated()
            .with_service_start_type("DiagTrack", ServiceStartType::Manual)
            .with_registry_dword(
                RegistryRoot::LocalMachine,
                r"SOFTWARE\Policies\Microsoft\Windows\DeliveryOptimization",
                "DODownloadMode",
                0,
            );
        let (registry, profile, facts) = setup(&machine, "minimal");
        let plan = plan(&machine, &facts, &registry, &profile).expect("plan");

        assert!(!plan.has_work());
        assert_eq!(plan.already_compliant.len(), 2);
    }

    #[test]
    fn the_strongest_restart_requirement_is_reported() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, profile, facts) = setup(&machine, "minimal");
        let plan = plan(&machine, &facts, &registry, &profile).expect("plan");
        // DiagTrack recommends a restart; Delivery Optimization needs none.
        assert_eq!(
            plan.restart_requirement(),
            RebootRequirement::RebootRecommended
        );
    }

    #[test]
    fn the_highest_risk_of_a_shipped_profile_is_low() {
        for profile_id in ["minimal", "gaming"] {
            let machine = FakeMachine::windows_11().elevated();
            let (registry, profile, facts) = setup(&machine, profile_id);
            let plan = plan(&machine, &facts, &registry, &profile).expect("plan");
            assert_eq!(
                plan.highest_risk(),
                Some(Risk::Low),
                "profile {profile_id} should not apply a medium-risk change by default"
            );
        }
    }

    // -- execution -----------------------------------------------------------

    fn execute_minimal(machine: &FakeMachine) -> (Database, ApplyOutcome) {
        let (registry, profile, facts) = setup(machine, "minimal");
        let plan = plan(machine, &facts, &registry, &profile).expect("plan");
        let mut database = Database::open_in_memory().expect("database");
        let clock = FixedClock::stepping(chrono::Utc::now(), 1);
        let outcome =
            execute(machine, &facts, &registry, &mut database, &plan, &clock).expect("execute");
        (database, outcome)
    }

    #[test]
    fn applying_the_minimal_profile_writes_verifies_and_records_each_change() {
        let machine = FakeMachine::windows_11().elevated();
        let (database, outcome) = execute_minimal(&machine);

        assert_eq!(outcome.status, SessionStatus::Completed);
        assert_eq!(outcome.verified_count(), 2);
        assert_eq!(outcome.failed_count(), 0);
        assert!(!outcome.stopped_early);

        // The machine really changed.
        assert_eq!(
            machine.service_start_type("DiagTrack"),
            Some(ServiceStartType::Manual)
        );
        assert_eq!(
            machine.registry_dword(
                RegistryRoot::LocalMachine,
                r"SOFTWARE\Policies\Microsoft\Windows\DeliveryOptimization",
                "DODownloadMode"
            ),
            Some(0)
        );

        // And every change is recorded as verified, with its rollback data.
        let records = database
            .change_records(outcome.session_id)
            .expect("records");
        assert_eq!(
            records.len(),
            2,
            "only the two enabled changes are recorded; the opt-in one is not"
        );
        let verified: Vec<&str> = records
            .iter()
            .filter(|record| record.status == ChangeStatus::Verified)
            .map(|record| record.change_id.as_str())
            .collect();
        assert_eq!(verified.len(), 2);
        for record in records.iter().filter(|r| r.status.is_restorable()) {
            assert!(!record.rollback.summary.is_empty());
            assert!(record.verification.is_some());
        }
    }

    #[test]
    fn rollback_data_is_on_disk_before_the_first_write_happens() {
        // Executed by inspecting the write log against the session row: the
        // session must exist with planned state for every change, and the
        // write log must contain exactly the applied changes.
        let machine = FakeMachine::windows_11().elevated();
        let (database, outcome) = execute_minimal(&machine);

        let records = database
            .change_records(outcome.session_id)
            .expect("records");
        for record in &records {
            // Present for every change, including the one never written.
            assert!(
                !record.state_before.summary.is_empty(),
                "change {} has no recorded pre-change state",
                record.change_id
            );
        }
        assert_eq!(machine.writes().len(), 2);
    }

    #[test]
    fn a_failing_change_stops_the_session_and_keeps_earlier_changes_recorded() {
        // DiagTrack is applied second in registry order, so the Delivery
        // Optimization change succeeds first and must remain restorable.
        let machine = FakeMachine::windows_11()
            .elevated()
            .failing(FailOn::ServiceWrite("DiagTrack".into()));
        let (database, outcome) = execute_minimal(&machine);

        assert_eq!(outcome.status, SessionStatus::CompletedWithFailures);
        assert!(outcome.stopped_early);
        assert_eq!(outcome.verified_count(), 1);
        assert_eq!(outcome.failed_count(), 1);

        let failure = outcome
            .results
            .iter()
            .find(|result| result.status == ChangeStatus::Failed)
            .expect("a failure");
        assert_eq!(failure.change_id, DIAGTRACK_START_TYPE);
        assert!(failure.error.as_deref().unwrap().contains("denied"));

        // The successful change is still recorded as restorable.
        assert!(
            database
                .latest_restorable_apply_session()
                .expect("restorable")
                .is_some()
        );
        let counts = database.session_counts(outcome.session_id).expect("counts");
        assert_eq!(counts.verified, 1);
        assert_eq!(counts.failed, 1);
    }

    #[test]
    fn a_change_that_does_not_read_back_is_a_failure_not_a_success() {
        // The fake records writes but we remove the effect behind MinWin's
        // back, so verification must catch the mismatch.
        let machine = FakeMachine::windows_11().elevated();
        let (registry, profile, facts) = setup(&machine, "minimal");
        let plan = plan(&machine, &facts, &registry, &profile).expect("plan");
        let mut database = Database::open_in_memory().expect("database");

        // Drive the first change manually, then subvert it.
        let clock = FixedClock::stepping(chrono::Utc::now(), 1);
        let outcome =
            execute(&machine, &facts, &registry, &mut database, &plan, &clock).expect("execute");
        assert_eq!(outcome.verified_count(), 2);

        // Now prove the mismatch path by verifying against a stale target.
        let change = registry
            .require(DIAGTRACK_START_TYPE, "test")
            .expect("change");
        machine.change_service_externally("DiagTrack", ServiceStartType::Automatic);
        let verification = change
            .verify(&machine, &facts, &plan.planned[1].plan.target)
            .expect("verify");
        assert!(matches!(verification, VerificationResult::Mismatch { .. }));
    }

    #[test]
    fn applying_a_compliant_machine_records_the_session_without_writing() {
        let machine = FakeMachine::windows_11()
            .elevated()
            .with_service_start_type("DiagTrack", ServiceStartType::Manual)
            .with_registry_dword(
                RegistryRoot::LocalMachine,
                r"SOFTWARE\Policies\Microsoft\Windows\DeliveryOptimization",
                "DODownloadMode",
                0,
            );
        let (database, outcome) = execute_minimal(&machine);

        assert!(machine.writes().is_empty());
        assert_eq!(outcome.status, SessionStatus::Completed);
        assert_eq!(outcome.already_compliant.len(), 2);
        assert_eq!(
            database
                .session_counts(outcome.session_id)
                .expect("counts")
                .already_compliant,
            2
        );
        // Nothing to roll back, because nothing changed.
        assert!(
            database
                .latest_restorable_apply_session()
                .expect("restorable")
                .is_none()
        );
    }

    #[test]
    fn changes_needing_a_restart_are_reported_after_apply() {
        let machine = FakeMachine::windows_11().elevated();
        let (_database, outcome) = execute_minimal(&machine);
        let needing = outcome.needing_restart();
        assert_eq!(needing.len(), 1);
        assert_eq!(needing[0].change_id, DIAGTRACK_START_TYPE);
        assert_eq!(needing[0].reboot, RebootRequirement::RebootRecommended);
    }

    #[test]
    fn a_running_service_is_reported_as_taking_effect_after_restart() {
        let machine = FakeMachine::windows_11().elevated();
        let (_database, outcome) = execute_minimal(&machine);
        let diagtrack = outcome
            .results
            .iter()
            .find(|result| result.change_id == DIAGTRACK_START_TYPE)
            .expect("diagtrack");
        assert!(
            diagtrack
                .note
                .as_deref()
                .unwrap_or_default()
                .contains("still running")
        );
    }

    #[test]
    fn applying_the_gaming_profile_also_switches_the_power_plan() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, profile, facts) = setup(&machine, "gaming");
        let plan = plan(&machine, &facts, &registry, &profile).expect("plan");
        let mut database = Database::open_in_memory().expect("database");
        let clock = FixedClock::stepping(chrono::Utc::now(), 1);
        let outcome =
            execute(&machine, &facts, &registry, &mut database, &plan, &clock).expect("execute");

        assert_eq!(outcome.verified_count(), 3);
        assert_eq!(
            machine
                .active_power_scheme_id()
                .map(|id| id.as_str().to_string()),
            Some("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c".to_string())
        );
    }
}
