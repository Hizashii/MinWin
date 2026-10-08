//! Measurement.
//!
//! MinWin's benchmark is a **repeatable baseline of system-level state**, not a
//! performance benchmark. It does not measure frame rates, disk throughput or
//! application launch time, and it makes no claim to scientific rigour: a
//! desktop running Windows is never idle, and a ten-sample window cannot
//! control for what the machine decided to do during it.
//!
//! What it *is* good for is noticing obvious, sustained differences in how much
//! background work the system is doing, and doing so the same way every time so
//! two runs are worth putting side by side.
//!
//! The honesty of the result comes from the separation in these modules:
//! [`collector`] produces raw samples, [`model`] summarises them with order
//! statistics, and [`compare`] is the only place allowed to say whether a
//! difference means anything.

pub mod collector;
pub mod compare;
pub mod model;
pub mod stats;

pub use collector::{InstantPacer, MetricCollector, Pacer, SleepPacer, collect_run};
pub use compare::{BenchmarkComparison, Classification, MetricComparison};
pub use model::{
    BenchmarkRun, BenchmarkSample, BenchmarkSummary, MetricDirection, MetricId, MetricSummary,
    MetricUnit, RunEnvironment, SamplingPlan,
};
