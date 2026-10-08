//! What MinWin and the machine currently look like.
//!
//! Every field here is read from the real machine or the state database.
//! Nothing is defaulted to a plausible-looking value: where MinWin has no
//! answer, the field is `None` and the renderer says so.

use serde::{Deserialize, Serialize};

use crate::benchmark::model::MetricId;
use crate::core::error::Result;
use crate::state::Database;
use crate::state::models::{SessionCounts, SessionStatus};
use crate::sys::SystemFacts;
use crate::sys::model::MemorySnapshot;

/// A previous apply session that never finished.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IncompleteSession {
    pub session_id: i64,
    pub profile_id: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub counts: SessionCounts,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveProfile {
    pub session_id: i64,
    pub profile_id: String,
    pub profile_name: String,
    pub applied_at: chrono::DateTime<chrono::Utc>,
    pub status: SessionStatus,
    pub counts: SessionCounts,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineSummary {
    pub run_id: i64,
    pub recorded_at: chrono::DateTime<chrono::Utc>,
    pub sample_count: u32,
    /// Median available memory at the time, for a one-line sanity check.
    pub median_available_bytes: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusReport {
    pub minwin_version: String,
    pub windows_label: String,
    pub windows_build: u32,
    pub windows_revision: Option<u32>,
    pub elevated: bool,
    pub centrally_managed: bool,
    pub management_description: String,
    pub uptime_seconds: u64,
    pub memory: MemorySnapshot,
    pub database_path: String,
    pub schema_version: i64,
    pub benchmark_run_count: u32,
    /// The most recent benchmark, which is what a comparison treats as the
    /// baseline.
    pub baseline: Option<BaselineSummary>,
    pub active_profile: Option<ActiveProfile>,
    /// Changes currently holding the machine away from its original state.
    pub active_change_count: u32,
    pub rollback_available: bool,
    pub rollback_session_count: u32,
    /// Non-empty when a previous run was interrupted. The CLI turns this into
    /// a warning, because it means recorded state and the machine may disagree.
    pub incomplete_sessions: Vec<IncompleteSession>,
    pub registered_change_count: usize,
}

impl StatusReport {
    pub fn has_baseline(&self) -> bool {
        self.baseline.is_some()
    }

    pub fn needs_attention(&self) -> bool {
        !self.incomplete_sessions.is_empty()
    }
}

/// Reads MinWin's own state and the machine's. Read-only throughout.
pub fn status(
    facts: &SystemFacts,
    machine: &dyn crate::sys::traits::Machine,
    database: &Database,
    registered_change_count: usize,
) -> Result<StatusReport> {
    let latest_run = database.latest_benchmark_run()?;
    let baseline = latest_run.as_ref().and_then(|run| {
        let summary = run.summarise();
        Some(BaselineSummary {
            run_id: run.id?,
            recorded_at: run.started_at,
            sample_count: run.plan.samples,
            median_available_bytes: summary
                .get(MetricId::MemoryAvailableBytes)
                .map(|metric| metric.median),
        })
    });

    let latest_session = database.latest_apply_session()?;
    let active_profile = match &latest_session {
        Some(session) => {
            let counts = database.session_counts(session.id)?;
            Some(ActiveProfile {
                session_id: session.id,
                profile_id: session.profile_id.clone(),
                profile_name: session.profile_name.clone(),
                applied_at: session.started_at,
                status: session.status,
                counts,
            })
        }
        None => None,
    };

    let restorable = database.latest_restorable_apply_session()?;
    let active_change_count = match &restorable {
        Some(session) => database.session_counts(session.id)?.active(),
        None => 0,
    };

    let incomplete_sessions = database
        .incomplete_apply_sessions()?
        .into_iter()
        .map(|session| {
            Ok(IncompleteSession {
                counts: database.session_counts(session.id)?,
                session_id: session.id,
                profile_id: session.profile_id,
                started_at: session.started_at,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(StatusReport {
        minwin_version: crate::core::MINWIN_VERSION.to_string(),
        windows_label: facts.windows.label(),
        windows_build: facts.windows.build,
        windows_revision: facts.windows.revision,
        elevated: facts.elevated,
        centrally_managed: facts.management.is_centrally_managed(),
        management_description: facts.management.describe(),
        uptime_seconds: machine.info().uptime_seconds()?,
        memory: machine.info().memory()?,
        database_path: database.path().display().to_string(),
        schema_version: database.schema_version()?,
        benchmark_run_count: database.benchmark_run_count()?,
        baseline,
        active_profile,
        active_change_count,
        rollback_available: restorable.is_some(),
        rollback_session_count: database.rollback_session_count()?,
        incomplete_sessions,
        registered_change_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::benchmark::SamplingPlan;
    use crate::changes::ChangeRegistry;
    use crate::core::clock::FixedClock;
    use crate::engine::{apply, rollback};
    use crate::profiles::load_builtin;
    use crate::sys::fake::FakeMachine;

    fn report(machine: &FakeMachine, database: &Database) -> StatusReport {
        let facts = SystemFacts::gather(machine).expect("facts");
        status(&facts, machine, database, 4).expect("status")
    }

    #[test]
    fn a_fresh_install_reports_no_baseline_and_no_profile() {
        let machine = FakeMachine::windows_11();
        let database = Database::open_in_memory().expect("database");
        let report = report(&machine, &database);

        assert_eq!(report.minwin_version, crate::core::MINWIN_VERSION);
        assert_eq!(report.windows_label, "Windows 11 24H2");
        assert_eq!(report.windows_build, 26100);
        assert!(!report.elevated);
        assert!(!report.has_baseline());
        assert!(report.active_profile.is_none());
        assert_eq!(report.active_change_count, 0);
        assert!(!report.rollback_available);
        assert!(!report.needs_attention());
        assert_eq!(report.registered_change_count, 4);
        assert_eq!(report.schema_version, 1);
    }

    #[test]
    fn real_machine_facts_are_reported_not_invented() {
        let machine = FakeMachine::windows_11().elevated();
        let database = Database::open_in_memory().expect("database");
        let report = report(&machine, &database);

        assert!(report.elevated);
        assert_eq!(report.uptime_seconds, 7 * 3600);
        assert_eq!(report.memory.total_physical_bytes, 32 * 1024 * 1024 * 1024);
        assert_eq!(report.memory.load_percent, 34);
        assert!(!report.centrally_managed);
        assert_eq!(report.management_description, "not centrally managed");
    }

    #[test]
    fn a_recorded_benchmark_becomes_the_baseline() {
        let machine = FakeMachine::windows_11();
        let mut database = Database::open_in_memory().expect("database");
        let facts = SystemFacts::gather(&machine).expect("facts");
        let run = crate::benchmark::collect_run(
            &machine,
            &facts,
            SamplingPlan {
                samples: 8,
                interval_ms: 0,
                settle_ms: 0,
            },
            &FixedClock::stepping(chrono::Utc::now(), 1),
            &crate::benchmark::InstantPacer,
        )
        .expect("run");
        database
            .insert_benchmark_run(&run, Some("baseline"))
            .expect("insert");

        let report = report(&machine, &database);
        assert!(report.has_baseline());
        assert_eq!(report.benchmark_run_count, 1);
        let baseline = report.baseline.expect("baseline");
        assert_eq!(baseline.sample_count, 8);
        assert_eq!(
            baseline.median_available_bytes,
            Some(21.0 * 1024.0 * 1024.0 * 1024.0)
        );
    }

    #[test]
    fn after_apply_the_profile_change_count_and_rollback_availability_are_reported() {
        let machine = FakeMachine::windows_11().elevated();
        let registry = ChangeRegistry::load();
        let profile = load_builtin("minimal", &registry).expect("profile");
        let facts = SystemFacts::gather(&machine).expect("facts");
        let plan = apply::plan(&machine, &facts, &registry, &profile).expect("plan");
        let mut database = Database::open_in_memory().expect("database");
        apply::execute(
            &machine,
            &facts,
            &registry,
            &mut database,
            &plan,
            &FixedClock::stepping(chrono::Utc::now(), 1),
        )
        .expect("execute");

        let report = report(&machine, &database);
        let active = report.active_profile.expect("active profile");
        assert_eq!(active.profile_id, "minimal");
        assert_eq!(active.status, SessionStatus::Completed);
        assert_eq!(active.counts.verified, 2);
        assert_eq!(report.active_change_count, 2);
        assert!(report.rollback_available);
        assert_eq!(report.rollback_session_count, 0);
    }

    #[test]
    fn after_rollback_nothing_is_left_to_restore() {
        let machine = FakeMachine::windows_11().elevated();
        let registry = ChangeRegistry::load();
        let profile = load_builtin("minimal", &registry).expect("profile");
        let facts = SystemFacts::gather(&machine).expect("facts");
        let plan = apply::plan(&machine, &facts, &registry, &profile).expect("plan");
        let mut database = Database::open_in_memory().expect("database");
        let clock = FixedClock::stepping(chrono::Utc::now(), 1);
        apply::execute(&machine, &facts, &registry, &mut database, &plan, &clock).expect("execute");

        let rollback_plan = rollback::plan_latest(&machine, &facts, &registry, &database)
            .expect("plan")
            .expect("some");
        rollback::execute(
            &machine,
            &facts,
            &registry,
            &mut database,
            &rollback_plan,
            rollback::RollbackAuthorisation::default(),
            &clock,
        )
        .expect("rollback");

        let report = report(&machine, &database);
        assert_eq!(report.active_change_count, 0);
        assert!(!report.rollback_available);
        assert_eq!(report.rollback_session_count, 1);
        assert_eq!(
            report.active_profile.expect("profile").status,
            SessionStatus::RolledBack
        );
    }

    #[test]
    fn an_interrupted_session_is_surfaced_as_needing_attention() {
        // The crash case that `status` exists to make visible.
        let machine = FakeMachine::windows_11().elevated();
        let mut database = Database::open_in_memory().expect("database");
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
                &[crate::state::PendingChange {
                    change_id: crate::changes::DIAGTRACK_START_TYPE.into(),
                    risk: crate::changes::Risk::Low,
                    reboot: crate::changes::RebootRequirement::RebootRecommended,
                    state_before: crate::changes::ObservedState::new(
                        "Automatic",
                        serde_json::json!({"service": "DiagTrack", "start_type": "automatic"}),
                    ),
                    state_planned: crate::changes::ObservedState::new(
                        "Manual",
                        serde_json::json!({"service": "DiagTrack", "start_type": "manual"}),
                    ),
                    rollback: crate::changes::RollbackData::new(
                        "Automatic",
                        serde_json::json!({"service": "DiagTrack", "start_type": "automatic"}),
                    ),
                    already_compliant: false,
                }],
                chrono::Utc::now(),
            )
            .expect("begin");
        database
            .mark_change_applying(session, crate::changes::DIAGTRACK_START_TYPE)
            .expect("mark");

        let report = report(&machine, &database);
        assert!(report.needs_attention());
        assert_eq!(report.incomplete_sessions.len(), 1);
        assert_eq!(report.incomplete_sessions[0].session_id, session);
        assert_eq!(report.incomplete_sessions[0].counts.incomplete, 1);
        // And its rollback data is still available.
        assert!(report.rollback_available);
        assert_eq!(report.active_change_count, 1);
    }

    #[test]
    fn status_never_writes_to_the_machine() {
        let machine = FakeMachine::windows_11().elevated();
        let database = Database::open_in_memory().expect("database");
        report(&machine, &database);
        assert!(machine.writes().is_empty());
    }

    #[test]
    fn a_managed_device_is_reported_as_managed() {
        let machine =
            FakeMachine::windows_11()
                .elevated()
                .managed(crate::sys::model::ManagementState {
                    domain_joined: true,
                    defender_for_endpoint_onboarded: false,
                });
        let database = Database::open_in_memory().expect("database");
        let report = report(&machine, &database);
        assert!(report.centrally_managed);
        assert!(report.management_description.contains("Active Directory"));
    }
}
