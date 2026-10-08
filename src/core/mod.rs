//! Cross-cutting foundations: errors, time, MinWin's own file locations and
//! the single-instance lock.

pub mod clock;
pub mod error;
pub mod lock;
pub mod paths;

/// MinWin's own version, as reported by `status` and recorded in every session
/// so that state written by an older build is identifiable.
pub const MINWIN_VERSION: &str = env!("CARGO_PKG_VERSION");
