//! Rows as Rust types.

use serde::{Deserialize, Serialize};

use crate::changes::model::{
    ObservedState, RebootRequirement, Risk, RollbackData, VerificationResult,
};

/// The lifecycle of one change within an apply session.
///
/// The intermediate states exist so that a crash is diagnosable. `Applying`
/// persisted on disk means MinWin was interrupted between the write and the
/// confirmation, which is exactly the situation `status` must be able to
/// report and a future `recover` must be able to resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    /// Recorded, with its rollback data, before anything was written.
    Planned,
    /// The write has been issued and not yet confirmed.
    Applying,
    /// The write succeeded; verification has not run.
    Applied,
    /// The write succeeded and the machine reads back as intended.
    Verified,
    /// The write or the verification failed. The rollback data is still valid.
    Failed,
    /// The machine already matched the target, so nothing was written.
    AlreadyCompliant,
    /// A rollback has restored this change.
    RolledBack,
}

impl ChangeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Applying => "applying",
            Self::Applied => "applied",
            Self::Verified => "verified",
            Self::Failed => "failed",
            Self::AlreadyCompliant => "already_compliant",
            Self::RolledBack => "rolled_back",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "planned" => Self::Planned,
            "applying" => Self::Applying,
            "applied" => Self::Applied,
            "verified" => Self::Verified,
            "failed" => Self::Failed,
            "already_compliant" => Self::AlreadyCompliant,
            "rolled_back" => Self::RolledBack,
            _ => return None,
        })
    }

    /// Whether this change left the machine altered, and so is something
    /// rollback should restore.
    pub fn is_restorable(self) -> bool {
        matches!(self, Self::Applying | Self::Applied | Self::Verified)
    }

    /// Whether this status means MinWin was interrupted mid-write.
    pub fn is_incomplete(self) -> bool {
        matches!(self, Self::Planned | Self::Applying)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Started and not finished. Seeing this on disk means a previous MinWin
    /// run did not complete.
    InProgress,
    Completed,
    /// Finished, but at least one change failed.
    CompletedWithFailures,
    /// Stopped before applying everything it planned.
    Stopped,
    /// Every applied change has been rolled back.
    RolledBack,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::CompletedWithFailures => "completed_with_failures",
            Self::Stopped => "stopped",
            Self::RolledBack => "rolled_back",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            "completed_with_failures" => Self::CompletedWithFailures,
            "stopped" => Self::Stopped,
            "rolled_back" => Self::RolledBack,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::InProgress => "incomplete",
            Self::Completed => "completed",
            Self::CompletedWithFailures => "completed with failures",
            Self::Stopped => "stopped early",
            Self::RolledBack => "rolled back",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackStatus {
    Restored,
    /// The machine already held the original value, so nothing was written.
    AlreadyOriginal,
    Failed,
    /// MinWin declined to restore this change, for example because the value
    /// had been changed outside MinWin and the user did not authorise it.
    Skipped,
}

impl RollbackStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Restored => "restored",
            Self::AlreadyOriginal => "already_original",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "restored" => Self::Restored,
            "already_original" => Self::AlreadyOriginal,
            "failed" => Self::Failed,
            "skipped" => Self::Skipped,
            _ => return None,
        })
    }
}

/// An `apply_sessions` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplySessionRecord {
    pub id: i64,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub status: SessionStatus,
    pub profile_id: String,
    pub profile_name: String,
    pub profile_source: String,
    pub minwin_version: String,
    pub windows_label: String,
    pub windows_build: u32,
    pub elevated: bool,
}

/// A `change_records` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub id: i64,
    pub session_id: i64,
    pub change_id: String,
    pub order_index: u32,
    pub status: ChangeStatus,
    pub risk: Risk,
    pub reboot: RebootRequirement,
    pub state_before: ObservedState,
    pub state_planned: ObservedState,
    pub rollback: RollbackData,
    pub state_after: Option<ObservedState>,
    pub verification: Option<VerificationResult>,
    pub error: Option<String>,
    pub applied_at: Option<chrono::DateTime<chrono::Utc>>,
    pub verified_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A `rollback_sessions` row together with its records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackSessionRecord {
    pub id: i64,
    pub apply_session_id: i64,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub status: SessionStatus,
}

/// Counts used by `status` so it does not have to load whole sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionCounts {
    pub total: u32,
    pub verified: u32,
    pub applied_unverified: u32,
    pub failed: u32,
    pub already_compliant: u32,
    pub rolled_back: u32,
    pub incomplete: u32,
}

impl SessionCounts {
    /// Changes that currently hold the machine away from its original state.
    pub fn active(&self) -> u32 {
        self.verified + self.applied_unverified
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_statuses_round_trip_through_their_stored_text() {
        for status in [
            ChangeStatus::Planned,
            ChangeStatus::Applying,
            ChangeStatus::Applied,
            ChangeStatus::Verified,
            ChangeStatus::Failed,
            ChangeStatus::AlreadyCompliant,
            ChangeStatus::RolledBack,
        ] {
            assert_eq!(ChangeStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(ChangeStatus::parse("nonsense"), None);
    }

    #[test]
    fn session_statuses_round_trip_through_their_stored_text() {
        for status in [
            SessionStatus::InProgress,
            SessionStatus::Completed,
            SessionStatus::CompletedWithFailures,
            SessionStatus::Stopped,
            SessionStatus::RolledBack,
        ] {
            assert_eq!(SessionStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(SessionStatus::parse("nonsense"), None);
    }

    #[test]
    fn rollback_statuses_round_trip_through_their_stored_text() {
        for status in [
            RollbackStatus::Restored,
            RollbackStatus::AlreadyOriginal,
            RollbackStatus::Failed,
            RollbackStatus::Skipped,
        ] {
            assert_eq!(RollbackStatus::parse(status.as_str()), Some(status));
        }
    }

    #[test]
    fn a_change_interrupted_mid_write_is_still_restorable() {
        // This is the property that makes a crash survivable: the rollback
        // data was persisted before the write, so `Applying` must be treated
        // as "may have altered the machine".
        assert!(ChangeStatus::Applying.is_restorable());
        assert!(ChangeStatus::Applied.is_restorable());
        assert!(ChangeStatus::Verified.is_restorable());

        // These did not alter the machine, or have already been undone.
        assert!(!ChangeStatus::Planned.is_restorable());
        assert!(!ChangeStatus::Failed.is_restorable());
        assert!(!ChangeStatus::AlreadyCompliant.is_restorable());
        assert!(!ChangeStatus::RolledBack.is_restorable());
    }

    #[test]
    fn incomplete_statuses_are_the_ones_status_must_warn_about() {
        assert!(ChangeStatus::Planned.is_incomplete());
        assert!(ChangeStatus::Applying.is_incomplete());
        assert!(!ChangeStatus::Verified.is_incomplete());
        assert!(!ChangeStatus::Failed.is_incomplete());
    }

    #[test]
    fn active_change_count_excludes_failures_and_rollbacks() {
        let counts = SessionCounts {
            total: 6,
            verified: 2,
            applied_unverified: 1,
            failed: 1,
            already_compliant: 1,
            rolled_back: 1,
            incomplete: 0,
        };
        assert_eq!(counts.active(), 3);
    }
}
