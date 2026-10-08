//! MinWin's structured error type.
//!
//! Two rules drive this design:
//!
//! 1. A Windows failure must never surface as "operation failed". Every call
//!    site wraps the raw OS error with the operation it was attempting, so the
//!    user sees `Failed to read service configuration for "SysMain": Access is
//!    denied. (os error 5)`.
//! 2. Conditions MinWin can explain in product terms (missing elevation, a
//!    held lock, an unknown change id) get their own variants so the CLI can
//!    render guidance instead of a stack of context strings.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, MinWinError>;

#[derive(Debug, thiserror::Error)]
pub enum MinWinError {
    /// A Windows API or OS-level call failed. `operation` describes what MinWin
    /// was doing in MinWin's own vocabulary.
    #[error("{operation}: {message} (Windows error {code})")]
    Windows {
        operation: String,
        message: String,
        code: u32,
    },

    #[error("{operation}: {source}")]
    Io {
        operation: String,
        #[source]
        source: std::io::Error,
    },

    #[error("state database error while {operation}: {source}")]
    Database {
        operation: String,
        #[source]
        source: rusqlite::Error,
    },

    #[error(
        "state database at {path} uses schema version {found}, but this build of MinWin supports {supported}"
    )]
    SchemaTooNew {
        path: PathBuf,
        found: i64,
        supported: i64,
    },

    #[error("profile {source_name} is not valid: {reason}")]
    InvalidProfile { source_name: String, reason: String },

    #[error("unknown profile {0:?}; built-in profiles are: minimal, gaming")]
    UnknownProfile(String),

    #[error("unknown change id {id:?} referenced by {source_name}")]
    UnknownChange { id: String, source_name: String },

    #[error("Windows service {0:?} is not installed on this system")]
    ServiceNotFound(String),

    #[error(
        "this operation requires Administrator privileges\n\n\
         Reopen your terminal as Administrator and run:\n    minwin {suggestion}"
    )]
    RequiresElevation { suggestion: String },

    #[error(
        "another MinWin operation is already running (lock held at {path})\n\n\
         Wait for it to finish, or close the other MinWin process."
    )]
    AlreadyRunning { path: PathBuf },

    #[error("MinWin could not determine a per-user application data directory: {reason}")]
    NoAppDataDirectory { reason: String },

    #[error("{0}")]
    Unsupported(String),

    #[error("no benchmark has been recorded yet; run `minwin benchmark` first")]
    NoBenchmark,

    #[error("MinWin has no recorded apply session, so there is nothing to {0}")]
    NothingRecorded(&'static str),

    #[error("could not serialise MinWin state: {0}")]
    Serialisation(#[from] serde_json::Error),
}

impl MinWinError {
    pub fn windows(operation: impl Into<String>, message: impl Into<String>, code: u32) -> Self {
        Self::Windows {
            operation: operation.into(),
            message: message.into(),
            code,
        }
    }

    pub fn io(operation: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            operation: operation.into(),
            source,
        }
    }

    pub fn db(operation: impl Into<String>, source: rusqlite::Error) -> Self {
        Self::Database {
            operation: operation.into(),
            source,
        }
    }

    pub fn invalid_profile(source_name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::InvalidProfile {
            source_name: source_name.into(),
            reason: reason.into(),
        }
    }

    /// True when the failure is a privilege problem the user can fix by
    /// relaunching an elevated terminal. The CLI uses this to pick an exit
    /// code and to avoid printing a generic "unexpected error" banner.
    pub fn is_privilege_problem(&self) -> bool {
        match self {
            Self::RequiresElevation { .. } => true,
            // ERROR_ACCESS_DENIED
            Self::Windows { code, .. } => *code == 5,
            _ => false,
        }
    }
}

/// Converts a `windows::core::Error` into a `MinWinError` with MinWin context.
///
/// Kept here (rather than as a `From` impl) so that every conversion is forced
/// to name the operation it was performing.
#[cfg(windows)]
pub fn from_win32(operation: impl Into<String>, error: windows::core::Error) -> MinWinError {
    MinWinError::Windows {
        operation: operation.into(),
        message: error.message().trim().to_string(),
        code: error.code().0 as u32 & 0xFFFF,
    }
}

/// Converts a raw `WIN32_ERROR` code (as returned by the registry and power
/// APIs, which do not use HRESULTs) into a `MinWinError`.
#[cfg(windows)]
pub fn from_win32_code(operation: impl Into<String>, code: u32) -> MinWinError {
    let error = windows::core::Error::from_hresult(
        windows::Win32::Foundation::WIN32_ERROR(code).to_hresult(),
    );
    MinWinError::Windows {
        operation: operation.into(),
        message: error.message().trim().to_string(),
        code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_errors_name_the_operation_and_code() {
        let error = MinWinError::windows(
            r#"read service configuration for "SysMain""#,
            "Access is denied.",
            5,
        );
        assert_eq!(
            error.to_string(),
            r#"read service configuration for "SysMain": Access is denied. (Windows error 5)"#
        );
        assert!(error.is_privilege_problem());
    }

    #[test]
    fn elevation_errors_tell_the_user_what_to_run() {
        let error = MinWinError::RequiresElevation {
            suggestion: "apply minimal".into(),
        };
        let rendered = error.to_string();
        assert!(rendered.contains("Administrator"));
        assert!(rendered.contains("minwin apply minimal"));
    }

    #[test]
    fn non_privilege_windows_errors_are_not_privilege_problems() {
        // ERROR_FILE_NOT_FOUND
        let error = MinWinError::windows("open registry key", "Not found.", 2);
        assert!(!error.is_privilege_problem());
    }
}
