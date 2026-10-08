//! Changes that adjust a Windows service's start type.
//!
//! One implementation, parameterised by service and target, rather than a
//! copy-pasted module per service. Adding a validated service change is a
//! `const` declaration plus an applicability rule — not another 200 lines.
//!
//! What this never does: stop a running service, change its binary path,
//! account or dependencies, or set a start type of `Disabled`. `Manual` is the
//! strongest setting MinWin will apply, because it leaves the service
//! startable on demand — by Windows, by a troubleshooter, or by the user — and
//! therefore leaves the system recoverable without MinWin's help.

use serde::{Deserialize, Serialize};

use crate::changes::SystemChange;
use crate::changes::model::{
    Applicability, ApplyResult, ChangeMetadata, ChangePlan, ObservedState, PlanAction,
    RollbackData, RollbackResult, VerificationResult,
};
use crate::core::error::{MinWinError, Result};
use crate::sys::SystemFacts;
use crate::sys::model::ServiceStartType;
use crate::sys::traits::Machine;

/// An extra condition that must hold for a service change to be offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagementRule {
    /// Offer the change regardless of how the device is managed.
    Unrestricted,
    /// Decline on centrally managed devices, with this explanation.
    DeclineIfManaged { because: &'static str },
}

/// The persisted shape of this change's state. A struct rather than a bare
/// string so that a future field (the delayed-start flag is already modelled in
/// [`ServiceStartType`]) can be added without invalidating stored rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ServiceStateDetail {
    service: String,
    start_type: ServiceStartType,
}

pub struct ServiceStartTypeChange {
    metadata: ChangeMetadata,
    service_name: &'static str,
    target: ServiceStartType,
    management_rule: ManagementRule,
}

impl ServiceStartTypeChange {
    pub const fn new(
        metadata: ChangeMetadata,
        service_name: &'static str,
        target: ServiceStartType,
        management_rule: ManagementRule,
    ) -> Self {
        Self {
            metadata,
            service_name,
            target,
            management_rule,
        }
    }

    fn observed(&self, start_type: ServiceStartType) -> ObservedState {
        ObservedState::new(
            start_type.label(),
            serde_json::to_value(ServiceStateDetail {
                service: self.service_name.to_string(),
                start_type,
            })
            // A two-field struct of owned primitives cannot fail to serialise.
            .unwrap_or(serde_json::Value::Null),
        )
    }

    fn decode(&self, detail: &serde_json::Value) -> Result<ServiceStartType> {
        let decoded: ServiceStateDetail = serde_json::from_value(detail.clone())?;
        if decoded.service != self.service_name {
            return Err(MinWinError::Unsupported(format!(
                "recorded state names service {:?} but change {} manages {:?}",
                decoded.service, self.metadata.id, self.service_name
            )));
        }
        Ok(decoded.start_type)
    }
}

impl SystemChange for ServiceStartTypeChange {
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

        if let ManagementRule::DeclineIfManaged { because } = self.management_rule
            && facts.management.is_centrally_managed()
        {
            return Ok(Applicability::not_applicable(format!(
                "{} ({})",
                because,
                facts.management.describe()
            )));
        }

        // Ask the machine whether the service exists at all. A missing service
        // is a normal outcome, not an error.
        let config = match machine.services().query(self.service_name) {
            Ok(config) => config,
            Err(MinWinError::ServiceNotFound(_)) => {
                return Ok(Applicability::not_applicable(format!(
                    "the {} service is not installed on this system",
                    self.service_name
                )));
            }
            // Reading a service's configuration needs no privileges, so an
            // access denial here means something unusual; report it rather
            // than guessing.
            Err(other) => return Err(other),
        };

        if config.start_type.is_kernel_stage() {
            return Ok(Applicability::not_applicable(format!(
                "the {} service starts at {} time, which MinWin does not reconfigure",
                self.service_name,
                config.start_type.label()
            )));
        }

        // Elevation is deliberately *not* checked here. Reading a service's
        // configuration needs no privileges, so MinWin can inspect and plan
        // this change from an ordinary terminal — which is what makes
        // `--dry-run` useful without administrator rights. Whether the process
        // may actually write is decided by the planner, from
        // `requires_admin`.
        Ok(Applicability::Applicable)
    }

    fn inspect(&self, machine: &dyn Machine, _facts: &SystemFacts) -> Result<ObservedState> {
        let config = machine.services().query(self.service_name)?;
        Ok(self.observed(config.start_type))
    }

    fn plan(&self, machine: &dyn Machine, _facts: &SystemFacts) -> Result<ChangePlan> {
        let current_type = machine.services().query(self.service_name)?.start_type;
        let current = self.observed(current_type);
        let target = self.observed(self.target);

        Ok(ChangePlan {
            change_id: self.metadata.id.to_string(),
            action: if current_type == self.target {
                PlanAction::AlreadyCompliant
            } else {
                PlanAction::Modify
            },
            // Rollback restores whatever was actually there, including a
            // delayed-auto flag the user may have set themselves.
            rollback: RollbackData::new(current_type.label(), current.detail.clone()),
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
        machine
            .services()
            .set_start_type(self.service_name, target)?;

        // Changing the start type does not stop a service that is already
        // running, and MinWin deliberately does not stop it: terminating a
        // running system service is a far larger intervention than changing
        // how it starts next time.
        let note = machine
            .services()
            .query(self.service_name)
            .ok()
            .filter(|config| {
                matches!(
                    config.run_state,
                    crate::sys::model::ServiceRunState::Running
                )
            })
            .map(|_| {
                format!(
                    "{} is still running; the new start type applies from the next restart",
                    self.service_name
                )
            });

        Ok(ApplyResult {
            reboot: self.metadata.reboot,
            note,
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
        machine
            .services()
            .set_start_type(self.service_name, original)?;
        Ok(RollbackResult {
            restored: self.observed(original),
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
    use crate::sys::model::ManagementState;
    use serde_json::json;

    fn change() -> ServiceStartTypeChange {
        ServiceStartTypeChange::new(
            ChangeMetadata {
                id: "test.service",
                name: "Test service",
                description: "test",
                category: ChangeCategory::Telemetry,
                risk: Risk::Low,
                requires_admin: true,
                reboot: RebootRequirement::RebootRecommended,
                mechanism: "ChangeServiceConfigW",
                rationale: "test",
                tradeoffs: "test",
                minimum_build: 22000,
            },
            "DiagTrack",
            ServiceStartType::Manual,
            ManagementRule::DeclineIfManaged {
                because: "managed devices rely on this service",
            },
        )
    }

    fn facts(machine: &FakeMachine) -> SystemFacts {
        SystemFacts::gather(machine).expect("facts")
    }

    #[test]
    fn an_elevated_unmanaged_machine_is_applicable() {
        let machine = FakeMachine::windows_11().elevated();
        let applicability = change()
            .check_applicability(&machine, &facts(&machine))
            .expect("check");
        assert_eq!(applicability, Applicability::Applicable);
    }

    #[test]
    fn applicability_does_not_depend_on_elevation_because_inspection_is_read_only() {
        // A privilege problem is not an applicability problem. Keeping them
        // separate is what lets `--dry-run` show a full plan from an ordinary
        // terminal; the planner refuses the write using `requires_admin`.
        let machine = FakeMachine::windows_11();
        assert_eq!(
            change()
                .check_applicability(&machine, &facts(&machine))
                .expect("check"),
            Applicability::Applicable
        );
        // Planning works, and writes nothing.
        let plan = change().plan(&machine, &facts(&machine)).expect("plan");
        assert_eq!(plan.current.summary, "Automatic");
        assert_eq!(plan.target.summary, "Manual");
        assert!(machine.writes().is_empty());
        assert!(change().metadata().requires_admin);
    }

    #[test]
    fn a_managed_device_is_declined_with_the_reason_quoted() {
        let machine = FakeMachine::windows_11()
            .elevated()
            .managed(ManagementState {
                domain_joined: true,
                defender_for_endpoint_onboarded: false,
            });
        let applicability = change()
            .check_applicability(&machine, &facts(&machine))
            .expect("check");
        let explained = applicability.explain();
        assert!(!applicability.is_applicable());
        assert!(explained.contains("managed devices rely on this service"));
        assert!(explained.contains("Active Directory"));
    }

    #[test]
    fn a_missing_service_is_not_applicable_rather_than_an_error() {
        let machine = FakeMachine::windows_11()
            .elevated()
            .without_service("DiagTrack");
        let applicability = change()
            .check_applicability(&machine, &facts(&machine))
            .expect("a missing service must not be an error");
        assert!(applicability.explain().contains("not installed"));
    }

    #[test]
    fn a_kernel_stage_service_is_left_alone() {
        let machine = FakeMachine::windows_11()
            .elevated()
            .with_service_start_type("DiagTrack", ServiceStartType::Boot);
        let applicability = change()
            .check_applicability(&machine, &facts(&machine))
            .expect("check");
        assert!(applicability.explain().contains("Boot time"));
    }

    #[test]
    fn an_older_windows_build_is_unsupported() {
        let machine = FakeMachine::windows_11().elevated();
        let mut facts = facts(&machine);
        facts.windows.build = 19045;
        let applicability = change()
            .check_applicability(&machine, &facts)
            .expect("check");
        assert!(matches!(applicability, Applicability::Unsupported { .. }));
        assert!(applicability.explain().contains("19045"));
    }

    #[test]
    fn planning_captures_the_current_value_as_the_rollback_target() {
        let machine = FakeMachine::windows_11().elevated();
        let plan = change().plan(&machine, &facts(&machine)).expect("plan");

        assert_eq!(plan.action, PlanAction::Modify);
        assert_eq!(plan.current.summary, "Automatic");
        assert_eq!(plan.target.summary, "Manual");
        assert_eq!(plan.rollback.summary, "Automatic");
        assert_eq!(plan.rollback.detail, plan.current.detail);
    }

    #[test]
    fn planning_writes_nothing() {
        let machine = FakeMachine::windows_11().elevated();
        let _ = change().plan(&machine, &facts(&machine)).expect("plan");
        assert!(machine.writes().is_empty());
    }

    #[test]
    fn a_machine_already_in_the_target_state_is_already_compliant() {
        let machine = FakeMachine::windows_11()
            .elevated()
            .with_service_start_type("DiagTrack", ServiceStartType::Manual);
        let plan = change().plan(&machine, &facts(&machine)).expect("plan");
        assert_eq!(plan.action, PlanAction::AlreadyCompliant);
    }

    #[test]
    fn apply_then_verify_then_rollback_returns_the_original_value() {
        let machine = FakeMachine::windows_11().elevated();
        let facts = facts(&machine);
        let change = change();
        let plan = change.plan(&machine, &facts).expect("plan");

        change.apply(&machine, &facts, &plan).expect("apply");
        assert_eq!(
            machine.service_start_type("DiagTrack"),
            Some(ServiceStartType::Manual)
        );

        let verification = change
            .verify(&machine, &facts, &plan.target)
            .expect("verify");
        assert!(verification.is_verified());

        change
            .rollback(&machine, &facts, &plan.rollback)
            .expect("rollback");
        assert_eq!(
            machine.service_start_type("DiagTrack"),
            Some(ServiceStartType::Automatic)
        );
    }

    #[test]
    fn rollback_restores_a_delayed_auto_flag_rather_than_flattening_it() {
        // A user who had set delayed start must get delayed start back.
        let machine = FakeMachine::windows_11()
            .elevated()
            .with_service_start_type("DiagTrack", ServiceStartType::AutomaticDelayed);
        let facts = facts(&machine);
        let change = change();

        let plan = change.plan(&machine, &facts).expect("plan");
        change.apply(&machine, &facts, &plan).expect("apply");
        change
            .rollback(&machine, &facts, &plan.rollback)
            .expect("rollback");

        assert_eq!(
            machine.service_start_type("DiagTrack"),
            Some(ServiceStartType::AutomaticDelayed)
        );
    }

    #[test]
    fn verification_reports_a_mismatch_when_the_machine_disagrees() {
        let machine = FakeMachine::windows_11().elevated();
        let facts = facts(&machine);
        let change = change();
        let plan = change.plan(&machine, &facts).expect("plan");

        // Never applied, so the machine still reads Automatic.
        let verification = change
            .verify(&machine, &facts, &plan.target)
            .expect("verify");
        assert!(matches!(verification, VerificationResult::Mismatch { .. }));
    }

    #[test]
    fn a_failed_write_surfaces_as_an_error_and_changes_nothing() {
        use crate::sys::fake::FailOn;
        let machine = FakeMachine::windows_11()
            .elevated()
            .failing(FailOn::ServiceWrite("DiagTrack".into()));
        let facts = facts(&machine);
        let change = change();
        let plan = change.plan(&machine, &facts).expect("plan");

        let error = change
            .apply(&machine, &facts, &plan)
            .expect_err("apply should fail");
        assert!(error.is_privilege_problem());
        assert_eq!(
            machine.service_start_type("DiagTrack"),
            Some(ServiceStartType::Automatic)
        );
    }

    #[test]
    fn state_recorded_for_a_different_service_is_rejected() {
        let machine = FakeMachine::windows_11().elevated();
        let facts = facts(&machine);
        let change = change();
        let foreign = RollbackData::new(
            "Automatic",
            json!({"service": "SomethingElse", "start_type": "automatic"}),
        );
        assert!(change.rollback(&machine, &facts, &foreign).is_err());
    }
}
