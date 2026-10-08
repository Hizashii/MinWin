//! Where MinWin keeps its own data.
//!
//! MinWin stores state under `%LOCALAPPDATA%\MinWin\`. Nothing is written into
//! the source tree, and nothing is written outside that directory.
//!
//! Known caveat, documented rather than hidden: `%LOCALAPPDATA%` is per-user.
//! If you benchmark as one user and then apply as a *different* administrator
//! account, the second account gets its own state database. Running an elevated
//! terminal under the same user account (the normal case) keeps one database.

use std::path::{Path, PathBuf};

use crate::core::error::{MinWinError, Result};

#[derive(Debug, Clone)]
pub struct AppPaths {
    root: PathBuf,
}

impl AppPaths {
    /// Resolves the real per-user data directory and creates it if needed.
    pub fn discover() -> Result<Self> {
        let base = local_app_data()?;
        Self::rooted_at(base.join("MinWin"))
    }

    /// Roots MinWin's data at an explicit directory. Used by tests and by the
    /// `MINWIN_DATA_DIR` override so a test run can never touch real state.
    pub fn rooted_at(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)
            .map_err(|e| MinWinError::io(format!("create data directory {}", root.display()), e))?;
        let logs = root.join("logs");
        std::fs::create_dir_all(&logs)
            .map_err(|e| MinWinError::io(format!("create log directory {}", logs.display()), e))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn database(&self) -> PathBuf {
        self.root.join("state.db")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.root.join("minwin.lock")
    }

    pub fn log_directory(&self) -> PathBuf {
        self.root.join("logs")
    }
}

/// `MINWIN_DATA_DIR` exists so the integration tests (and anyone debugging) can
/// point MinWin at a scratch directory. It only moves MinWin's *own* files; it
/// never affects which machine state MinWin inspects or changes.
pub fn data_dir_override() -> Option<PathBuf> {
    match std::env::var_os("MINWIN_DATA_DIR") {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

#[cfg(windows)]
fn local_app_data() -> Result<PathBuf> {
    // SHGetKnownFolderPath is the documented way to resolve this folder and,
    // unlike %LOCALAPPDATA%, it cannot be redirected by a crafted environment
    // block. MinWin runs elevated, so that distinction matters.
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    // SAFETY: `FOLDERID_LocalAppData` is a valid known-folder id. Windows
    // allocates the returned string with CoTaskMemAlloc and the contract is
    // that the caller frees it with CoTaskMemFree, which we do unconditionally
    // before propagating any decoding failure.
    let wide = unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None) }
        .map_err(|e| crate::core::error::from_win32("resolve the LocalAppData folder", e))?;
    let decoded = unsafe { wide.to_string() };
    unsafe { CoTaskMemFree(Some(wide.as_ptr().cast())) };

    let path = decoded.map_err(|e| MinWinError::NoAppDataDirectory {
        reason: format!("the folder path is not valid UTF-16: {e}"),
    })?;
    if path.is_empty() {
        return Err(MinWinError::NoAppDataDirectory {
            reason: "Windows returned an empty path".into(),
        });
    }
    Ok(PathBuf::from(path))
}

#[cfg(not(windows))]
fn local_app_data() -> Result<PathBuf> {
    Err(MinWinError::NoAppDataDirectory {
        reason: "MinWin only runs on Windows".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rooted_paths_all_live_under_the_root() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = AppPaths::rooted_at(temp.path().join("MinWin")).expect("create");

        assert!(paths.database().starts_with(paths.root()));
        assert!(paths.lock_file().starts_with(paths.root()));
        assert!(paths.log_directory().starts_with(paths.root()));
        assert_eq!(paths.database().file_name().unwrap(), "state.db");
        assert!(paths.log_directory().is_dir());
    }

    #[test]
    fn rooting_is_idempotent() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("MinWin");
        let first = AppPaths::rooted_at(root.clone()).expect("first");
        let second = AppPaths::rooted_at(root).expect("second");
        assert_eq!(first.root(), second.root());
    }
}
