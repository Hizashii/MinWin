//! Changes that set a documented Group Policy registry DWORD.
//!
//! The interesting part of this change kind is the *absent* case. A policy
//! value that has never been configured does not exist in the registry, and
//! Windows then uses its own default. Restoring such a value therefore means
//! **deleting** it, not writing a zero — writing zero would leave the machine
//! holding an explicit policy it never had, which would be a silent,
//! permanent change disguised as a rollback.
//!
//! So the recorded state here is `Option<u32>`, and rollback branches on it.
//!
//! The key path and value name are `&'static str` fields of the change itself.
//! No profile, CLI argument or file content can reach them, which is what stops
//! a TOML file from addressing an arbitrary registry location.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::changes::SystemChange;
use crate::changes::model::{
    Applicability, ApplyResult, ChangeMetadata, ChangePlan, ObservedState, PlanAction,
    RollbackData, RollbackResult, VerificationResult,
};
use crate::core::error::Result;
use crate::sys::SystemFacts;
use crate::sys::model::{RegistryRoot, RegistryValue};
use crate::sys::traits::Machine;

/// The persisted state: which value, and what it held — including "nothing".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PolicyStateDetail {
    subkey: String,
    value_name: String,
    /// `None` means the value was not present.
    value: Option<u32>,
}

/// Describes what a particular DWORD setting means, so the CLI can render a
/// number as something a person can evaluate.
pub struct PolicyDwordChange {
    metadata: ChangeMetadata,
    root: RegistryRoot,
    subkey: &'static str,
    value_name: &'static str,
    target_value: u32,
    /// Renders a value (or its absence) in product terms, e.g.
    /// `0 (HTTP only, no peer-to-peer)`.
    describe_value: fn(Option<u32>) -> String,
}

impl PolicyDwordChange {
    pub const fn new(
        metadata: ChangeMetadata,
        root: RegistryRoot,
        subkey: &'static str,
        value_name: &'static str,
        target_value: u32,
        describe_value: fn(Option<u32>) -> String,
    ) -> Self {
        Self {
            metadata,
            root,
            subkey,
            value_name,
            target_value,
            describe_value,
        }
    }

    /// The full registry location, for display and for error messages.
    pub fn location(&self) -> String {
        format!(
            r"{}\{}\{}",
            self.root.short_name(),
            self.subkey,
            self.value_name
        )
    }

    fn observed(&self, value: Option<u32>) -> ObservedState {
        ObservedState::new(
            (self.describe_value)(value),
            serde_json::to_value(PolicyStateDetail {
                subkey: self.subkey.to_string(),
                value_name: self.value_name.to_string(),
                value,
            })
            // Owned primitives only; serialisation cannot fail.
            .unwrap_or(serde_json::Value::Null),
        )
    }

    fn decode(&self, detail: &serde_json::Value) -> Result<Option<u32>> {
        let decoded: PolicyStateDetail = serde_json::from_value(detail.clone())?;
        if decoded.subkey != self.subkey || decoded.value_name != self.value_name {
            return Err(crate::core::error::MinWinError::Unsupported(format!(
                r"recorded state names {}\{} but change {} manages {}",
                decoded.subkey,
                decoded.value_name,
                self.metadata.id,
                self.location()
            )));
        }
        Ok(decoded.value)
    }

    /// Reads the value, treating a present-but-wrong-type value as a condition
    /// worth reporting rather than as absence.
    fn read(&self, machine: &dyn Machine) -> Result<ReadOutcome> {
        match machine
            .registry()
            .read_value(self.root, self.subkey, self.value_name)?
        {
            None => Ok(ReadOutcome::Absent),
            Some(RegistryValue::Dword(value)) => Ok(ReadOutcome::Dword(value)),
            Some(other) => Ok(ReadOutcome::WrongType(other.describe())),
        }
    }
}

enum ReadOutcome {
    Absent,
    Dword(u32),
    WrongType(String),
}

impl SystemChange for PolicyDwordChange {
    fn metadata(&self) -> &ChangeMetadata {
        &self.metadata
    }

    fn check_applicability(
        &self,
        machine: &dyn Machine,
        facts: &SystemFacts,
    ) -> Result<Applicability> {
        if facts.windows.build < self.metadata.minimum_build {
            return Ok(Applicability::unsupported(format!(
                "MinWin has only validated this change on Windows build {} and later; this machine reports build {}",
                self.metadata.minimum_build, facts.windows.build
            )));
        }

        // A value of an unexpected type means something other than MinWin owns
        // this setting. MinWin declines rather than overwriting it.
        if let ReadOutcome::WrongType(description) = self.read(machine)? {
            return Ok(Applicability::not_applicable(format!(
                "{} already holds a {}, which MinWin will not overwrite",
                self.location(),
                description
            )));
        }

        // Elevation is not checked here: reading the value needs no
        // privileges, so planning works from an ordinary terminal. The planner
        // decides whether the write may proceed. See the note in
        // `service_start_type`.
        Ok(Applicability::Applicable)
    }

    fn inspect(&self, machine: &dyn Machine, _facts: &SystemFacts) -> Result<ObservedState> {
        match self.read(machine)? {
            ReadOutcome::Absent => Ok(self.observed(None)),
            ReadOutcome::Dword(value) => Ok(self.observed(Some(value))),
            ReadOutcome::WrongType(description) => Ok(ObservedState::new(
                format!("unexpected: {description}"),
                json!({
                    "subkey": self.subkey,
                    "value_name": self.value_name,
                    "unexpected_type": description,
                }),
            )),
        }
    }

    fn plan(&self, machine: &dyn Machine, _facts: &SystemFacts) -> Result<ChangePlan> {
        let current_value = match self.read(machine)? {
            ReadOutcome::Absent => None,
            ReadOutcome::Dword(value) => Some(value),
            ReadOutcome::WrongType(description) => {
                return Err(crate::core::error::MinWinError::Unsupported(format!(
                    "{} holds a {}; MinWin declines to plan a change for it",
                    self.location(),
                    description
                )));
            }
        };
        let current = self.observed(current_value);
        let target = self.observed(Some(self.target_value));

        Ok(ChangePlan {
            change_id: self.metadata.id.to_string(),
            action: if current_value == Some(self.target_value) {
                PlanAction::AlreadyCompliant
            } else {
                PlanAction::Modify
            },
            rollback: RollbackData::new(
                match current_value {
                    Some(_) => (self.describe_value)(current_value),
                    // Spelled out, because this is the case that would
                    // otherwise be rolled back incorrectly.
                    None => format!("not configured (MinWin will delete {})", self.location()),
                },
                current.detail.clone(),
            ),
            current,
            target,
            reboot: self.metadata.reboot,
        })
    }

    fn apply(
        &self,
        machine: &dyn Machine,
        _facts: &SystemFacts,
        plan: &ChangePlan,
    ) -> Result<ApplyResult> {
        let target = self.decode(&plan.target.detail)?;
        match target {
            Some(value) => {
                machine
                    .registry()
                    .write_dword(self.root, self.subkey, self.value_name, value)?
            }
            // A target of "absent" is not something any registered change
            // declares, but handling it keeps apply and rollback symmetric.
            None => machine
                .registry()
                .delete_value(self.root, self.subkey, self.value_name)?,
        }
        Ok(ApplyResult {
            reboot: self.metadata.reboot,
            note: None,
        })
    }

    fn verify(
        &self,
        machine: &dyn Machine,
        facts: &SystemFacts,
        expected: &ObservedState,
    ) -> Result<VerificationResult> {
        match self.inspect(machine, facts) {
            Ok(observed) if observed.matches(expected) => {
                Ok(VerificationResult::Verified { observed })
            }
            Ok(observed) => Ok(VerificationResult::Mismatch {
                expected: expected.clone(),
                observed,
            }),
            Err(error) => Ok(VerificationResult::Unverifiable {
                reason: error.to_string(),
            }),
        }
    }

    fn rollback(
        &self,
        machine: &dyn Machine,
        _facts: &SystemFacts,
        rollback: &RollbackData,
    ) -> Result<RollbackResult> {
        let original = self.decode(&rollback.detail)?;
        let note = match original {
            Some(value) => {
                machine
                    .registry()
                    .write_dword(self.root, self.subkey, self.value_name, value)?;
                None
            }
            None => {
                // This is the branch that makes rollback honest: the value was
                // never configured, so restoring means removing it and letting
                // Windows apply its own default again.
                machine
                    .registry()
                    .delete_value(self.root, self.subkey, self.value_name)?;
                Some(format!(
                    "{} was not configured before MinWin ran, so it has been removed and Windows' own default applies again",
                    self.location()
                ))
            }
        };
        Ok(RollbackResult {
            restored: self.observed(original),
            reboot: self.metadata.reboot,
            note,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::model::{ChangeCategory, RebootRequirement, Risk};
    use crate::sys::fake::FakeMachine;

    const SUBKEY: &str = r"SOFTWARE\Policies\Microsoft\Windows\DeliveryOptimization";
    const VALUE: &str = "DODownloadMode";

    fn describe(value: Option<u32>) -> String {
        match value {
            None => "not configured (Windows default)".to_string(),
            Some(0) => "0 (HTTP only)".to_string(),
            Some(other) => format!("{other} (other mode)"),
        }
    }

    fn change() -> PolicyDwordChange {
        PolicyDwordChange::new(
            ChangeMetadata {
                id: "test.policy",
                name: "Test policy",
                description: "test",
                category: ChangeCategory::UpdateDelivery,
                risk: Risk::Low,
                requires_admin: true,
                reboot: RebootRequirement::NoReboot,
                mechanism: "registry policy value",
                rationale: "test",
                tradeoffs: "test",
                minimum_build: 22000,
            },
            RegistryRoot::LocalMachine,
            SUBKEY,
            VALUE,
            0,
            describe,
        )
    }

    fn facts(machine: &FakeMachine) -> SystemFacts {
        SystemFacts::gather(machine).expect("facts")
    }

    #[test]
    fn an_unconfigured_policy_reads_as_absent_not_as_zero() {
        let machine = FakeMachine::windows_11().elevated();
        let observed = change()
            .inspect(&machine, &facts(&machine))
            .expect("inspect");
        assert_eq!(observed.summary, "not configured (Windows default)");
        assert_eq!(observed.detail["value"], serde_json::Value::Null);
    }

    #[test]
    fn rollback_of_a_previously_unconfigured_policy_deletes_the_value() {
        // The critical case: MinWin must not leave an explicit 1 or 0 behind.
        let machine = FakeMachine::windows_11().elevated();
        let facts = facts(&machine);
        let change = change();

        let plan = change.plan(&machine, &facts).expect("plan");
        assert_eq!(plan.action, PlanAction::Modify);
        assert!(plan.rollback.summary.contains("not configured"));
        assert!(plan.rollback.summary.contains("will delete"));

        change.apply(&machine, &facts, &plan).expect("apply");
        assert_eq!(
            machine.registry_dword(RegistryRoot::LocalMachine, SUBKEY, VALUE),
            Some(0)
        );

        let result = change
            .rollback(&machine, &facts, &plan.rollback)
            .expect("rollback");
        assert_eq!(
            machine.registry_dword(RegistryRoot::LocalMachine, SUBKEY, VALUE),
            None,
            "the value must be removed, not set to zero"
        );
        assert!(result.note.expect("note").contains("removed"));
    }

    #[test]
    fn rollback_of_a_previously_configured_policy_restores_that_exact_value() {
        let machine = FakeMachine::windows_11().elevated().with_registry_dword(
            RegistryRoot::LocalMachine,
            SUBKEY,
            VALUE,
            3,
        );
        let facts = facts(&machine);
        let change = change();

        let plan = change.plan(&machine, &facts).expect("plan");
        assert_eq!(plan.current.summary, "3 (other mode)");

        change.apply(&machine, &facts, &plan).expect("apply");
        change
            .rollback(&machine, &facts, &plan.rollback)
            .expect("rollback");

        assert_eq!(
            machine.registry_dword(RegistryRoot::LocalMachine, SUBKEY, VALUE),
            Some(3)
        );
    }

    #[test]
    fn a_machine_already_at_the_target_is_already_compliant() {
        let machine = FakeMachine::windows_11().elevated().with_registry_dword(
            RegistryRoot::LocalMachine,
            SUBKEY,
            VALUE,
            0,
        );
        let plan = change().plan(&machine, &facts(&machine)).expect("plan");
        assert_eq!(plan.action, PlanAction::AlreadyCompliant);
    }

    #[test]
    fn a_value_of_an_unexpected_type_is_declined_rather_than_overwritten() {
        // Something other than MinWin configured this setting, and not as a
        // DWORD. MinWin must step back instead of replacing it.
        let machine = FakeMachine::windows_11().elevated().with_registry_text(
            RegistryRoot::LocalMachine,
            SUBKEY,
            VALUE,
            "managed-elsewhere",
        );
        let change = change();

        let applicability = change
            .check_applicability(&machine, &facts(&machine))
            .expect("check");
        assert!(!applicability.is_applicable());
        assert!(applicability.explain().contains("will not overwrite"));

        // Inspection still reports what is there, rather than claiming absence.
        let observed = change.inspect(&machine, &facts(&machine)).expect("inspect");
        assert!(observed.summary.starts_with("unexpected:"));

        // And planning refuses outright, so no apply path can reach the value.
        assert!(change.plan(&machine, &facts(&machine)).is_err());
        assert!(machine.writes().is_empty());
    }

    #[test]
    fn applicability_does_not_depend_on_elevation_because_reading_is_read_only() {
        let machine = FakeMachine::windows_11();
        assert_eq!(
            change()
                .check_applicability(&machine, &facts(&machine))
                .expect("check"),
            Applicability::Applicable
        );
        let plan = change().plan(&machine, &facts(&machine)).expect("plan");
        assert_eq!(plan.current.summary, "not configured (Windows default)");
        assert!(machine.writes().is_empty());
        assert!(change().metadata().requires_admin);
    }

    #[test]
    fn planning_writes_nothing() {
        let machine = FakeMachine::windows_11().elevated();
        let _ = change().plan(&machine, &facts(&machine)).expect("plan");
        assert!(machine.writes().is_empty());
    }

    #[test]
    fn verification_confirms_the_written_value() {
        let machine = FakeMachine::windows_11().elevated();
        let facts = facts(&machine);
        let change = change();
        let plan = change.plan(&machine, &facts).expect("plan");

        let before = change
            .verify(&machine, &facts, &plan.target)
            .expect("verify");
        assert!(matches!(before, VerificationResult::Mismatch { .. }));

        change.apply(&machine, &facts, &plan).expect("apply");
        let after = change
            .verify(&machine, &facts, &plan.target)
            .expect("verify");
        assert!(after.is_verified());
    }

    #[test]
    fn a_failed_write_surfaces_as_an_error() {
        use crate::sys::fake::FailOn;
        let machine = FakeMachine::windows_11()
            .elevated()
            .failing(FailOn::RegistryWrite {
                subkey: SUBKEY.to_string(),
                value_name: VALUE.to_string(),
            });
        let facts = facts(&machine);
        let change = change();
        let plan = change.plan(&machine, &facts).expect("plan");

        assert!(change.apply(&machine, &facts, &plan).is_err());
        assert_eq!(
            machine.registry_dword(RegistryRoot::LocalMachine, SUBKEY, VALUE),
            None
        );
    }

    #[test]
    fn state_recorded_for_a_different_value_is_rejected() {
        let machine = FakeMachine::windows_11().elevated();
        let facts = facts(&machine);
        let foreign = RollbackData::new(
            "whatever",
            json!({"subkey": r"SOFTWARE\Elsewhere", "value_name": "Other", "value": 1}),
        );
        assert!(change().rollback(&machine, &facts, &foreign).is_err());
    }

    #[test]
    fn the_location_is_rendered_for_display() {
        assert_eq!(
            change().location(),
            r"HKLM\SOFTWARE\Policies\Microsoft\Windows\DeliveryOptimization\DODownloadMode"
        );
    }
}
