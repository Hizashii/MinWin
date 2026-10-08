//! What a MinWin change *is*.
//!
//! The five lifecycle stages are separate types rather than one mutable blob,
//! because each one answers a different question and each one is persisted at a
//! different moment:
//!
//! | Stage       | Question                                        |
//! |-------------|-------------------------------------------------|
//! | Applicability | May MinWin touch this on *this* machine?      |
//! | Inspection  | What is the current value?                       |
//! | Planning    | What will MinWin set, and how is it undone?      |
//! | Application | Did the write succeed?                           |
//! | Verification| Does the machine now read back as intended?      |
//!
//! Keeping planning separate from application is what makes `--dry-run` real
//! rather than a printout: the dry-run path runs the first three stages and
//! then stops.

use serde::{Deserialize, Serialize};

/// Why a user might or might not want a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeCategory {
    /// Diagnostic data collection and upload.
    Telemetry,
    /// Background network activity related to updates.
    UpdateDelivery,
    /// Memory and disk prefetching behaviour.
    MemoryManagement,
    /// Power and performance configuration.
    Power,
}

impl ChangeCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::Telemetry => "telemetry",
            Self::UpdateDelivery => "update delivery",
            Self::MemoryManagement => "memory management",
            Self::Power => "power",
        }
    }
}

/// MinWin's own risk rating.
///
/// This describes the chance of a *user-visible downside*, not the chance of
/// the write failing. A low-risk change can still have a tradeoff; see
/// [`ChangeMetadata::tradeoffs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Reversible, documented, and unlikely to be noticed in normal use.
    Low,
    /// Reversible and documented, but a user may notice the tradeoff.
    Medium,
}

impl Risk {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
        }
    }
}

/// Whether a restart is needed for a change to take full effect.
///
/// MinWin never reboots. It reports and lets the user choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebootRequirement {
    /// The change is fully in effect immediately.
    NoReboot,
    /// The new configuration is stored and will be honoured from the next
    /// restart; the currently running component is unaffected until then.
    RebootRecommended,
    /// The change has no observable effect at all until a restart.
    RebootRequired,
}

impl RebootRequirement {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoReboot => "no restart needed",
            Self::RebootRecommended => "restart recommended",
            Self::RebootRequired => "restart required",
        }
    }

    pub fn needs_restart(self) -> bool {
        !matches!(self, Self::NoReboot)
    }
}

/// Everything MinWin can tell a user about a change before touching anything.
///
/// The fields are `&'static str` because this is a compile-time registry: the
/// text is part of the binary, not data loaded at runtime. That is why the type
/// is `Serialize` (for `--json`) but not `Deserialize`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChangeMetadata {
    pub id: &'static str,
    pub name: &'static str,
    /// One line: what MinWin will do.
    pub description: &'static str,
    pub category: ChangeCategory,
    pub risk: Risk,
    pub requires_admin: bool,
    pub reboot: RebootRequirement,
    /// The Windows mechanism used, named precisely enough to be checked
    /// against Microsoft's documentation.
    pub mechanism: &'static str,
    /// Why this may reduce unnecessary background work.
    pub rationale: &'static str,
    /// What the user gives up. Never empty: a change with no downside at all
    /// would not need a risk rating.
    pub tradeoffs: &'static str,
    /// The minimum Windows build MinWin will apply this on.
    pub minimum_build: u32,
}

/// Whether a change may be applied on this machine, right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Applicability {
    Applicable,
    /// This machine is fine, but the change is pointless or inappropriate here
    /// — for example the service is not installed, or the device is managed.
    NotApplicable {
        reason: String,
    },
    /// This Windows version is outside what MinWin has validated.
    Unsupported {
        reason: String,
    },
    /// Everything is fine except the process's privileges.
    RequiresElevation,
}

impl Applicability {
    pub fn is_applicable(&self) -> bool {
        matches!(self, Self::Applicable)
    }

    pub fn not_applicable(reason: impl Into<String>) -> Self {
        Self::NotApplicable {
            reason: reason.into(),
        }
    }

    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported {
            reason: reason.into(),
        }
    }

    /// The sentence the CLI prints when a change is skipped.
    pub fn explain(&self) -> String {
        match self {
            Self::Applicable => "applicable".to_string(),
            Self::NotApplicable { reason } => reason.clone(),
            Self::Unsupported { reason } => reason.clone(),
            Self::RequiresElevation => {
                "requires an elevated terminal (Run as Administrator)".to_string()
            }
        }
    }
}

/// A reading of the machine for one change.
///
/// `summary` is for humans; `detail` is the machine-comparable form. Diff and
/// rollback compare `detail` by equality and never parse `summary`, so changing
/// a human string can never alter MinWin's logic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedState {
    pub summary: String,
    pub detail: serde_json::Value,
}

impl ObservedState {
    pub fn new(summary: impl Into<String>, detail: serde_json::Value) -> Self {
        Self {
            summary: summary.into(),
            detail,
        }
    }

    /// Equality on the machine-readable form only.
    pub fn matches(&self, other: &ObservedState) -> bool {
        self.detail == other.detail
    }
}

/// What restoring this change requires.
///
/// Produced during planning, from the *observed* pre-change state, and
/// persisted before any write happens. That ordering is the reason an
/// interrupted apply is still recoverable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackData {
    pub summary: String,
    pub detail: serde_json::Value,
}

impl RollbackData {
    pub fn new(summary: impl Into<String>, detail: serde_json::Value) -> Self {
        Self {
            summary: summary.into(),
            detail,
        }
    }
}

/// Whether planning found anything to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanAction {
    /// The machine does not currently match the target.
    Modify,
    /// The machine is already in the target state. MinWin records this and
    /// writes nothing, so re-running `apply` is harmless.
    AlreadyCompliant,
}

/// A fully-formed intention, ready to display or execute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangePlan {
    pub change_id: String,
    pub action: PlanAction,
    pub current: ObservedState,
    pub target: ObservedState,
    pub rollback: RollbackData,
    pub reboot: RebootRequirement,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplyResult {
    pub reboot: RebootRequirement,
    /// What the change wants to tell the user about what just happened, beyond
    /// the before/after values.
    pub note: Option<String>,
}

/// The result of reading the machine back after a write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum VerificationResult {
    /// The machine reads back as intended.
    Verified { observed: ObservedState },
    /// The write reported success but the machine does not match the target.
    /// This is a genuine failure, not a warning.
    Mismatch {
        expected: ObservedState,
        observed: ObservedState,
    },
    /// MinWin could not read the state back. The change may or may not have
    /// taken; MinWin says so rather than assuming success.
    Unverifiable { reason: String },
}

impl VerificationResult {
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }

    pub fn explain(&self) -> String {
        match self {
            Self::Verified { observed } => format!("verified as {}", observed.summary),
            Self::Mismatch { expected, observed } => format!(
                "expected {} but the system reports {}",
                expected.summary, observed.summary
            ),
            Self::Unverifiable { reason } => format!("could not verify: {reason}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackResult {
    pub restored: ObservedState,
    pub reboot: RebootRequirement,
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn observed_states_compare_on_the_machine_readable_detail_only() {
        let a = ObservedState::new("Automatic", json!({"start_type": "automatic"}));
        let b = ObservedState::new(
            "Automatic (different wording)",
            json!({"start_type": "automatic"}),
        );
        let c = ObservedState::new("Automatic", json!({"start_type": "manual"}));

        assert!(a.matches(&b), "summary text must not affect equality");
        assert!(!a.matches(&c));
    }

    #[test]
    fn applicability_always_carries_a_reason_the_cli_can_print() {
        assert_eq!(Applicability::Applicable.explain(), "applicable");
        assert_eq!(
            Applicability::not_applicable("the service is not installed").explain(),
            "the service is not installed"
        );
        assert!(
            Applicability::RequiresElevation
                .explain()
                .contains("Administrator")
        );
        assert!(!Applicability::RequiresElevation.is_applicable());
    }

    #[test]
    fn reboot_requirements_report_whether_a_restart_is_needed() {
        assert!(!RebootRequirement::NoReboot.needs_restart());
        assert!(RebootRequirement::RebootRecommended.needs_restart());
        assert!(RebootRequirement::RebootRequired.needs_restart());
    }

    #[test]
    fn a_mismatch_is_not_a_verification() {
        let expected = ObservedState::new("Manual", json!("manual"));
        let observed = ObservedState::new("Automatic", json!("automatic"));
        let mismatch = VerificationResult::Mismatch {
            expected: expected.clone(),
            observed,
        };
        assert!(!mismatch.is_verified());
        assert!(mismatch.explain().contains("expected Manual"));

        assert!(VerificationResult::Verified { observed: expected }.is_verified());
    }

    #[test]
    fn unverifiable_states_explain_themselves_rather_than_claiming_success() {
        let result = VerificationResult::Unverifiable {
            reason: "access is denied".into(),
        };
        assert!(!result.is_verified());
        assert!(result.explain().contains("could not verify"));
    }

    #[test]
    fn lifecycle_types_survive_a_json_round_trip() {
        // These are persisted as JSON columns, so a schema change that breaks
        // deserialisation must fail here rather than on a user's machine.
        let plan = ChangePlan {
            change_id: "telemetry.diagtrack_start_type".into(),
            action: PlanAction::Modify,
            current: ObservedState::new("Automatic", json!({"start_type": "automatic"})),
            target: ObservedState::new("Manual", json!({"start_type": "manual"})),
            rollback: RollbackData::new("Automatic", json!({"start_type": "automatic"})),
            reboot: RebootRequirement::RebootRecommended,
        };
        let encoded = serde_json::to_string(&plan).expect("encode");
        let decoded: ChangePlan = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, plan);
    }
}
