//! Single-instance guard for mutating operations.
//!
//! `apply` and `rollback` must never interleave: two processes planning against
//! the same machine would record each other's changes as their own "original"
//! state, which would quietly destroy rollback correctness.
//!
//! The lock is a file opened for exclusive access (`dwShareMode == 0`). The
//! kernel releases the handle when the process exits, including on a crash or
//! a `Ctrl+C`, so MinWin cannot leave a stale lock behind that needs manual
//! cleanup. Read-only commands (`status`, `benchmark`, `diff`) do not take it.

use std::path::{Path, PathBuf};

use crate::core::error::{MinWinError, Result};

/// Held for the duration of a mutating operation. Dropping it releases the
/// lock.
#[derive(Debug)]
pub struct ProcessLock {
    path: PathBuf,
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
    #[cfg(not(windows))]
    _file: std::fs::File,
}

impl ProcessLock {
    #[cfg(windows)]
    pub fn acquire(path: &Path) -> Result<Self> {
        use windows::Win32::Foundation::{ERROR_SHARING_VIOLATION, GENERIC_WRITE};
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE, OPEN_ALWAYS,
        };
        use windows::core::HSTRING;

        let wide = HSTRING::from(path.as_os_str());
        // SAFETY: `wide` outlives the call. FILE_SHARE_MODE(0) requests
        // exclusive access, which is what makes this a lock.
        let handle = unsafe {
            CreateFileW(
                &wide,
                GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };

        match handle {
            Ok(handle) => Ok(Self {
                path: path.to_path_buf(),
                handle,
            }),
            Err(error) if error.code() == ERROR_SHARING_VIOLATION.to_hresult() => {
                Err(MinWinError::AlreadyRunning {
                    path: path.to_path_buf(),
                })
            }
            Err(error) => Err(crate::core::error::from_win32(
                format!("open the MinWin lock file at {}", path.display()),
                error,
            )),
        }
    }

    #[cfg(not(windows))]
    pub fn acquire(path: &Path) -> Result<Self> {
        // Non-Windows builds exist only so the pure logic can be compiled and
        // tested off-platform; there is nothing to serialise against.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|e| MinWinError::io(format!("open lock file {}", path.display()), e))?;
        Ok(Self {
            path: path.to_path_buf(),
            _file: file,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(windows)]
impl Drop for ProcessLock {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileW and is closed exactly once.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_second_acquisition_is_rejected_while_the_first_is_held() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("minwin.lock");

        let held = ProcessLock::acquire(&path).expect("first lock");
        let second = ProcessLock::acquire(&path);
        assert!(
            matches!(second, Err(MinWinError::AlreadyRunning { .. })),
            "expected AlreadyRunning, got {second:?}"
        );

        drop(held);
        // Releasing the handle must make the lock available again.
        ProcessLock::acquire(&path).expect("lock after release");
    }
}
