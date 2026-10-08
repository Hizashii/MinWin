//! Business logic.
//!
//! Every command's actual work lives here and returns a structured,
//! serialisable report. The CLI's only job is to parse arguments, ask for
//! confirmation, and render those reports. That split is what would let a GUI
//! or TUI reuse this crate without reimplementing a single decision.
//!
//! No function in this module prints anything.

pub mod apply;
pub mod bench;
pub mod diff;
pub mod rollback;
pub mod status;

pub use apply::{ApplyOutcome, ApplyPlan};
pub use bench::{BenchmarkReport, run_benchmark};
pub use diff::{DiffReport, DiffStatus};
pub use rollback::{RollbackAuthorisation, RollbackOutcome, RollbackPlan};
pub use status::StatusReport;
