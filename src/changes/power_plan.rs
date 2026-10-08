//! Selecting the active Windows power plan.
//!
//! MinWin switches *between existing plans*. It does not create plans, and it
//! does not edit the settings inside one — so it cannot leave a user with a
//! plan called "Balanced" that no longer behaves like Balanced.
//!
//! Two things make this change unusual in MinWin's small set, and both are
//! worth noting:
//!
//! * **It needs no elevation.** `PowerSetActiveScheme` acts on the calling
//!   user's active scheme, so this is the one change a non-administrator can
//!   apply. MinWin reflects that rather than demanding admin for everything.
//! * **The target plan may not exist.** Many Windows 11 systems, particularly
//!   laptops using modern standby, never have the High performance plan
//!   created. MinWin enumerates the installed plans and reports the change as
//!   not applicable instead of failing at apply time.

use serde::{Deserialize, Serialize};

use crate::changes::SystemChange;
use crate::changes::model::{
    Applicability, ApplyResult, ChangeMetadata, ChangePlan, ObservedState, PlanAction,
    RollbackData, RollbackResult, VerificationResult,
};
use crate::core::error::Result;
use crate::sys::SystemFacts;
use crate::sys::model::PowerSchemeId;
use crate::sys::traits::Machine;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PowerStateDetail {
    scheme_id: String,
    /// Stored for display only. Plan names are localised, so MinWin compares
    /// GUIDs and never names.
    scheme_name: String,
}

pub struct PowerPlanChange {
    metadata: ChangeMetadata,
    /// Resolved lazily from the documented `GUID_*_POWER_SAVINGS` constants
    /// rather than stored as a literal.
    target: fn() -> PowerSchemeId,
}

impl PowerPlanChange {
    pub const fn new(metadata: ChangeMetadata, target: fn() -> PowerSchemeId) -> Self {
        Self { metadata, target }
    }

    fn target_id(&self) -> PowerSchemeId {
        (self.target)()
    }

    fn observed(&self, id: &PowerSchemeId, name: &str) -> ObservedState {
        ObservedState::new(
            name.to_string(),
            serde_json::to_value(PowerStateDetail {
                scheme_id: id.as_str().to_string(),
                scheme_name: name.to_string(),
            })
            .unwrap_or(serde_json::Value::Null),
        )
    }

    fn decode(&self, detail: &serde_json::Value) -> Result<PowerSchemeId> {
        let decoded: PowerStateDetail = serde_json::from_value(detail.clone())?;
        PowerSchemeId::parse(&decoded.scheme_id).ok_or_else(|| {
            crate::core::error::MinWinError::Unsupported(format!(
                "recorded power plan id {:?} is not a valid GUID",
                decoded.scheme_id
            ))
        })
    }

    fn name_of(&self, machine: &dyn Machine, id: &PowerSchemeId) -> Result<Option<String>> {
        Ok(machine
            .power()
            .list_schemes()?
            .into_iter()
            .find(|scheme| &scheme.id == id)
            .map(|scheme| scheme.name))
    }
}

impl SystemChange for PowerPlanChange {
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

        let target = self.target_id();
        if self.name_of(machine, &target)?.is_none() {
            return Ok(Applicability::not_applicable(format!(
                "the target power plan ({target}) is not installed on this system, which is normal on devices that use modern standby"
            )));
        }

        Ok(Applicability::Applicable)
    }

    fn inspect(&self, machine: &dyn Machine, _facts: &SystemFacts) -> Result<ObservedState> {
        let active = machine.power().active_scheme()?;
        Ok(self.observed(&active.id, &active.name))
    }

    fn plan(&self, machine: &dyn Machine, _facts: &SystemFacts) -> Result<ChangePlan> {
        let active = machine.power().active_scheme()?;
        let target = self.target_id();
        let target_name = self
            .name_of(machine, &target)?
            .unwrap_or_else(|| target.to_string());

        let current = self.observed(&active.id, &active.name);

        Ok(ChangePlan {
            change_id: self.metadata.id.to_string(),
            action: if active.id == target {
                PlanAction::AlreadyCompliant
            } else {
                PlanAction::Modify
            },
            rollback: RollbackData::new(active.name.clone(), current.detail.clone()),
            current,
            target: self.observed(&target, &target_name),
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
        machine.power().set_active_scheme(&target)?;
        Ok(ApplyResult {
            reboot: self.metadata.reboot,
            note: None,
        })
    }

    fn verify(
        &self,
        machine: &dyn Machine,
        _facts: &SystemFacts,
        expected: &ObservedState,
    ) -> Result<VerificationResult> {
        // Compare GUIDs, not the rendered names, because the name is localised
        // and could differ between the plan list and the active scheme.
        let expected_id = match self.decode(&expected.detail) {
            Ok(id) => id,
            Err(error) => {
                return Ok(VerificationResult::Unverifiable {
                    reason: error.to_string(),
                });
            }
        };

        match machine.power().active_scheme() {
            Ok(active) if active.id == expected_id => Ok(VerificationResult::Verified {
                observed: self.observed(&active.id, &active.name),
            }),
            Ok(active) => Ok(VerificationResult::Mismatch {
                expected: expected.clone(),
                observed: self.observed(&active.id, &active.name),
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
        machine.power().set_active_scheme(&original)?;
        let name = self
            .name_of(machine, &original)?
            .unwrap_or_else(|| original.to_string());
        Ok(RollbackResult {
            restored: self.observed(&original, &name),
            reboot: self.metadata.reboot,
            note: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::model::{ChangeCategory, RebootRequirement, Risk};
    use crate::sys::fake::FakeMachine;
    use serde_json::json;

    fn high_performance() -> PowerSchemeId {
        PowerSchemeId::parse("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c").expect("guid")
    }

    fn balanced() -> PowerSchemeId {
        PowerSchemeId::parse("381b4222-f694-41f0-9685-ff5bb260df2e").expect("guid")
    }

    fn change() -> PowerPlanChange {
        PowerPlanChange::new(
            ChangeMetadata {
                id: "test.power",
                name: "Test power plan",
                description: "test",
                category: ChangeCategory::Power,
                risk: Risk::Low,
                requires_admin: false,
                reboot: RebootRequirement::NoReboot,
                mechanism: "PowerSetActiveScheme",
                rationale: "test",
                tradeoffs: "test",
                minimum_build: 22000,
            },
            high_performance,
        )
    }

    fn facts(machine: &FakeMachine) -> SystemFacts {
        SystemFacts::gather(machine).expect("facts")
    }

    #[test]
    fn this_change_does_not_require_elevation() {
        // Deliberately a non-elevated machine.
        let machine = FakeMachine::windows_11();
        assert_eq!(
            change()
                .check_applicability(&machine, &facts(&machine))
                .expect("check"),
            Applicability::Applicable
        );
    }

    #[test]
    fn a_machine_without_the_target_plan_is_not_applicable() {
        let machine = FakeMachine::windows_11().without_power_scheme(&high_performance());
        let applicability = change()
            .check_applicability(&machine, &facts(&machine))
            .expect("check");
        assert!(!applicability.is_applicable());
        assert!(applicability.explain().contains("modern standby"));
    }

    #[test]
    fn planning_records_the_current_plan_for_rollback() {
        let machine = FakeMachine::windows_11();
        let plan = change().plan(&machine, &facts(&machine)).expect("plan");
        assert_eq!(plan.action, PlanAction::Modify);
        assert_eq!(plan.current.summary, "Balanced");
        assert_eq!(plan.target.summary, "High performance");
        assert_eq!(plan.rollback.summary, "Balanced");
        assert!(machine.writes().is_empty());
    }

    #[test]
    fn apply_verify_and_rollback_restore_the_original_plan() {
        let machine = FakeMachine::windows_11();
        let facts = facts(&machine);
        let change = change();
        let plan = change.plan(&machine, &facts).expect("plan");

        change.apply(&machine, &facts, &plan).expect("apply");
        assert_eq!(machine.active_power_scheme_id(), Some(high_performance()));
        assert!(
            change
                .verify(&machine, &facts, &plan.target)
                .expect("verify")
                .is_verified()
        );

        change
            .rollback(&machine, &facts, &plan.rollback)
            .expect("rollback");
        assert_eq!(machine.active_power_scheme_id(), Some(balanced()));
    }

    #[test]
    fn verification_compares_guids_not_localised_names() {
        let machine = FakeMachine::windows_11();
        let facts = facts(&machine);
        let change = change();
        let plan = change.plan(&machine, &facts).expect("plan");
        change.apply(&machine, &facts, &plan).expect("apply");

        // An expectation carrying the right GUID but a different display name
        // must still verify.
        let renamed = ObservedState::new(
            "Hohe Leistung",
            json!({
                "scheme_id": high_performance().as_str(),
                "scheme_name": "Hohe Leistung",
            }),
        );
        assert!(
            change
                .verify(&machine, &facts, &renamed)
                .expect("verify")
                .is_verified()
        );
    }

    #[test]
    fn a_machine_already_on_the_target_plan_is_already_compliant() {
        let machine = FakeMachine::windows_11();
        machine
            .power()
            .set_active_scheme(&high_performance())
            .expect("seed");
        let plan = change().plan(&machine, &facts(&machine)).expect("plan");
        assert_eq!(plan.action, PlanAction::AlreadyCompliant);
    }

    #[test]
    fn a_failed_activation_surfaces_as_an_error() {
        use crate::sys::fake::FailOn;
        let machine = FakeMachine::windows_11().failing(FailOn::PowerSchemeWrite);
        let facts = facts(&machine);
        let change = change();
        let plan = change.plan(&machine, &facts).expect("plan");
        assert!(change.apply(&machine, &facts, &plan).is_err());
        assert_eq!(machine.active_power_scheme_id(), Some(balanced()));
    }

    #[test]
    fn a_corrupt_recorded_guid_is_unverifiable_rather_than_a_silent_pass() {
        let machine = FakeMachine::windows_11();
        let facts = facts(&machine);
        let broken = ObservedState::new(
            "nonsense",
            json!({"scheme_id": "not-a-guid", "scheme_name": "nonsense"}),
        );
        let verification = change().verify(&machine, &facts, &broken).expect("verify");
        assert!(matches!(
            verification,
            VerificationResult::Unverifiable { .. }
        ));
    }
}
