//! MinWin — make Windows lighter without blindly disabling things.
//!
//! The library is split so that a future GUI or TUI can consume exactly what
//! the CLI consumes:
//!
//! * [`sys`] is the only code that talks to Windows, behind traits.
//! * [`changes`] defines what a reversible system change *is*, and registers
//!   the small set MinWin supports.
//! * [`benchmark`] measures the machine and compares runs conservatively.
//! * [`profiles`] parses the human-readable TOML profiles.
//! * [`state`] persists benchmark runs, apply sessions and rollback records.
//! * [`engine`] holds the business logic for status, benchmark, apply, diff and
//!   rollback, and returns structured reports.
//! * [`cli`] only parses arguments and renders those reports.
//!
//! No module above [`sys`] contains a Windows API call, which is why the test
//! suite can drive the whole engine against a fake machine.

pub mod benchmark;
pub mod changes;
pub mod cli;
pub mod core;
pub mod engine;
pub mod profiles;
pub mod state;
pub mod sys;

pub use crate::core::MINWIN_VERSION;
pub use crate::core::error::{MinWinError, Result};
