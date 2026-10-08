//! Restoring what MinWin changed.
//!
//! Rollback never assumes the machine still looks the way MinWin left it. The
//! flow is:
//!
//! 1. Find the most recent session that still holds restorable changes.
//! 2. Inspect the machine *now*, per change.
//! 3. Classify each one: safe to restore, already original, changed by
//!    somebody else, or unreadable.
//! 4. Hand that assessment back to the caller for confirmation.
//! 5. Restore, verify, and record — including the state the machine was in
//!    immediately before the restore, so the history stays truthful.
//!
//! Changes are restored in reverse application order, so the machine unwinds
//! the way it was wound.
//!
//! # On `--yes`
//!
//! `--yes` answers the ordinary "are you sure" question. It does **not**
//! authorise overwriting a value that changed outside MinWin — that needs its
//! own explicit flag, because the two decisions are genuinely different. One is
//! "I know what I asked for"; the other is "I accept discarding somebody
//! else's change".

use serde::{Deserialize, Serialize};

use crate::changes::ChangeRegistry;
use crate::changes::model::{ObservedState, RebootRequirement, VerificationResult};
use crate::core::clock::Clock;
use crate::core::error::{MinWinError, Result};
use crate::state::models::{ChangeRecord, RollbackStatus, SessionStatus};
use crate::state::{ApplySessionRecord, Database};
use crate::sys::SystemFacts;
use crate::sys::traits::Machine;

/// MinWin's read on whether a change can be safely restored right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreAssessment {
    /// The machine holds what MinWin applied. Restoring is unambiguous.
    SafeToRestore,
    /// The machine already holds the original value; there is nothing to do.
    AlreadyOriginal,
    /// The value changed after MinWin applied it. Restoring would discard
    /// somebody else's change, so it needs explicit authorisation.
    ChangedOutsideMinWin,
    /// The current value could not be read, so MinWin cannot tell what
    /// restoring would overwrite.
    UnableToInspect,
}

impl RestoreAssessment {
    pub fn label(self) -> &'static str {
        match self {
            Self::SafeToRestore => "safe to restore",
            Self::AlreadyOriginal => "already at its original value",
            Self::ChangedOutsideMinWin => "changed outside MinWin",
            Self::UnableToInspect => "unable to inspect",
        }
    }

    /// Whether restoring this change needs the user to say so explicitly.
    pub fn needs_explicit_authorisation(self) -> bool {
        matches!(self, Self::ChangedOutsideMinWin | Self::UnableToInspect)
    }
}

/// One change rollback is considering, with everything the user needs to judge
/// it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackCandidate {
    pub change_record_id: i64,
    pub change_id: String,
    pub name: String,
    /// What MinWin set.
    pub minwin_applied: String,
    /// What the machine reads now.
    pub current: Option<String>,
    /// What MinWin will restore.
    pub original: String,
    pub assessment: RestoreAssessment,
    pub reboot: RebootRequirement,
    pub inspection_error: Option<String>,
    /// Extra explanation, such as the policy value being removed rather than
    /// set to zero.
    pub note: Option<String>,
}

/// The assessment of a whole rollback, before anything is written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackPlan {
    pub session_id: i64,
    pub profile_id: String,
    pub profile_name: String,
    pub applied_at: chrono::DateTime<chrono::Utc>,
    pub session_status: SessionStatus,
    /// In the order MinWin will restore them: reverse of application.
    pub candidates: Vec<RollbackCandidate>,
}

impl RollbackPlan {
    /// Changes MinWin will restore without needing extra authorisation.
    pub fn straightforward_count(&self) -> usize {
        self.candidates
            .iter()
            .filter(|candidate| candidate.assessment == RestoreAssessment::SafeToRestore)
            .count()
    }

    pub fn needing_authorisation(&self) -> Vec<&RollbackCandidate> {
        self.candidates
            .iter()
            .filter(|candidate| candidate.assessment.needs_explicit_authorisation())
            .collect()
    }

    pub fn requires_explicit_authorisation(&self) -> bool {
        !self.needing_authorisation().is_empty()
    }

    pub fn restart_requirement(&self) -> RebootRequirement {
        self.candidates
            .iter()
            .filter(|candidate| candidate.assessment != RestoreAssessment::AlreadyOriginal)
            .map(|candidate| candidate.reboot)
            .fold(RebootRequirement::NoReboot, |strongest, next| {
                match (strongest, next) {
                    (_, RebootRequirement::RebootRequired)
                    | (RebootRequirement::RebootRequired, _) => RebootRequirement::RebootRequired,
                    (RebootRequirement::NoReboot, other) => other,
                    (current, _) => current,
                }
            })
    }
}

/// Assesses the most recent restorable session. Writes nothing.
pub fn plan_latest(
    machine: &dyn Machine,
    facts: &SystemFacts,
    registry: &ChangeRegistry,
    database: &Database,
) -> Result<Option<RollbackPlan>> {
    let Some(session) = database.latest_restorable_apply_session()? else {
        return Ok(None);
    };
    Ok(Some(plan_session(
        machine, facts, registry, database, &session,
    )?))
}

pub fn plan_session(
    machine: &dyn Machine,
    facts: &SystemFacts,
    registry: &ChangeRegistry,
    database: &Database,
    session: &ApplySessionRecord,
) -> Result<RollbackPlan> {
    let mut records: Vec<ChangeRecord> = database
        .change_records(session.id)?
        .into_iter()
        .filter(|record| record.status.is_restorable())
        .collect();
    // Unwind in reverse application order.
    records.sort_by_key(|record| std::cmp::Reverse(record.order_index));

    let mut candidates = Vec::with_capacity(records.len());
    for record in records {
        let change = registry.get(&record.change_id);
        let name = change
            .map(|change| change.metadata().name.to_string())
            .unwrap_or_else(|| record.change_id.clone());

        let applied = record
            .state_after
            .clone()
            .unwrap_or_else(|| record.state_planned.clone());

        let (assessment, current, inspection_error) = match change {
            None => (
                RestoreAssessment::UnableToInspect,
                None,
                Some(format!(
                    "this build of MinWin no longer implements change {}",
                    record.change_id
                )),
            ),
            Some(change) => match change.inspect(machine, facts) {
                Ok(observed) => {
                    let assessment = assess(&observed, &applied, &record.rollback);
                    (assessment, Some(observed), None)
                }
                Err(error) => (
                    RestoreAssessment::UnableToInspect,
                    None,
                    Some(error.to_string()),
                ),
            },
        };

        candidates.push(RollbackCandidate {
            change_record_id: record.id,
            change_id: record.change_id,
            name,
            minwin_applied: applied.summary,
            current: current.map(|state| state.summary),
            original: record.rollback.summary.clone(),
            assessment,
            reboot: record.reboot,
            inspection_error,
            note: None,
        });
    }

    Ok(RollbackPlan {
        session_id: session.id,
        profile_id: session.profile_id.clone(),
        profile_name: session.profile_name.clone(),
        applied_at: session.started_at,
        session_status: session.status,
        candidates,
    })
}

/// Compares the machine's current state with what MinWin applied and what it
/// would restore. Uses the machine-readable detail, never the rendered text.
fn assess(
    current: &ObservedState,
    applied: &ObservedState,
    rollback: &crate::changes::RollbackData,
) -> RestoreAssessment {
    if current.detail == rollback.detail {
        return RestoreAssessment::AlreadyOriginal;
    }
    if current.matches(applied) {
        return RestoreAssessment::SafeToRestore;
    }
    RestoreAssessment::ChangedOutsideMinWin
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestoredChange {
    pub change_id: String,
    pub name: String,
    pub status: RollbackStatus,
    pub restored_to: Option<String>,
    pub verification: Option<VerificationResult>,
    pub error: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackOutcome {
    pub rollback_session_id: i64,
    pub apply_session_id: i64,
    pub status: SessionStatus,
    pub results: Vec<RestoredChange>,
    /// True when the whole apply session is now fully restored.
    pub session_fully_restored: bool,
}

impl RollbackOutcome {
    pub fn restored_count(&self) -> usize {
        self.count_of(RollbackStatus::Restored)
    }

    pub fn failed_count(&self) -> usize {
        self.count_of(RollbackStatus::Failed)
    }

    pub fn skipped_count(&self) -> usize {
        self.count_of(RollbackStatus::Skipped)
    }

    fn count_of(&self, status: RollbackStatus) -> usize {
        self.results
            .iter()
            .filter(|result| result.status == status)
            .count()
    }
}

/// How much the caller has authorised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RollbackAuthorisation {
    /// Set when the user explicitly accepted overwriting values that changed
    /// outside MinWin. `--yes` alone must never set this.
    pub allow_external_changes: bool,
}

/// Restores the assessed changes.
///
/// Candidates needing authorisation that was not granted are recorded as
/// `Skipped`, not quietly restored and not silently dropped.
pub fn execute(
    machine: &dyn Machine,
    facts: &SystemFacts,
    registry: &ChangeRegistry,
    database: &mut Database,
    plan: &RollbackPlan,
    authorisation: RollbackAuthorisation,
    clock: &dyn Clock,
) -> Result<RollbackOutcome> {
    let session_id = database.begin_rollback_session(plan.session_id, clock.now())?;
    let records = database.change_records(plan.session_id)?;
    let mut results = Vec::new();

    for candidate in &plan.candidates {
        let record = records
            .iter()
            .find(|record| record.id == candidate.change_record_id)
            .ok_or_else(|| {
                MinWinError::Unsupported(format!(
                    "the recorded change {} is no longer in apply session {}",
                    candidate.change_id, plan.session_id
                ))
            })?;

        // What the machine looked like just before this restore. Recorded even
        // when MinWin declines to act, so the history explains the decision.
        let state_before_rollback = candidate
            .current
            .clone()
            .map(|summary| ObservedState::new(summary, serde_json::Value::Null))
            .unwrap_or_else(|| ObservedState::new("could not be read", serde_json::Value::Null));

        if candidate.assessment.needs_explicit_authorisation()
            && !authorisation.allow_external_changes
        {
            let note = match candidate.assessment {
                RestoreAssessment::ChangedOutsideMinWin => {
                    "skipped: the value changed outside MinWin and restoring it was not authorised"
                }
                _ => "skipped: MinWin could not read the current value",
            };
            database.record_rollback_outcome(
                session_id,
                candidate.change_record_id,
                &candidate.change_id,
                RollbackStatus::Skipped,
                &state_before_rollback,
                None,
                None,
                None,
                Some(note),
                clock.now(),
            )?;
            results.push(RestoredChange {
                change_id: candidate.change_id.clone(),
                name: candidate.name.clone(),
                status: RollbackStatus::Skipped,
                restored_to: None,
                verification: None,
                error: None,
                note: Some(note.to_string()),
            });
            continue;
        }

        if candidate.assessment == RestoreAssessment::AlreadyOriginal {
            database.record_rollback_outcome(
                session_id,
                candidate.change_record_id,
                &candidate.change_id,
                RollbackStatus::AlreadyOriginal,
                &state_before_rollback,
                None,
                None,
                None,
                Some("already at its original value; nothing was written"),
                clock.now(),
            )?;
            results.push(RestoredChange {
                change_id: candidate.change_id.clone(),
                name: candidate.name.clone(),
                status: RollbackStatus::AlreadyOriginal,
                restored_to: Some(candidate.original.clone()),
                verification: None,
                error: None,
                note: Some("already at its original value".to_string()),
            });
            continue;
        }

        let change = registry.require(&candidate.change_id, "the state database")?;
        match change.rollback(machine, facts, &record.rollback) {
            Ok(result) => {
                // Read it back: a rollback MinWin cannot confirm is not a
                // rollback MinWin claims.
                let verification = change.verify(machine, facts, &result.restored)?;
                let status = if verification.is_verified() {
                    RollbackStatus::Restored
                } else {
                    RollbackStatus::Failed
                };
                database.record_rollback_outcome(
                    session_id,
                    candidate.change_record_id,
                    &candidate.change_id,
                    status,
                    &state_before_rollback,
                    Some(&result.restored),
                    Some(&verification),
                    (!verification.is_verified())
                        .then(|| verification.explain())
                        .as_deref(),
                    result.note.as_deref(),
                    clock.now(),
                )?;
                results.push(RestoredChange {
                    change_id: candidate.change_id.clone(),
                    name: candidate.name.clone(),
                    status,
                    restored_to: Some(result.restored.summary),
                    error: (!verification.is_verified()).then(|| verification.explain()),
                    verification: Some(verification),
                    note: result.note,
                });
            }
            Err(error) => {
                tracing::warn!(
                    change = %candidate.change_id,
                    %error,
                    "a change could not be restored"
                );
                database.record_rollback_outcome(
                    session_id,
                    candidate.change_record_id,
                    &candidate.change_id,
                    RollbackStatus::Failed,
                    &state_before_rollback,
                    None,
                    None,
                    Some(&error.to_string()),
                    None,
                    clock.now(),
                )?;
                results.push(RestoredChange {
                    change_id: candidate.change_id.clone(),
                    name: candidate.name.clone(),
                    status: RollbackStatus::Failed,
                    restored_to: None,
                    verification: None,
                    error: Some(error.to_string()),
                    note: None,
                });
                // Unlike apply, rollback continues: each remaining change is
                // an independent chance to get the machine closer to where it
                // started.
            }
        }
    }

    let failed = results
        .iter()
        .filter(|result| result.status == RollbackStatus::Failed)
        .count();
    let skipped = results
        .iter()
        .filter(|result| result.status == RollbackStatus::Skipped)
        .count();
    let status = if failed > 0 {
        SessionStatus::CompletedWithFailures
    } else if skipped > 0 {
        SessionStatus::Stopped
    } else {
        SessionStatus::Completed
    };

    database.finish_rollback_session(session_id, plan.session_id, status, clock.now())?;
    let session_fully_restored = database.session_counts(plan.session_id)?.active() == 0;

    Ok(RollbackOutcome {
        rollback_session_id: session_id,
        apply_session_id: plan.session_id,
        status,
        results,
        session_fully_restored,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{DELIVERY_OPTIMIZATION_DOWNLOAD_MODE, DIAGTRACK_START_TYPE};
    use crate::core::clock::FixedClock;
    use crate::engine::apply;
    use crate::profiles::load_builtin;
    use crate::sys::fake::{FailOn, FakeMachine};
    use crate::sys::model::{RegistryRoot, ServiceStartType};

    const DO_SUBKEY: &str = r"SOFTWARE\Policies\Microsoft\Windows\DeliveryOptimization";

    fn applied(machine: &FakeMachine) -> (ChangeRegistry, SystemFacts, Database) {
        let registry = ChangeRegistry::load();
        let profile = load_builtin("minimal", &registry).expect("profile");
        let facts = SystemFacts::gather(machine).expect("facts");
        let plan = apply::plan(machine, &facts, &registry, &profile).expect("plan");
        let mut database = Database::open_in_memory().expect("database");
        apply::execute(
            machine,
            &facts,
            &registry,
            &mut database,
            &plan,
            &FixedClock::stepping(chrono::Utc::now(), 1),
        )
        .expect("execute");
        (registry, facts, database)
    }

    fn clock() -> FixedClock {
        FixedClock::stepping(chrono::Utc::now(), 1)
    }

    fn candidate<'a>(plan: &'a RollbackPlan, change_id: &str) -> &'a RollbackCandidate {
        plan.candidates
            .iter()
            .find(|candidate| candidate.change_id == change_id)
            .expect("candidate should exist")
    }

    #[test]
    fn with_nothing_applied_there_is_nothing_to_roll_back() {
        let machine = FakeMachine::windows_11().elevated();
        let registry = ChangeRegistry::load();
        let facts = SystemFacts::gather(&machine).expect("facts");
        let database = Database::open_in_memory().expect("database");
        assert!(
            plan_latest(&machine, &facts, &registry, &database)
                .expect("plan")
                .is_none()
        );
    }

    #[test]
    fn planning_a_rollback_writes_nothing_and_unwinds_in_reverse_order() {
        let machine = FakeMachine::windows_11().elevated();
        let writes_after_apply = machine.writes().len();
        let (registry, facts, database) = applied(&machine);

        let plan = plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");

        assert_eq!(plan.candidates.len(), 2);
        // Applied in registry order (Delivery Optimization, then DiagTrack),
        // so restored in the opposite order.
        assert_eq!(plan.candidates[0].change_id, DIAGTRACK_START_TYPE);
        assert_eq!(
            plan.candidates[1].change_id,
            DELIVERY_OPTIMIZATION_DOWNLOAD_MODE
        );
        assert_eq!(plan.straightforward_count(), 2);
        assert!(!plan.requires_explicit_authorisation());
        assert_eq!(machine.writes().len(), writes_after_apply + 2);
    }

    #[test]
    fn a_full_rollback_restores_the_machine_and_marks_the_session_restored() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, mut database) = applied(&machine);
        let plan = plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");

        let outcome = execute(
            &machine,
            &facts,
            &registry,
            &mut database,
            &plan,
            RollbackAuthorisation::default(),
            &clock(),
        )
        .expect("rollback");

        assert_eq!(outcome.status, SessionStatus::Completed);
        assert_eq!(outcome.restored_count(), 2);
        assert_eq!(outcome.failed_count(), 0);
        assert!(outcome.session_fully_restored);

        // The machine is genuinely back where it started.
        assert_eq!(
            machine.service_start_type("DiagTrack"),
            Some(ServiceStartType::Automatic)
        );
        assert_eq!(
            machine.registry_dword(RegistryRoot::LocalMachine, DO_SUBKEY, "DODownloadMode"),
            None,
            "the policy value must be deleted, not set to zero"
        );

        // And nothing is left to restore.
        assert!(
            database
                .latest_restorable_apply_session()
                .expect("restorable")
                .is_none()
        );
    }

    #[test]
    fn every_restored_change_is_verified_before_being_claimed() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, mut database) = applied(&machine);
        let plan = plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");
        let outcome = execute(
            &machine,
            &facts,
            &registry,
            &mut database,
            &plan,
            RollbackAuthorisation::default(),
            &clock(),
        )
        .expect("rollback");

        for result in &outcome.results {
            let verification = result.verification.as_ref().expect("verification");
            assert!(
                verification.is_verified(),
                "change {} was reported restored without verification",
                result.change_id
            );
        }
    }

    #[test]
    fn the_policy_deletion_is_explained_to_the_user() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, mut database) = applied(&machine);
        let plan = plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");
        let outcome = execute(
            &machine,
            &facts,
            &registry,
            &mut database,
            &plan,
            RollbackAuthorisation::default(),
            &clock(),
        )
        .expect("rollback");

        let policy = outcome
            .results
            .iter()
            .find(|result| result.change_id == DELIVERY_OPTIMIZATION_DOWNLOAD_MODE)
            .expect("policy result");
        let note = policy.note.as_deref().unwrap_or_default();
        assert!(note.contains("not configured"));
        assert!(note.contains("removed"));
    }

    // -- the external-modification path -------------------------------------

    #[test]
    fn a_value_changed_outside_minwin_is_flagged_and_needs_authorisation() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, database) = applied(&machine);
        machine.change_service_externally("DiagTrack", ServiceStartType::Disabled);

        let plan = plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");
        let diagtrack = candidate(&plan, DIAGTRACK_START_TYPE);

        assert_eq!(
            diagtrack.assessment,
            RestoreAssessment::ChangedOutsideMinWin
        );
        assert_eq!(diagtrack.minwin_applied, "Manual");
        assert_eq!(diagtrack.current.as_deref(), Some("Disabled"));
        assert_eq!(diagtrack.original, "Automatic");
        assert!(plan.requires_explicit_authorisation());
        assert_eq!(plan.needing_authorisation().len(), 1);
    }

    #[test]
    fn without_authorisation_an_externally_changed_value_is_skipped_not_overwritten() {
        // This is the guarantee that `--yes` must not be able to bypass.
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, mut database) = applied(&machine);
        machine.change_service_externally("DiagTrack", ServiceStartType::Disabled);

        let plan = plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");
        let outcome = execute(
            &machine,
            &facts,
            &registry,
            &mut database,
            &plan,
            RollbackAuthorisation::default(),
            &clock(),
        )
        .expect("rollback");

        assert_eq!(outcome.skipped_count(), 1);
        assert_eq!(outcome.restored_count(), 1);
        assert_eq!(outcome.status, SessionStatus::Stopped);
        assert!(!outcome.session_fully_restored);

        // Somebody else's value survived untouched.
        assert_eq!(
            machine.service_start_type("DiagTrack"),
            Some(ServiceStartType::Disabled)
        );
        // And it is still recorded as needing attention.
        assert!(
            database
                .latest_restorable_apply_session()
                .expect("restorable")
                .is_some()
        );
    }

    #[test]
    fn with_authorisation_an_externally_changed_value_is_restored() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, mut database) = applied(&machine);
        machine.change_service_externally("DiagTrack", ServiceStartType::Disabled);

        let plan = plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");
        let outcome = execute(
            &machine,
            &facts,
            &registry,
            &mut database,
            &plan,
            RollbackAuthorisation {
                allow_external_changes: true,
            },
            &clock(),
        )
        .expect("rollback");

        assert_eq!(outcome.restored_count(), 2);
        assert_eq!(outcome.skipped_count(), 0);
        assert_eq!(
            machine.service_start_type("DiagTrack"),
            Some(ServiceStartType::Automatic)
        );
    }

    #[test]
    fn a_value_already_back_at_its_original_is_not_written_again() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, mut database) = applied(&machine);
        // Somebody already put it back.
        machine.change_service_externally("DiagTrack", ServiceStartType::Automatic);
        let writes_before = machine.writes().len();

        let plan = plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");
        assert_eq!(
            candidate(&plan, DIAGTRACK_START_TYPE).assessment,
            RestoreAssessment::AlreadyOriginal
        );
        assert!(!plan.requires_explicit_authorisation());

        let outcome = execute(
            &machine,
            &facts,
            &registry,
            &mut database,
            &plan,
            RollbackAuthorisation::default(),
            &clock(),
        )
        .expect("rollback");

        let diagtrack = outcome
            .results
            .iter()
            .find(|result| result.change_id == DIAGTRACK_START_TYPE)
            .expect("result");
        assert_eq!(diagtrack.status, RollbackStatus::AlreadyOriginal);
        assert!(outcome.session_fully_restored);
        // Only the policy value was written; the service was left alone.
        assert_eq!(machine.writes().len(), writes_before + 1);
    }

    #[test]
    fn an_unreadable_value_is_skipped_rather_than_blindly_restored() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, mut database) = applied(&machine);

        // The service disappears, so MinWin cannot tell what it would
        // overwrite.
        let gone = FakeMachine::windows_11()
            .elevated()
            .without_service("DiagTrack");
        let plan = plan_latest(&gone, &facts, &registry, &database)
            .expect("plan")
            .expect("some");
        assert_eq!(
            candidate(&plan, DIAGTRACK_START_TYPE).assessment,
            RestoreAssessment::UnableToInspect
        );
        assert!(plan.requires_explicit_authorisation());

        let outcome = execute(
            &gone,
            &facts,
            &registry,
            &mut database,
            &plan,
            RollbackAuthorisation::default(),
            &clock(),
        )
        .expect("rollback");
        assert_eq!(outcome.skipped_count(), 1);
    }

    #[test]
    fn a_rollback_that_fails_continues_with_the_remaining_changes() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, mut database) = applied(&machine);

        // Restoring the service will be refused, but the policy value must
        // still be restored.
        let blocked = FakeMachine::windows_11()
            .elevated()
            .with_service_start_type("DiagTrack", ServiceStartType::Manual)
            .with_registry_dword(RegistryRoot::LocalMachine, DO_SUBKEY, "DODownloadMode", 0)
            .failing(FailOn::ServiceWrite("DiagTrack".into()));

        let plan = plan_latest(&blocked, &facts, &registry, &database)
            .expect("plan")
            .expect("some");
        let outcome = execute(
            &blocked,
            &facts,
            &registry,
            &mut database,
            &plan,
            RollbackAuthorisation::default(),
            &clock(),
        )
        .expect("rollback");

        assert_eq!(outcome.failed_count(), 1);
        assert_eq!(outcome.restored_count(), 1);
        assert_eq!(outcome.status, SessionStatus::CompletedWithFailures);
        assert!(!outcome.session_fully_restored);

        // The policy value really was restored despite the earlier failure.
        assert_eq!(
            blocked.registry_dword(RegistryRoot::LocalMachine, DO_SUBKEY, "DODownloadMode"),
            None
        );
        // The failed change is still on record as needing a rollback.
        assert!(
            database
                .latest_restorable_apply_session()
                .expect("restorable")
                .is_some()
        );
    }

    #[test]
    fn rolling_back_twice_finds_nothing_the_second_time() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, mut database) = applied(&machine);
        let plan = plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");
        execute(
            &machine,
            &facts,
            &registry,
            &mut database,
            &plan,
            RollbackAuthorisation::default(),
            &clock(),
        )
        .expect("rollback");

        assert!(
            plan_latest(&machine, &facts, &registry, &database)
                .expect("plan")
                .is_none()
        );
    }

    #[test]
    fn a_change_interrupted_mid_write_is_still_offered_for_rollback() {
        // Simulates a crash: the change is left in `Applying`, with its
        // rollback data already persisted.
        let machine = FakeMachine::windows_11().elevated();
        let registry = ChangeRegistry::load();
        let profile = load_builtin("minimal", &registry).expect("profile");
        let facts = SystemFacts::gather(&machine).expect("facts");
        let apply_plan = apply::plan(&machine, &facts, &registry, &profile).expect("plan");

        let mut database = Database::open_in_memory().expect("database");
        let pending: Vec<crate::state::PendingChange> = apply_plan
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
        let session = database
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

        // The write happened, then MinWin died before recording the outcome.
        database
            .mark_change_applying(session, DIAGTRACK_START_TYPE)
            .expect("mark");
        machine.change_service_externally("DiagTrack", ServiceStartType::Manual);

        let plan = plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("a crashed session must still be restorable");
        let diagtrack = candidate(&plan, DIAGTRACK_START_TYPE);
        assert_eq!(diagtrack.assessment, RestoreAssessment::SafeToRestore);

        let outcome = execute(
            &machine,
            &facts,
            &registry,
            &mut database,
            &plan,
            RollbackAuthorisation::default(),
            &clock(),
        )
        .expect("rollback");
        assert_eq!(outcome.restored_count(), 1);
        assert_eq!(
            machine.service_start_type("DiagTrack"),
            Some(ServiceStartType::Automatic)
        );
    }

    #[test]
    fn the_restart_requirement_of_a_rollback_is_reported() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, database) = applied(&machine);
        let plan = plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");
        assert_eq!(
            plan.restart_requirement(),
            RebootRequirement::RebootRecommended
        );
    }

    #[test]
    fn assessment_uses_the_detail_not_the_summary() {
        use serde_json::json;
        let applied = ObservedState::new("Manual", json!({"v": "manual"}));
        let rollback = crate::changes::RollbackData::new("Automatic", json!({"v": "auto"}));

        let same_detail = ObservedState::new("completely different text", json!({"v": "manual"}));
        assert_eq!(
            assess(&same_detail, &applied, &rollback),
            RestoreAssessment::SafeToRestore
        );

        let at_original = ObservedState::new("whatever", json!({"v": "auto"}));
        assert_eq!(
            assess(&at_original, &applied, &rollback),
            RestoreAssessment::AlreadyOriginal
        );

        let third = ObservedState::new("Manual", json!({"v": "disabled"}));
        assert_eq!(
            assess(&third, &applied, &rollback),
            RestoreAssessment::ChangedOutsideMinWin
        );
    }
}
