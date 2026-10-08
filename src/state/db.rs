//! The state database.
//!
//! One important ordering guarantee is implemented here rather than described
//! in a comment elsewhere: [`Database::begin_apply_session`] writes the apply
//! session *and every change's pre-change state and rollback data* in a single
//! transaction, and returns only once that transaction has committed. No
//! caller can reach a mutation path without that row already on disk.
//!
//! Nothing about the user is stored. The data here is: what MinWin changed,
//! what it was before, when, on which Windows build, and the numeric
//! benchmark readings. No file paths from the user's profile, no credentials,
//! no process names, no environment.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::benchmark::model::{
    BenchmarkRun, BenchmarkSample, MetricId, RunEnvironment, SamplingPlan,
};
use crate::changes::model::{
    ObservedState, RebootRequirement, Risk, RollbackData, VerificationResult,
};
use crate::core::error::{MinWinError, Result};
use crate::state::migrations;
use crate::state::models::{
    ApplySessionRecord, ChangeRecord, ChangeStatus, RollbackStatus, SessionCounts, SessionStatus,
};

pub struct Database {
    connection: Connection,
    path: PathBuf,
}

/// What a caller must supply to open an apply session: the planned work,
/// already decided, so that persistence happens before mutation.
pub struct PendingChange {
    pub change_id: String,
    pub risk: Risk,
    pub reboot: RebootRequirement,
    pub state_before: ObservedState,
    pub state_planned: ObservedState,
    pub rollback: RollbackData,
    /// `true` when the machine already matched the target, so the row is
    /// recorded for the audit trail but nothing will be written.
    pub already_compliant: bool,
}

pub struct ApplySessionHeader {
    pub profile_id: String,
    pub profile_name: String,
    pub profile_source: String,
    pub windows_label: String,
    pub windows_build: u32,
    pub elevated: bool,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path).map_err(|e| {
            MinWinError::db(format!("open the state database at {}", path.display()), e)
        })?;
        Self::prepare(connection, path.to_path_buf())
    }

    /// An in-memory database, for tests.
    pub fn open_in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory()
            .map_err(|e| MinWinError::db("open an in-memory state database", e))?;
        Self::prepare(connection, PathBuf::from(":memory:"))
    }

    fn prepare(connection: Connection, path: PathBuf) -> Result<Self> {
        // Foreign keys are off by default in SQLite; MinWin relies on the
        // cascades declared in the schema.
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| MinWinError::db("enable foreign key enforcement", e))?;
        // Durability matters more than speed here: these rows are what makes a
        // change reversible.
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|e| MinWinError::db("set the database synchronous mode", e))?;

        migrations::migrate(&connection, &path)?;
        Ok(Self { connection, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schema_version(&self) -> Result<i64> {
        migrations::current_version(&self.connection)
    }

    // -- benchmarks ---------------------------------------------------------

    /// Stores a run and all its samples in one transaction, returning the
    /// assigned run id.
    pub fn insert_benchmark_run(&mut self, run: &BenchmarkRun, label: Option<&str>) -> Result<i64> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|e| MinWinError::db("begin a benchmark transaction", e))?;

        transaction
            .execute(
                "INSERT INTO benchmark_runs (
                     started_at, finished_at, minwin_version, windows_label, windows_build,
                     elevated, sample_count, interval_ms, settle_ms, total_physical_bytes,
                     uptime_seconds_at_start, label
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    run.started_at.to_rfc3339(),
                    run.finished_at.to_rfc3339(),
                    run.environment.minwin_version,
                    run.environment.windows_label,
                    run.environment.windows_build,
                    run.environment.elevated,
                    run.plan.samples,
                    run.plan.interval_ms as i64,
                    run.plan.settle_ms as i64,
                    run.environment.total_physical_bytes as i64,
                    run.environment.uptime_seconds_at_start as i64,
                    label,
                ],
            )
            .map_err(|e| MinWinError::db("record a benchmark run", e))?;
        let run_id = transaction.last_insert_rowid();

        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO benchmark_samples
                         (run_id, metric_id, sample_index, captured_at, value, error)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .map_err(|e| MinWinError::db("prepare the benchmark sample insert", e))?;
            for sample in &run.samples {
                statement
                    .execute(params![
                        run_id,
                        sample.metric.key(),
                        sample.sample_index,
                        sample.captured_at.to_rfc3339(),
                        sample.value,
                        sample.error,
                    ])
                    .map_err(|e| MinWinError::db("record a benchmark sample", e))?;
            }
        }

        transaction
            .commit()
            .map_err(|e| MinWinError::db("commit a benchmark run", e))?;
        Ok(run_id)
    }

    /// The most recent runs, newest first.
    pub fn recent_benchmark_runs(&self, limit: u32) -> Result<Vec<BenchmarkRun>> {
        let ids: Vec<i64> = self
            .connection
            .prepare("SELECT id FROM benchmark_runs ORDER BY started_at DESC, id DESC LIMIT ?1")
            .and_then(|mut statement| {
                statement
                    .query_map([limit], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<i64>>>()
            })
            .map_err(|e| MinWinError::db("list recent benchmark runs", e))?;

        ids.into_iter()
            .map(|id| self.benchmark_run(id))
            .collect::<Result<Vec<_>>>()
    }

    pub fn latest_benchmark_run(&self) -> Result<Option<BenchmarkRun>> {
        Ok(self.recent_benchmark_runs(1)?.into_iter().next())
    }

    pub fn benchmark_run_count(&self) -> Result<u32> {
        self.connection
            .query_row("SELECT COUNT(*) FROM benchmark_runs", [], |row| row.get(0))
            .map_err(|e| MinWinError::db("count benchmark runs", e))
    }

    pub fn benchmark_run(&self, run_id: i64) -> Result<BenchmarkRun> {
        let mut run = self
            .connection
            .query_row(
                "SELECT started_at, finished_at, minwin_version, windows_label, windows_build,
                        elevated, sample_count, interval_ms, settle_ms, total_physical_bytes,
                        uptime_seconds_at_start
                 FROM benchmark_runs WHERE id = ?1",
                [run_id],
                |row| {
                    Ok(BenchmarkRun {
                        id: Some(run_id),
                        started_at: parse_time(&row.get::<_, String>(0)?),
                        finished_at: parse_time(&row.get::<_, String>(1)?),
                        plan: SamplingPlan {
                            samples: row.get(6)?,
                            interval_ms: row.get::<_, i64>(7)? as u64,
                            settle_ms: row.get::<_, i64>(8)? as u64,
                        },
                        environment: RunEnvironment {
                            minwin_version: row.get(2)?,
                            windows_label: row.get(3)?,
                            windows_build: row.get(4)?,
                            elevated: row.get(5)?,
                            total_physical_bytes: row.get::<_, i64>(9)? as u64,
                            uptime_seconds_at_start: row.get::<_, i64>(10)? as u64,
                        },
                        samples: Vec::new(),
                    })
                },
            )
            .map_err(|e| MinWinError::db(format!("read benchmark run {run_id}"), e))?;

        run.samples = self
            .connection
            .prepare(
                "SELECT metric_id, sample_index, captured_at, value, error
                 FROM benchmark_samples WHERE run_id = ?1
                 ORDER BY sample_index, id",
            )
            .and_then(|mut statement| {
                statement
                    .query_map([run_id], |row| {
                        let key: String = row.get(0)?;
                        let sample_index: u32 = row.get(1)?;
                        // Parsed leniently: a timestamp MinWin cannot re-read
                        // must not make the whole run unreadable.
                        let captured_at = parse_time(&row.get::<_, String>(2).unwrap_or_default());
                        let value: Option<f64> = row.get(3)?;
                        let error: Option<String> = row.get(4)?;
                        Ok(MetricId::from_key(&key).map(|metric| BenchmarkSample {
                            metric,
                            sample_index,
                            captured_at,
                            value,
                            error,
                        }))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(|e| MinWinError::db(format!("read samples for benchmark run {run_id}"), e))?
            .into_iter()
            // A metric id this build does not know about is skipped rather
            // than guessed at, which is how a downgrade stays readable.
            .flatten()
            .collect();

        Ok(run)
    }

    // -- apply sessions -----------------------------------------------------

    /// Opens an apply session, persisting every change's pre-change state and
    /// rollback data **before** returning.
    ///
    /// This is the crash-safety boundary. Once this call has returned, MinWin
    /// can be killed at any later point and `rollback` still has everything it
    /// needs.
    pub fn begin_apply_session(
        &mut self,
        header: &ApplySessionHeader,
        pending: &[PendingChange],
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<i64> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|e| MinWinError::db("begin an apply session transaction", e))?;

        transaction
            .execute(
                "INSERT INTO apply_sessions (
                     started_at, finished_at, status, profile_id, profile_name, profile_source,
                     minwin_version, windows_label, windows_build, elevated
                 ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    started_at.to_rfc3339(),
                    SessionStatus::InProgress.as_str(),
                    header.profile_id,
                    header.profile_name,
                    header.profile_source,
                    crate::core::MINWIN_VERSION,
                    header.windows_label,
                    header.windows_build,
                    header.elevated,
                ],
            )
            .map_err(|e| MinWinError::db("record an apply session", e))?;
        let session_id = transaction.last_insert_rowid();

        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO change_records (
                         session_id, change_id, order_index, status, risk, reboot,
                         state_before_json, state_planned_json, rollback_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .map_err(|e| MinWinError::db("prepare the change record insert", e))?;
            for (index, change) in pending.iter().enumerate() {
                let status = if change.already_compliant {
                    ChangeStatus::AlreadyCompliant
                } else {
                    ChangeStatus::Planned
                };
                statement
                    .execute(params![
                        session_id,
                        change.change_id,
                        index as u32,
                        status.as_str(),
                        serde_json::to_string(&change.risk)?,
                        serde_json::to_string(&change.reboot)?,
                        serde_json::to_string(&change.state_before)?,
                        serde_json::to_string(&change.state_planned)?,
                        serde_json::to_string(&change.rollback)?,
                    ])
                    .map_err(|e| {
                        MinWinError::db(
                            format!("record the planned state of change {}", change.change_id),
                            e,
                        )
                    })?;
            }
        }

        transaction
            .commit()
            .map_err(|e| MinWinError::db("commit an apply session", e))?;
        Ok(session_id)
    }

    /// Marks a change as about to be written. Persisted immediately, so an
    /// interrupted write is distinguishable from one that never started.
    pub fn mark_change_applying(&self, session_id: i64, change_id: &str) -> Result<()> {
        self.set_change_status(session_id, change_id, ChangeStatus::Applying)
    }

    fn set_change_status(
        &self,
        session_id: i64,
        change_id: &str,
        status: ChangeStatus,
    ) -> Result<()> {
        self.connection
            .execute(
                "UPDATE change_records SET status = ?1 WHERE session_id = ?2 AND change_id = ?3",
                params![status.as_str(), session_id, change_id],
            )
            .map_err(|e| {
                MinWinError::db(
                    format!(
                        "update the status of change {change_id} to {}",
                        status.as_str()
                    ),
                    e,
                )
            })?;
        Ok(())
    }

    /// Records the outcome of applying and verifying one change.
    ///
    /// The parameter list is wide because every column is written in one
    /// statement: a partial update here would leave a row whose status and
    /// recorded state disagree, which is exactly the inconsistency rollback
    /// must never encounter.
    #[allow(clippy::too_many_arguments)]
    pub fn record_change_outcome(
        &self,
        session_id: i64,
        change_id: &str,
        status: ChangeStatus,
        state_after: Option<&ObservedState>,
        verification: Option<&VerificationResult>,
        error: Option<&str>,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        self.connection
            .execute(
                "UPDATE change_records
                 SET status = ?1, state_after_json = ?2, verification_json = ?3, error = ?4,
                     applied_at = ?5, verified_at = ?6
                 WHERE session_id = ?7 AND change_id = ?8",
                params![
                    status.as_str(),
                    state_after.map(serde_json::to_string).transpose()?,
                    verification.map(serde_json::to_string).transpose()?,
                    error,
                    at.to_rfc3339(),
                    verification.is_some().then(|| at.to_rfc3339()),
                    session_id,
                    change_id,
                ],
            )
            .map_err(|e| MinWinError::db(format!("record the outcome of change {change_id}"), e))?;
        Ok(())
    }

    pub fn finish_apply_session(
        &self,
        session_id: i64,
        status: SessionStatus,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        self.connection
            .execute(
                "UPDATE apply_sessions SET status = ?1, finished_at = ?2 WHERE id = ?3",
                params![status.as_str(), at.to_rfc3339(), session_id],
            )
            .map_err(|e| MinWinError::db(format!("finish apply session {session_id}"), e))?;
        Ok(())
    }

    pub fn apply_session(&self, session_id: i64) -> Result<ApplySessionRecord> {
        self.connection
            .query_row(
                "SELECT id, started_at, finished_at, status, profile_id, profile_name,
                        profile_source, minwin_version, windows_label, windows_build, elevated
                 FROM apply_sessions WHERE id = ?1",
                [session_id],
                parse_apply_session,
            )
            .map_err(|e| MinWinError::db(format!("read apply session {session_id}"), e))
    }

    /// The most recent apply session, if any.
    pub fn latest_apply_session(&self) -> Result<Option<ApplySessionRecord>> {
        self.connection
            .query_row(
                "SELECT id, started_at, finished_at, status, profile_id, profile_name,
                        profile_source, minwin_version, windows_label, windows_build, elevated
                 FROM apply_sessions ORDER BY started_at DESC, id DESC LIMIT 1",
                [],
                parse_apply_session,
            )
            .optional()
            .map_err(|e| MinWinError::db("read the most recent apply session", e))
    }

    /// The most recent apply session that still holds changes a rollback could
    /// restore. This is what `rollback` acts on, and it deliberately skips
    /// sessions that have already been fully rolled back.
    pub fn latest_restorable_apply_session(&self) -> Result<Option<ApplySessionRecord>> {
        let restorable: Vec<&str> = vec![
            ChangeStatus::Applying.as_str(),
            ChangeStatus::Applied.as_str(),
            ChangeStatus::Verified.as_str(),
        ];
        let placeholders = restorable.join("', '");
        let sql = format!(
            "SELECT s.id, s.started_at, s.finished_at, s.status, s.profile_id, s.profile_name,
                    s.profile_source, s.minwin_version, s.windows_label, s.windows_build, s.elevated
             FROM apply_sessions s
             WHERE EXISTS (
                 SELECT 1 FROM change_records c
                 WHERE c.session_id = s.id AND c.status IN ('{placeholders}')
             )
             ORDER BY s.started_at DESC, s.id DESC LIMIT 1"
        );
        self.connection
            .query_row(&sql, [], parse_apply_session)
            .optional()
            .map_err(|e| MinWinError::db("find the most recent restorable apply session", e))
    }

    pub fn change_records(&self, session_id: i64) -> Result<Vec<ChangeRecord>> {
        self.connection
            .prepare(
                "SELECT id, session_id, change_id, order_index, status, risk, reboot,
                        state_before_json, state_planned_json, rollback_json, state_after_json,
                        verification_json, error, applied_at, verified_at
                 FROM change_records WHERE session_id = ?1 ORDER BY order_index",
            )
            .and_then(|mut statement| {
                statement
                    .query_map([session_id], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, u32>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, String>(8)?,
                            row.get::<_, String>(9)?,
                            row.get::<_, Option<String>>(10)?,
                            row.get::<_, Option<String>>(11)?,
                            row.get::<_, Option<String>>(12)?,
                            row.get::<_, Option<String>>(13)?,
                            row.get::<_, Option<String>>(14)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(|e| {
                MinWinError::db(format!("read the changes of apply session {session_id}"), e)
            })?
            .into_iter()
            .map(|row| {
                Ok(ChangeRecord {
                    id: row.0,
                    session_id: row.1,
                    change_id: row.2,
                    order_index: row.3,
                    status: ChangeStatus::parse(&row.4).ok_or_else(|| {
                        MinWinError::Unsupported(format!(
                            "the state database holds an unrecognised change status {:?}",
                            row.4
                        ))
                    })?,
                    risk: serde_json::from_str(&row.5)?,
                    reboot: serde_json::from_str(&row.6)?,
                    state_before: serde_json::from_str(&row.7)?,
                    state_planned: serde_json::from_str(&row.8)?,
                    rollback: serde_json::from_str(&row.9)?,
                    state_after: row.10.as_deref().map(serde_json::from_str).transpose()?,
                    verification: row.11.as_deref().map(serde_json::from_str).transpose()?,
                    error: row.12,
                    applied_at: row.13.as_deref().map(parse_time),
                    verified_at: row.14.as_deref().map(parse_time),
                })
            })
            .collect()
    }

    pub fn session_counts(&self, session_id: i64) -> Result<SessionCounts> {
        let records = self.change_records(session_id)?;
        let mut counts = SessionCounts {
            total: records.len() as u32,
            ..Default::default()
        };
        for record in &records {
            match record.status {
                ChangeStatus::Verified => counts.verified += 1,
                ChangeStatus::Applied | ChangeStatus::Applying => {
                    counts.applied_unverified += 1;
                }
                ChangeStatus::Failed => counts.failed += 1,
                ChangeStatus::AlreadyCompliant => counts.already_compliant += 1,
                ChangeStatus::RolledBack => counts.rolled_back += 1,
                ChangeStatus::Planned => {}
            }
            if record.status.is_incomplete() {
                counts.incomplete += 1;
            }
        }
        Ok(counts)
    }

    /// Apply sessions that were never finished. `status` reports these so an
    /// interrupted run is visible rather than silent.
    pub fn incomplete_apply_sessions(&self) -> Result<Vec<ApplySessionRecord>> {
        self.connection
            .prepare(
                "SELECT id, started_at, finished_at, status, profile_id, profile_name,
                        profile_source, minwin_version, windows_label, windows_build, elevated
                 FROM apply_sessions WHERE status = ?1 ORDER BY started_at DESC",
            )
            .and_then(|mut statement| {
                statement
                    .query_map([SessionStatus::InProgress.as_str()], parse_apply_session)?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(|e| MinWinError::db("list incomplete apply sessions", e))
    }

    // -- rollback -----------------------------------------------------------

    pub fn begin_rollback_session(
        &self,
        apply_session_id: i64,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<i64> {
        self.connection
            .execute(
                "INSERT INTO rollback_sessions
                     (apply_session_id, started_at, finished_at, status, minwin_version)
                 VALUES (?1, ?2, NULL, ?3, ?4)",
                params![
                    apply_session_id,
                    started_at.to_rfc3339(),
                    SessionStatus::InProgress.as_str(),
                    crate::core::MINWIN_VERSION,
                ],
            )
            .map_err(|e| MinWinError::db("record a rollback session", e))?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Records one change's rollback outcome and, on success, flips the
    /// original change record to `RolledBack` in the same transaction — so the
    /// two can never disagree.
    #[allow(clippy::too_many_arguments)]
    pub fn record_rollback_outcome(
        &mut self,
        rollback_session_id: i64,
        change_record_id: i64,
        change_id: &str,
        status: RollbackStatus,
        state_before_rollback: &ObservedState,
        state_restored: Option<&ObservedState>,
        verification: Option<&VerificationResult>,
        error: Option<&str>,
        note: Option<&str>,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|e| MinWinError::db("begin a rollback outcome transaction", e))?;

        transaction
            .execute(
                "INSERT INTO rollback_records (
                     rollback_session_id, change_record_id, change_id, status,
                     state_before_rollback_json, state_restored_json, verification_json,
                     error, note, restored_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    rollback_session_id,
                    change_record_id,
                    change_id,
                    status.as_str(),
                    serde_json::to_string(state_before_rollback)?,
                    state_restored.map(serde_json::to_string).transpose()?,
                    verification.map(serde_json::to_string).transpose()?,
                    error,
                    note,
                    at.to_rfc3339(),
                ],
            )
            .map_err(|e| {
                MinWinError::db(format!("record the rollback of change {change_id}"), e)
            })?;

        if matches!(
            status,
            RollbackStatus::Restored | RollbackStatus::AlreadyOriginal
        ) {
            transaction
                .execute(
                    "UPDATE change_records SET status = ?1 WHERE id = ?2",
                    params![ChangeStatus::RolledBack.as_str(), change_record_id],
                )
                .map_err(|e| {
                    MinWinError::db(format!("mark change {change_id} as rolled back"), e)
                })?;
        }

        transaction
            .commit()
            .map_err(|e| MinWinError::db("commit a rollback outcome", e))?;
        Ok(())
    }

    pub fn finish_rollback_session(
        &self,
        rollback_session_id: i64,
        apply_session_id: i64,
        status: SessionStatus,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        self.connection
            .execute(
                "UPDATE rollback_sessions SET status = ?1, finished_at = ?2 WHERE id = ?3",
                params![status.as_str(), at.to_rfc3339(), rollback_session_id],
            )
            .map_err(|e| MinWinError::db("finish a rollback session", e))?;

        // If nothing restorable is left, the apply session itself is rolled
        // back.
        let remaining = self.session_counts(apply_session_id)?.active();
        if remaining == 0 {
            self.connection
                .execute(
                    "UPDATE apply_sessions SET status = ?1 WHERE id = ?2",
                    params![SessionStatus::RolledBack.as_str(), apply_session_id],
                )
                .map_err(|e| {
                    MinWinError::db(
                        format!("mark apply session {apply_session_id} as rolled back"),
                        e,
                    )
                })?;
        }
        Ok(())
    }

    pub fn rollback_session_count(&self) -> Result<u32> {
        self.connection
            .query_row("SELECT COUNT(*) FROM rollback_sessions", [], |row| {
                row.get(0)
            })
            .map_err(|e| MinWinError::db("count rollback sessions", e))
    }
}

fn parse_apply_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApplySessionRecord> {
    let status: String = row.get(3)?;
    Ok(ApplySessionRecord {
        id: row.get(0)?,
        started_at: parse_time(&row.get::<_, String>(1)?),
        finished_at: row.get::<_, Option<String>>(2)?.as_deref().map(parse_time),
        // An unrecognised status is treated as an unfinished session, which is
        // the conservative reading: it keeps rollback data in play.
        status: SessionStatus::parse(&status).unwrap_or(SessionStatus::InProgress),
        profile_id: row.get(4)?,
        profile_name: row.get(5)?,
        profile_source: row.get(6)?,
        minwin_version: row.get(7)?,
        windows_label: row.get(8)?,
        windows_build: row.get(9)?,
        elevated: row.get(10)?,
    })
}

/// Parses a stored RFC 3339 timestamp. Falls back to the Unix epoch rather
/// than failing, because an unreadable timestamp must not make a session's
/// rollback data inaccessible.
fn parse_time(text: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|value| value.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::DateTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::benchmark::model::MetricId;
    use serde_json::json;

    fn observed(text: &str) -> ObservedState {
        ObservedState::new(text, json!({"value": text}))
    }

    fn header() -> ApplySessionHeader {
        ApplySessionHeader {
            profile_id: "minimal".into(),
            profile_name: "Minimal".into(),
            profile_source: "built-in profile 'minimal'".into(),
            windows_label: "Windows 11 24H2".into(),
            windows_build: 26100,
            elevated: true,
        }
    }

    fn pending(change_id: &str) -> PendingChange {
        PendingChange {
            change_id: change_id.into(),
            risk: Risk::Low,
            reboot: RebootRequirement::NoReboot,
            state_before: observed("before"),
            state_planned: observed("after"),
            rollback: RollbackData::new("before", json!({"value": "before"})),
            already_compliant: false,
        }
    }

    fn database() -> Database {
        Database::open_in_memory().expect("database")
    }

    #[test]
    fn a_fresh_database_is_migrated_and_empty() {
        let db = database();
        assert_eq!(db.schema_version().expect("version"), 1);
        assert_eq!(db.benchmark_run_count().expect("count"), 0);
        assert!(db.latest_apply_session().expect("session").is_none());
        assert!(
            db.latest_restorable_apply_session()
                .expect("restorable")
                .is_none()
        );
    }

    #[test]
    fn a_benchmark_run_round_trips_with_every_sample_including_failures() {
        let mut db = database();
        let machine = crate::sys::fake::FakeMachine::windows_11();
        let facts = crate::sys::SystemFacts::gather(&machine).expect("facts");
        let run = crate::benchmark::collect_run(
            &machine,
            &facts,
            SamplingPlan {
                samples: 6,
                interval_ms: 0,
                settle_ms: 0,
            },
            &crate::core::clock::FixedClock::stepping(chrono::Utc::now(), 1),
            &crate::benchmark::InstantPacer,
        )
        .expect("run");

        let run_id = db
            .insert_benchmark_run(&run, Some("baseline"))
            .expect("insert");
        let loaded = db.benchmark_run(run_id).expect("load");

        assert_eq!(loaded.id, Some(run_id));
        assert_eq!(loaded.samples.len(), run.samples.len());
        assert_eq!(loaded.plan, run.plan);
        assert_eq!(loaded.environment, run.environment);
        assert_eq!(
            loaded.values_for(MetricId::ProcessCount),
            run.values_for(MetricId::ProcessCount)
        );
    }

    #[test]
    fn a_failed_sample_is_stored_as_null_with_its_error() {
        let mut db = database();
        let at = chrono::Utc::now();
        let run = BenchmarkRun {
            id: None,
            started_at: at,
            finished_at: at,
            plan: SamplingPlan::default(),
            environment: RunEnvironment {
                minwin_version: "0.1.0".into(),
                windows_label: "Windows 11 24H2".into(),
                windows_build: 26100,
                elevated: false,
                total_physical_bytes: 1,
                uptime_seconds_at_start: 1,
            },
            samples: vec![BenchmarkSample {
                metric: MetricId::CpuBusyPercent,
                sample_index: 0,
                captured_at: at,
                value: None,
                error: Some("access is denied".into()),
            }],
        };
        let id = db.insert_benchmark_run(&run, None).expect("insert");
        let loaded = db.benchmark_run(id).expect("load");
        assert_eq!(loaded.samples[0].value, None);
        assert_eq!(loaded.samples[0].error.as_deref(), Some("access is denied"));
    }

    #[test]
    fn runs_come_back_newest_first() {
        let mut db = database();
        let base = chrono::Utc::now();
        for offset in 0..3 {
            let at = base + chrono::Duration::seconds(offset);
            let run = BenchmarkRun {
                id: None,
                started_at: at,
                finished_at: at,
                plan: SamplingPlan::default(),
                environment: RunEnvironment {
                    minwin_version: "0.1.0".into(),
                    windows_label: "Windows 11 24H2".into(),
                    windows_build: 26100,
                    elevated: false,
                    total_physical_bytes: offset as u64,
                    uptime_seconds_at_start: 0,
                },
                samples: vec![],
            };
            db.insert_benchmark_run(&run, None).expect("insert");
        }
        let recent = db.recent_benchmark_runs(3).expect("recent");
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].environment.total_physical_bytes, 2);
        assert_eq!(recent[2].environment.total_physical_bytes, 0);
        assert_eq!(
            db.latest_benchmark_run()
                .expect("latest")
                .expect("some")
                .environment
                .total_physical_bytes,
            2
        );
    }

    #[test]
    fn opening_a_session_persists_rollback_data_before_anything_is_applied() {
        // The crash-safety property, asserted directly.
        let mut db = database();
        let session = db
            .begin_apply_session(&header(), &[pending("a"), pending("b")], chrono::Utc::now())
            .expect("begin");

        let records = db.change_records(session).expect("records");
        assert_eq!(records.len(), 2);
        for record in &records {
            assert_eq!(record.status, ChangeStatus::Planned);
            assert_eq!(record.rollback.summary, "before");
            assert_eq!(record.state_before, observed("before"));
            assert_eq!(record.state_planned, observed("after"));
            // Nothing has been applied yet.
            assert!(record.state_after.is_none());
            assert!(record.applied_at.is_none());
        }
        assert_eq!(records[0].order_index, 0);
        assert_eq!(records[1].order_index, 1);
    }

    #[test]
    fn an_already_compliant_change_is_recorded_without_being_planned() {
        let mut db = database();
        let mut compliant = pending("a");
        compliant.already_compliant = true;
        let session = db
            .begin_apply_session(&header(), &[compliant], chrono::Utc::now())
            .expect("begin");
        let records = db.change_records(session).expect("records");
        assert_eq!(records[0].status, ChangeStatus::AlreadyCompliant);
        assert!(!records[0].status.is_restorable());
    }

    #[test]
    fn a_session_interrupted_mid_write_is_detectable_afterwards() {
        let mut db = database();
        let session = db
            .begin_apply_session(&header(), &[pending("a")], chrono::Utc::now())
            .expect("begin");
        db.mark_change_applying(session, "a").expect("mark");
        // MinWin "crashes" here: no finish call.

        let incomplete = db.incomplete_apply_sessions().expect("incomplete");
        assert_eq!(incomplete.len(), 1);
        assert_eq!(incomplete[0].id, session);

        let counts = db.session_counts(session).expect("counts");
        assert_eq!(counts.incomplete, 1);
        // And the change is still treated as needing rollback.
        assert_eq!(counts.active(), 1);
        assert!(
            db.latest_restorable_apply_session()
                .expect("restorable")
                .is_some()
        );
    }

    #[test]
    fn outcomes_and_verification_round_trip() {
        let mut db = database();
        let at = chrono::Utc::now();
        let session = db
            .begin_apply_session(&header(), &[pending("a")], at)
            .expect("begin");

        let verification = VerificationResult::Verified {
            observed: observed("after"),
        };
        db.record_change_outcome(
            session,
            "a",
            ChangeStatus::Verified,
            Some(&observed("after")),
            Some(&verification),
            None,
            at,
        )
        .expect("record");
        db.finish_apply_session(session, SessionStatus::Completed, at)
            .expect("finish");

        let records = db.change_records(session).expect("records");
        assert_eq!(records[0].status, ChangeStatus::Verified);
        assert_eq!(records[0].state_after, Some(observed("after")));
        assert_eq!(records[0].verification, Some(verification));
        assert!(records[0].verified_at.is_some());
        assert_eq!(
            db.apply_session(session).expect("session").status,
            SessionStatus::Completed
        );
        assert!(db.incomplete_apply_sessions().expect("none").is_empty());
    }

    #[test]
    fn a_failed_change_keeps_its_error_and_stays_out_of_rollback() {
        let mut db = database();
        let at = chrono::Utc::now();
        let session = db
            .begin_apply_session(&header(), &[pending("a")], at)
            .expect("begin");
        db.record_change_outcome(
            session,
            "a",
            ChangeStatus::Failed,
            None,
            None,
            Some("Access is denied. (Windows error 5)"),
            at,
        )
        .expect("record");

        let records = db.change_records(session).expect("records");
        assert_eq!(records[0].status, ChangeStatus::Failed);
        assert!(records[0].error.as_deref().unwrap().contains("denied"));
        assert!(!records[0].status.is_restorable());
        assert_eq!(db.session_counts(session).expect("counts").failed, 1);
        assert!(
            db.latest_restorable_apply_session()
                .expect("restorable")
                .is_none(),
            "a session whose only change failed has nothing to restore"
        );
    }

    #[test]
    fn rolling_back_every_change_marks_the_apply_session_rolled_back() {
        let mut db = database();
        let at = chrono::Utc::now();
        let session = db
            .begin_apply_session(&header(), &[pending("a"), pending("b")], at)
            .expect("begin");
        for id in ["a", "b"] {
            db.record_change_outcome(
                session,
                id,
                ChangeStatus::Verified,
                Some(&observed("after")),
                None,
                None,
                at,
            )
            .expect("record");
        }
        db.finish_apply_session(session, SessionStatus::Completed, at)
            .expect("finish");

        let rollback = db.begin_rollback_session(session, at).expect("begin");
        let records = db.change_records(session).expect("records");
        for record in &records {
            db.record_rollback_outcome(
                rollback,
                record.id,
                &record.change_id,
                RollbackStatus::Restored,
                &observed("after"),
                Some(&observed("before")),
                None,
                None,
                None,
                at,
            )
            .expect("rollback record");
        }
        db.finish_rollback_session(rollback, session, SessionStatus::Completed, at)
            .expect("finish");

        assert_eq!(
            db.apply_session(session).expect("session").status,
            SessionStatus::RolledBack
        );
        assert_eq!(db.session_counts(session).expect("counts").active(), 0);
        assert!(
            db.latest_restorable_apply_session()
                .expect("restorable")
                .is_none()
        );
        assert_eq!(db.rollback_session_count().expect("count"), 1);
    }

    #[test]
    fn a_partially_rolled_back_session_remains_restorable() {
        let mut db = database();
        let at = chrono::Utc::now();
        let session = db
            .begin_apply_session(&header(), &[pending("a"), pending("b")], at)
            .expect("begin");
        for id in ["a", "b"] {
            db.record_change_outcome(
                session,
                id,
                ChangeStatus::Verified,
                Some(&observed("after")),
                None,
                None,
                at,
            )
            .expect("record");
        }
        db.finish_apply_session(session, SessionStatus::Completed, at)
            .expect("finish");

        let rollback = db.begin_rollback_session(session, at).expect("begin");
        let first = db.change_records(session).expect("records")[0].clone();
        db.record_rollback_outcome(
            rollback,
            first.id,
            &first.change_id,
            RollbackStatus::Restored,
            &observed("after"),
            Some(&observed("before")),
            None,
            None,
            None,
            at,
        )
        .expect("rollback record");
        db.finish_rollback_session(rollback, session, SessionStatus::CompletedWithFailures, at)
            .expect("finish");

        assert_eq!(db.session_counts(session).expect("counts").active(), 1);
        assert_eq!(
            db.apply_session(session).expect("session").status,
            SessionStatus::Completed,
            "a session with work left must not be marked rolled back"
        );
        assert!(
            db.latest_restorable_apply_session()
                .expect("restorable")
                .is_some()
        );
    }

    #[test]
    fn a_failed_rollback_does_not_mark_the_change_as_restored() {
        let mut db = database();
        let at = chrono::Utc::now();
        let session = db
            .begin_apply_session(&header(), &[pending("a")], at)
            .expect("begin");
        db.record_change_outcome(
            session,
            "a",
            ChangeStatus::Verified,
            Some(&observed("after")),
            None,
            None,
            at,
        )
        .expect("record");

        let rollback = db.begin_rollback_session(session, at).expect("begin");
        let record = db.change_records(session).expect("records")[0].clone();
        db.record_rollback_outcome(
            rollback,
            record.id,
            "a",
            RollbackStatus::Failed,
            &observed("after"),
            None,
            None,
            Some("Access is denied."),
            None,
            at,
        )
        .expect("rollback record");

        let after = db.change_records(session).expect("records");
        assert_eq!(
            after[0].status,
            ChangeStatus::Verified,
            "a failed rollback must leave the change recorded as still applied"
        );
        assert!(
            db.latest_restorable_apply_session()
                .expect("restorable")
                .is_some()
        );
    }

    #[test]
    fn a_skipped_rollback_leaves_the_change_in_place_too() {
        let mut db = database();
        let at = chrono::Utc::now();
        let session = db
            .begin_apply_session(&header(), &[pending("a")], at)
            .expect("begin");
        db.record_change_outcome(
            session,
            "a",
            ChangeStatus::Verified,
            Some(&observed("after")),
            None,
            None,
            at,
        )
        .expect("record");
        let rollback = db.begin_rollback_session(session, at).expect("begin");
        let record = db.change_records(session).expect("records")[0].clone();
        db.record_rollback_outcome(
            rollback,
            record.id,
            "a",
            RollbackStatus::Skipped,
            &observed("changed externally"),
            None,
            None,
            None,
            Some("changed outside MinWin"),
            at,
        )
        .expect("rollback record");

        assert_eq!(
            db.change_records(session).expect("records")[0].status,
            ChangeStatus::Verified
        );
    }

    #[test]
    fn the_most_recent_session_is_the_one_rollback_would_act_on() {
        let mut db = database();
        let base = chrono::Utc::now();
        let mut sessions = Vec::new();
        for offset in 0..2 {
            let at = base + chrono::Duration::seconds(offset);
            let session = db
                .begin_apply_session(&header(), &[pending("a")], at)
                .expect("begin");
            db.record_change_outcome(
                session,
                "a",
                ChangeStatus::Verified,
                Some(&observed("after")),
                None,
                None,
                at,
            )
            .expect("record");
            sessions.push(session);
        }
        assert_eq!(
            db.latest_restorable_apply_session()
                .expect("restorable")
                .expect("some")
                .id,
            sessions[1]
        );
    }

    #[test]
    fn a_database_survives_being_closed_and_reopened() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("state.db");
        let at = chrono::Utc::now();
        let session = {
            let mut db = Database::open(&path).expect("open");
            db.begin_apply_session(&header(), &[pending("a")], at)
                .expect("begin")
        };

        let db = Database::open(&path).expect("reopen");
        let records = db.change_records(session).expect("records");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].rollback.summary, "before");
        assert_eq!(db.path(), path);
    }
}
