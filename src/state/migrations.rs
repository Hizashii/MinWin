//! Schema migrations.
//!
//! Each migration is an append-only entry in [`MIGRATIONS`]. Applying them is
//! idempotent: the recorded version is read first, only later migrations run,
//! and each one runs inside a transaction so a failure leaves the database at
//! its previous version rather than half-upgraded.
//!
//! A database written by a *newer* MinWin is refused rather than opened, so an
//! older binary cannot misread columns it does not know about.

use rusqlite::Connection;

use crate::core::error::{MinWinError, Result};

pub struct Migration {
    pub version: i64,
    pub description: &'static str,
    pub sql: &'static str,
}

/// The schema.
///
/// Design notes worth stating once:
///
/// * Benchmark samples are stored one row per metric per sample, not as a JSON
///   blob per sample. Raw data stays queryable, which is what lets a future,
///   more rigorous statistical treatment be applied to runs recorded today.
/// * Change state is JSON, because its shape genuinely varies by change kind
///   (a service start type, an optional registry DWORD, a power plan GUID).
///   Everything *around* it — ids, statuses, timestamps, ordering — is in
///   real columns.
/// * `change_records` stores the pre-change state, the planned state and the
///   rollback data in separate columns, written before any mutation. That is
///   the row an interrupted apply leaves behind, and it is sufficient on its
///   own to roll the change back.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    description: "initial schema",
    sql: r#"
CREATE TABLE schema_version (
    version     INTEGER NOT NULL,
    applied_at  TEXT    NOT NULL,
    description TEXT    NOT NULL
);

-- One benchmark session.
CREATE TABLE benchmark_runs (
    id                       INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at               TEXT    NOT NULL,
    finished_at              TEXT    NOT NULL,
    minwin_version           TEXT    NOT NULL,
    windows_label            TEXT    NOT NULL,
    windows_build            INTEGER NOT NULL,
    elevated                 INTEGER NOT NULL,
    sample_count             INTEGER NOT NULL,
    interval_ms              INTEGER NOT NULL,
    settle_ms                INTEGER NOT NULL,
    total_physical_bytes     INTEGER NOT NULL,
    uptime_seconds_at_start  INTEGER NOT NULL,
    -- Free-text label such as 'baseline' or 'after apply minimal'.
    label                    TEXT
);

-- Raw readings. A failed read is stored with a NULL value and the error text,
-- never substituted with a number.
CREATE TABLE benchmark_samples (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id       INTEGER NOT NULL REFERENCES benchmark_runs(id) ON DELETE CASCADE,
    metric_id    TEXT    NOT NULL,
    sample_index INTEGER NOT NULL,
    captured_at  TEXT    NOT NULL,
    value        REAL,
    error        TEXT
);

CREATE INDEX idx_samples_run_metric ON benchmark_samples(run_id, metric_id);

-- One `minwin apply` invocation. Dry runs are not recorded: they change
-- nothing, so they must not appear as history that diff or rollback could act
-- on.
CREATE TABLE apply_sessions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at      TEXT    NOT NULL,
    finished_at     TEXT,
    status          TEXT    NOT NULL,
    profile_id      TEXT    NOT NULL,
    profile_name    TEXT    NOT NULL,
    profile_source  TEXT    NOT NULL,
    minwin_version  TEXT    NOT NULL,
    windows_label   TEXT    NOT NULL,
    windows_build   INTEGER NOT NULL,
    elevated        INTEGER NOT NULL
);

-- One change within one apply session.
CREATE TABLE change_records (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id          INTEGER NOT NULL REFERENCES apply_sessions(id) ON DELETE CASCADE,
    change_id           TEXT    NOT NULL,
    order_index         INTEGER NOT NULL,
    status              TEXT    NOT NULL,
    risk                TEXT    NOT NULL,
    reboot              TEXT    NOT NULL,
    -- Written before the change is applied.
    state_before_json   TEXT    NOT NULL,
    state_planned_json  TEXT    NOT NULL,
    rollback_json       TEXT    NOT NULL,
    -- Written after.
    state_after_json    TEXT,
    verification_json   TEXT,
    error               TEXT,
    applied_at          TEXT,
    verified_at         TEXT
);

CREATE INDEX idx_change_records_session ON change_records(session_id, order_index);

-- One `minwin rollback` invocation.
CREATE TABLE rollback_sessions (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    apply_session_id  INTEGER NOT NULL REFERENCES apply_sessions(id) ON DELETE CASCADE,
    started_at        TEXT    NOT NULL,
    finished_at       TEXT,
    status            TEXT    NOT NULL,
    minwin_version    TEXT    NOT NULL
);

-- One change within one rollback session.
CREATE TABLE rollback_records (
    id                        INTEGER PRIMARY KEY AUTOINCREMENT,
    rollback_session_id       INTEGER NOT NULL REFERENCES rollback_sessions(id) ON DELETE CASCADE,
    change_record_id          INTEGER NOT NULL REFERENCES change_records(id) ON DELETE CASCADE,
    change_id                 TEXT    NOT NULL,
    status                    TEXT    NOT NULL,
    -- What the machine read immediately before the restore, which is how an
    -- external modification is recorded rather than just warned about.
    state_before_rollback_json TEXT   NOT NULL,
    state_restored_json       TEXT,
    verification_json         TEXT,
    error                     TEXT,
    note                      TEXT,
    restored_at               TEXT
);

CREATE INDEX idx_rollback_records_session ON rollback_records(rollback_session_id);
"#,
}];

/// The highest schema version this build understands.
pub fn supported_version() -> i64 {
    MIGRATIONS
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0)
}

/// Reads the current schema version. A database with no `schema_version` table
/// is a fresh one, at version 0.
pub fn current_version(connection: &Connection) -> Result<i64> {
    let has_table: bool = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|_: i64| true)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(false),
            other => Err(MinWinError::db("check for the schema_version table", other)),
        })?;

    if !has_table {
        return Ok(0);
    }

    connection
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(|e| MinWinError::db("read the current schema version", e))
}

/// Brings the database up to [`supported_version`].
pub fn migrate(connection: &Connection, path: &std::path::Path) -> Result<i64> {
    let current = current_version(connection)?;
    let supported = supported_version();

    if current > supported {
        return Err(MinWinError::SchemaTooNew {
            path: path.to_path_buf(),
            found: current,
            supported,
        });
    }

    for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
        tracing::info!(
            version = migration.version,
            description = migration.description,
            "applying a schema migration"
        );
        connection
            .execute_batch(&format!("BEGIN; {} COMMIT;", migration.sql))
            .map_err(|e| {
                MinWinError::db(
                    format!(
                        "apply schema migration {} ({})",
                        migration.version, migration.description
                    ),
                    e,
                )
            })?;
        connection
            .execute(
                "INSERT INTO schema_version (version, applied_at, description) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    migration.version,
                    chrono::Utc::now().to_rfc3339(),
                    migration.description
                ],
            )
            .map_err(|e| {
                MinWinError::db(format!("record schema migration {}", migration.version), e)
            })?;
    }

    Ok(supported)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_connection() -> Connection {
        Connection::open_in_memory().expect("in-memory database")
    }

    #[test]
    fn migration_versions_are_unique_and_ascending() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        let mut sorted = versions.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            versions, sorted,
            "migrations must be append-only, unique and in ascending order"
        );
        assert_eq!(versions.first(), Some(&1));
    }

    #[test]
    fn a_fresh_database_reports_version_zero_then_migrates() {
        let connection = memory_connection();
        assert_eq!(current_version(&connection).expect("version"), 0);

        let version = migrate(&connection, std::path::Path::new(":memory:")).expect("migrate");
        assert_eq!(version, supported_version());
        assert_eq!(current_version(&connection).expect("version"), version);
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let connection = memory_connection();
        let path = std::path::Path::new(":memory:");
        migrate(&connection, path).expect("first");
        migrate(&connection, path).expect("second");

        let applied: i64 = connection
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .expect("count");
        assert_eq!(applied, MIGRATIONS.len() as i64);
    }

    #[test]
    fn a_database_from_a_newer_minwin_is_refused_not_opened() {
        let connection = memory_connection();
        let path = std::path::Path::new("state.db");
        migrate(&connection, path).expect("migrate");

        connection
            .execute(
                "INSERT INTO schema_version (version, applied_at, description) VALUES (999, '', 'from the future')",
                [],
            )
            .expect("insert");

        let error = migrate(&connection, path).expect_err("should refuse");
        assert!(matches!(error, MinWinError::SchemaTooNew { .. }));
        assert!(error.to_string().contains("999"));
    }

    #[test]
    fn every_expected_table_exists_after_migration() {
        let connection = memory_connection();
        migrate(&connection, std::path::Path::new(":memory:")).expect("migrate");

        for table in [
            "schema_version",
            "benchmark_runs",
            "benchmark_samples",
            "apply_sessions",
            "change_records",
            "rollback_sessions",
            "rollback_records",
        ] {
            let found: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("query");
            assert_eq!(found, 1, "table {table} is missing");
        }
    }
}
