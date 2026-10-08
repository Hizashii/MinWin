//! The system layer: plain state types, the traits that read and write them,
//! the real Windows implementation and a fake for tests.

pub mod command;
pub mod fake;
pub mod model;
pub mod traits;

#[cfg(windows)]
pub mod windows;

use crate::core::error::{MinWinError, Result};
use crate::sys::model::{ManagementState, WindowsVersion};
use crate::sys::traits::Machine;

/// Builds the live machine adapter.
///
/// This is the only constructor the CLI uses. A fake is never reachable from a
/// production code path.
#[cfg(windows)]
pub fn live_machine() -> Result<Box<dyn Machine>> {
    Ok(Box::new(windows::WindowsMachine::new()))
}

#[cfg(not(windows))]
pub fn live_machine() -> Result<Box<dyn Machine>> {
    Err(MinWinError::Unsupported(
        "MinWin inspects and changes Windows configuration, so it only runs on Windows".into(),
    ))
}

/// The facts MinWin reads once per command and then passes around, so that a
/// single invocation cannot disagree with itself about what machine it is on.
#[derive(Debug, Clone)]
pub struct SystemFacts {
    pub windows: WindowsVersion,
    pub elevated: bool,
    pub management: ManagementState,
}

impl SystemFacts {
    pub fn gather(machine: &dyn Machine) -> Result<Self> {
        let info = machine.info();
        Ok(Self {
            windows: info.windows_version()?,
            elevated: info.is_elevated()?,
            management: info.management_state()?,
        })
    }

    /// Fails with the privilege guidance message unless the process is
    /// elevated. `suggestion` is the command the user should re-run.
    pub fn require_elevation(&self, suggestion: &str) -> Result<()> {
        if self.elevated {
            Ok(())
        } else {
            Err(MinWinError::RequiresElevation {
                suggestion: suggestion.to_string(),
            })
        }
    }
}

/// The platform MinWin is being asked to inspect, for the one case where the
/// answer is "not this one".
#[cfg(not(windows))]
#[allow(dead_code)]
const _NON_WINDOWS_NOTE: &str = "non-Windows builds exist only to compile and test the pure logic";
