//! What did MinWin change, and does the machine still look that way?
//!
//! This is deliberately **not** a general Windows diff. It answers one
//! question: for each change MinWin recorded, how does the machine now compare
//! with what MinWin left behind?
//!
//! The reason this exists as a separate command rather than as a step inside
//! rollback is that the answer matters on its own. A value that was changed by
//! somebody else after MinWin applied it is a value MinWin should not quietly
//! overwrite, and the user deserves to know before being asked to confirm
//! anything.

use serde::{Deserialize, Serialize};

use crate::changes::ChangeRegistry;
use crate::changes::model::ObservedState;
use crate::core::error::Result;
use crate::state::models::{ChangeStatus, SessionStatus};
use crate::state::{ApplySessionRecord, Database};
use crate::sys::SystemFacts;
use crate::sys::traits::Machine;

/// How the machine's current state relates to what MinWin recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStatus {
    /// The machine still holds the value MinWin applied.
    UnchangedSinceApply,
    /// The value now matches what it was *before* MinWin ran. Something, or
    /// someone, undid this change outside MinWin.
    RevertedOutsideMinWin,
    /// The value is neither what MinWin set nor what was there before.
    ChangedOutsideMinWin,
    /// MinWin recorded this change as failed, so it never altered the machine.
    NotApplied,
    /// Already restored by a MinWin rollback.
    RolledBack,
    /// The current value could not be read.
    UnableToInspect,
}

impl DiffStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::UnchangedSinceApply => "unchanged since apply",
            Self::RevertedOutsideMinWin => "reverted outside MinWin",
            Self::ChangedOutsideMinWin => "changed outside MinWin",
            Self::NotApplied => "not applied",
            Self::RolledBack => "rolled back by MinWin",
            Self::UnableToInspect => "unable to inspect",
        }
    }

    /// Whether this row needs the user's attention.
    pub fn is_surprising(self) -> bool {
        matches!(
            self,
            Self::RevertedOutsideMinWin | Self::ChangedOutsideMinWin | Self::UnableToInspect
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffEntry {
    pub change_id: String,
    pub name: String,
    /// The value before MinWin ran.
    pub before: String,
    /// The value MinWin applied, if it applied one.
    pub applied: Option<String>,
    /// The value right now.
    pub current: Option<String>,
    pub status: DiffStatus,
    /// Present when the current value could not be read.
    pub inspection_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffReport {
    pub session_id: i64,
    pub profile_id: String,
    pub profile_name: String,
    pub session_status: SessionStatus,
    pub applied_at: chrono::DateTime<chrono::Utc>,
    /// The Windows build the session ran on, so a diff taken after a feature
    /// update can be read with that in mind.
    pub session_windows_label: String,
    pub current_windows_label: String,
    pub entries: Vec<DiffEntry>,
}

impl DiffReport {
    pub fn surprising_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.status.is_surprising())
            .count()
    }

    pub fn count_of(&self, status: DiffStatus) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.status == status)
            .count()
    }

    /// True when the machine was upgraded since the session ran, which makes
    /// the comparison less meaningful.
    pub fn windows_changed(&self) -> bool {
        self.session_windows_label != self.current_windows_label
    }
}

/// Builds a diff for the most recent apply session.
///
/// Returns `None` when MinWin has never applied anything, so the caller can
/// say so plainly rather than rendering an empty table.
pub fn diff_latest(
    machine: &dyn Machine,
    facts: &SystemFacts,
    registry: &ChangeRegistry,
    database: &Database,
) -> Result<Option<DiffReport>> {
    let Some(session) = database.latest_apply_session()? else {
        return Ok(None);
    };
    Ok(Some(diff_session(
        machine, facts, registry, database, &session,
    )?))
}

pub fn diff_session(
    machine: &dyn Machine,
    facts: &SystemFacts,
    registry: &ChangeRegistry,
    database: &Database,
    session: &ApplySessionRecord,
) -> Result<DiffReport> {
    let records = database.change_records(session.id)?;
    let mut entries = Vec::with_capacity(records.len());

    for record in records {
        // A change id MinWin no longer implements can still be reported: the
        // recorded values are all the diff needs.
        let name = registry
            .get(&record.change_id)
            .map(|change| change.metadata().name.to_string())
            .unwrap_or_else(|| record.change_id.clone());

        // Only inspect the machine for changes that actually altered it.
        let needs_inspection = record.status.is_restorable();
        let current = if needs_inspection {
            match registry.get(&record.change_id) {
                Some(change) => match change.inspect(machine, facts) {
                    Ok(state) => Ok(Some(state)),
                    Err(error) => Err(error.to_string()),
                },
                None => Err(format!(
                    "this build of MinWin no longer implements change {}",
                    record.change_id
                )),
            }
        } else {
            Ok(None)
        };

        let applied = record.state_after.clone().or_else(|| {
            // An interrupted change has no recorded after-state, but its
            // intended state is still what MinWin was aiming for.
            record
                .status
                .is_restorable()
                .then(|| record.state_planned.clone())
        });

        let (status, current_summary, inspection_error) = match (&current, record.status) {
            (_, ChangeStatus::Failed) => (DiffStatus::NotApplied, None, None),
            (_, ChangeStatus::AlreadyCompliant) => (
                DiffStatus::UnchangedSinceApply,
                Some(record.state_before.summary.clone()),
                None,
            ),
            (_, ChangeStatus::RolledBack) => (DiffStatus::RolledBack, None, None),
            (_, ChangeStatus::Planned) => (DiffStatus::NotApplied, None, None),
            (Err(error), _) => (DiffStatus::UnableToInspect, None, Some(error.clone())),
            (Ok(None), _) => (DiffStatus::UnableToInspect, None, None),
            (Ok(Some(observed)), _) => {
                let status = classify(observed, applied.as_ref(), &record.state_before);
                (status, Some(observed.summary.clone()), None)
            }
        };

        entries.push(DiffEntry {
            change_id: record.change_id,
            name,
            before: record.state_before.summary,
            applied: applied.map(|state| state.summary),
            current: current_summary,
            status,
            inspection_error,
        });
    }

    Ok(DiffReport {
        session_id: session.id,
        profile_id: session.profile_id.clone(),
        profile_name: session.profile_name.clone(),
        session_status: session.status,
        applied_at: session.started_at,
        session_windows_label: session.windows_label.clone(),
        current_windows_label: facts.windows.label(),
        entries,
    })
}

/// Compares on the machine-readable detail, never on the rendered summary.
fn classify(
    current: &ObservedState,
    applied: Option<&ObservedState>,
    before: &ObservedState,
) -> DiffStatus {
    if let Some(applied) = applied
        && current.matches(applied)
    {
        return DiffStatus::UnchangedSinceApply;
    }
    if current.matches(before) {
        return DiffStatus::RevertedOutsideMinWin;
    }
    DiffStatus::ChangedOutsideMinWin
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

    /// Applies the minimal profile and returns everything needed to diff.
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

    fn entry<'a>(report: &'a DiffReport, change_id: &str) -> &'a DiffEntry {
        report
            .entries
            .iter()
            .find(|entry| entry.change_id == change_id)
            .expect("entry should exist")
    }

    #[test]
    fn with_no_apply_session_there_is_nothing_to_diff() {
        let machine = FakeMachine::windows_11().elevated();
        let registry = ChangeRegistry::load();
        let facts = SystemFacts::gather(&machine).expect("facts");
        let database = Database::open_in_memory().expect("database");

        assert!(
            diff_latest(&machine, &facts, &registry, &database)
                .expect("diff")
                .is_none()
        );
    }

    #[test]
    fn immediately_after_apply_everything_is_unchanged_since_apply() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, database) = applied(&machine);
        let report = diff_latest(&machine, &facts, &registry, &database)
            .expect("diff")
            .expect("report");

        assert_eq!(report.profile_id, "minimal");
        assert_eq!(report.surprising_count(), 0);

        let diagtrack = entry(&report, DIAGTRACK_START_TYPE);
        assert_eq!(diagtrack.status, DiffStatus::UnchangedSinceApply);
        assert_eq!(diagtrack.before, "Automatic");
        assert_eq!(diagtrack.applied.as_deref(), Some("Manual"));
        assert_eq!(diagtrack.current.as_deref(), Some("Manual"));
    }

    #[test]
    fn a_value_set_back_to_its_original_is_reported_as_reverted_outside_minwin() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, database) = applied(&machine);

        // Somebody puts the service back to Automatic by hand.
        machine.change_service_externally("DiagTrack", ServiceStartType::Automatic);

        let report = diff_latest(&machine, &facts, &registry, &database)
            .expect("diff")
            .expect("report");
        let diagtrack = entry(&report, DIAGTRACK_START_TYPE);
        assert_eq!(diagtrack.status, DiffStatus::RevertedOutsideMinWin);
        assert_eq!(diagtrack.current.as_deref(), Some("Automatic"));
        assert!(diagtrack.status.is_surprising());
        assert_eq!(report.surprising_count(), 1);
    }

    #[test]
    fn a_value_changed_to_something_else_entirely_is_reported_as_changed_outside_minwin() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, database) = applied(&machine);

        // Neither MinWin's value nor the original.
        machine.change_service_externally("DiagTrack", ServiceStartType::Disabled);

        let report = diff_latest(&machine, &facts, &registry, &database)
            .expect("diff")
            .expect("report");
        let diagtrack = entry(&report, DIAGTRACK_START_TYPE);
        assert_eq!(diagtrack.status, DiffStatus::ChangedOutsideMinWin);
        assert_eq!(diagtrack.current.as_deref(), Some("Disabled"));
    }

    #[test]
    fn a_deleted_policy_value_is_detected_as_reverted() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, database) = applied(&machine);

        // The policy value is removed, which is exactly its original state.
        machine.change_registry_externally(
            RegistryRoot::LocalMachine,
            DO_SUBKEY,
            "DODownloadMode",
            None,
        );

        let report = diff_latest(&machine, &facts, &registry, &database)
            .expect("diff")
            .expect("report");
        assert_eq!(
            entry(&report, DELIVERY_OPTIMIZATION_DOWNLOAD_MODE).status,
            DiffStatus::RevertedOutsideMinWin
        );
    }

    #[test]
    fn a_policy_value_set_to_a_third_value_is_changed_outside_minwin() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, database) = applied(&machine);
        machine.change_registry_externally(
            RegistryRoot::LocalMachine,
            DO_SUBKEY,
            "DODownloadMode",
            Some(2),
        );

        let report = diff_latest(&machine, &facts, &registry, &database)
            .expect("diff")
            .expect("report");
        let entry = entry(&report, DELIVERY_OPTIMIZATION_DOWNLOAD_MODE);
        assert_eq!(entry.status, DiffStatus::ChangedOutsideMinWin);
        assert!(entry.current.as_deref().unwrap().contains("private group"));
    }

    #[test]
    fn a_failed_change_is_reported_as_never_applied_and_is_not_inspected() {
        let machine = FakeMachine::windows_11()
            .elevated()
            .failing(FailOn::ServiceWrite("DiagTrack".into()));
        let (registry, facts, database) = applied(&machine);

        let report = diff_latest(&machine, &facts, &registry, &database)
            .expect("diff")
            .expect("report");
        let diagtrack = entry(&report, DIAGTRACK_START_TYPE);
        assert_eq!(diagtrack.status, DiffStatus::NotApplied);
        assert!(diagtrack.applied.is_none());
        assert!(!diagtrack.status.is_surprising());
    }

    #[test]
    fn an_offered_but_disabled_change_never_appears_in_the_diff() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, database) = applied(&machine);
        let report = diff_latest(&machine, &facts, &registry, &database)
            .expect("diff")
            .expect("report");
        assert!(
            !report
                .entries
                .iter()
                .any(|entry| entry.change_id == crate::changes::SYSMAIN_START_TYPE),
            "a change the profile never enabled must not be in the diff"
        );
    }

    #[test]
    fn an_unreadable_change_is_reported_rather_than_omitted() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, facts, database) = applied(&machine);

        // Remove the service entirely, so inspection fails.
        let machine = FakeMachine::windows_11()
            .elevated()
            .without_service("DiagTrack");
        let report = diff_session(
            &machine,
            &facts,
            &registry,
            &database,
            &database
                .latest_apply_session()
                .expect("session")
                .expect("some"),
        )
        .expect("diff");

        let diagtrack = entry(&report, DIAGTRACK_START_TYPE);
        assert_eq!(diagtrack.status, DiffStatus::UnableToInspect);
        assert!(
            diagtrack
                .inspection_error
                .as_deref()
                .unwrap()
                .contains("not installed")
        );
        assert!(diagtrack.status.is_surprising());
    }

    #[test]
    fn a_windows_upgrade_since_the_session_is_flagged() {
        let machine = FakeMachine::windows_11().elevated();
        let (registry, mut facts, database) = applied(&machine);
        assert!(
            !diff_latest(&machine, &facts, &registry, &database)
                .expect("diff")
                .expect("report")
                .windows_changed()
        );

        facts.windows.display_version = Some("25H2".into());
        let report = diff_latest(&machine, &facts, &registry, &database)
            .expect("diff")
            .expect("report");
        assert!(report.windows_changed());
        assert_eq!(report.current_windows_label, "Windows 11 25H2");
        assert_eq!(report.session_windows_label, "Windows 11 24H2");
    }

    #[test]
    fn classification_ignores_the_human_summary_and_uses_the_detail() {
        use serde_json::json;
        let before = ObservedState::new("Automatic", json!({"v": "auto"}));
        let applied = ObservedState::new("Manual", json!({"v": "manual"}));

        // Same detail, different wording: still unchanged.
        let current = ObservedState::new("Manual start", json!({"v": "manual"}));
        assert_eq!(
            classify(&current, Some(&applied), &before),
            DiffStatus::UnchangedSinceApply
        );

        // Same wording as applied, different detail: not unchanged.
        let impostor = ObservedState::new("Manual", json!({"v": "disabled"}));
        assert_eq!(
            classify(&impostor, Some(&applied), &before),
            DiffStatus::ChangedOutsideMinWin
        );
    }
}
