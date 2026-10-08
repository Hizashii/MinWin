//! Benchmark orchestration: measure, persist, and compare against the
//! previous run.

use serde::{Deserialize, Serialize};

use crate::benchmark::compare::BenchmarkComparison;
use crate::benchmark::model::{BenchmarkRun, BenchmarkSummary, SamplingPlan};
use crate::benchmark::{Pacer, collect_run};
use crate::core::clock::Clock;
use crate::core::error::Result;
use crate::state::Database;
use crate::sys::SystemFacts;
use crate::sys::traits::Machine;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub run_id: i64,
    pub plan: SamplingPlan,
    pub summary: BenchmarkSummary,
    pub total_physical_bytes: u64,
    pub uptime_seconds: u64,
    pub windows_label: String,
    pub elevated: bool,
    /// Present when a previous run exists to compare against.
    pub comparison: Option<BenchmarkComparison>,
    /// The number of runs now stored, so the user knows what history exists.
    pub stored_run_count: u32,
    /// Metrics that could not be read at all during this run, named so the
    /// user is not left wondering why a row is missing.
    pub unreadable_metrics: Vec<String>,
}

impl BenchmarkReport {
    pub fn is_first_run(&self) -> bool {
        self.comparison.is_none()
    }
}

/// Runs a benchmark, stores it, and compares it with the previous run.
///
/// The previous run is read *before* the new one is stored, so "previous" means
/// the run before this one rather than this one.
pub fn run_benchmark(
    machine: &dyn Machine,
    facts: &SystemFacts,
    database: &mut Database,
    plan: SamplingPlan,
    label: Option<&str>,
    clock: &dyn Clock,
    pacer: &dyn Pacer,
) -> Result<BenchmarkReport> {
    let previous = database.latest_benchmark_run()?;

    let run = collect_run(machine, facts, plan, clock, pacer)?;
    let run_id = database.insert_benchmark_run(&run, label)?;

    let summary = run.summarise();
    let comparison = previous.as_ref().map(|previous| {
        let previous_summary = previous.summarise();
        BenchmarkComparison::build(
            &previous_summary,
            &summary,
            previous.id,
            Some(run_id),
            previous.started_at,
            run.started_at,
            previous.environment.windows_build != run.environment.windows_build,
        )
    });

    Ok(BenchmarkReport {
        run_id,
        plan,
        total_physical_bytes: run.environment.total_physical_bytes,
        uptime_seconds: run.environment.uptime_seconds_at_start,
        windows_label: run.environment.windows_label.clone(),
        elevated: run.environment.elevated,
        unreadable_metrics: unreadable_metrics(&run),
        summary,
        comparison,
        stored_run_count: database.benchmark_run_count()?,
    })
}

/// Metrics for which no sample in the run produced a value.
fn unreadable_metrics(run: &BenchmarkRun) -> Vec<String> {
    crate::benchmark::MetricId::ALL
        .into_iter()
        .filter(|metric| run.values_for(*metric).is_empty())
        .map(|metric| metric.label().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::benchmark::compare::Classification;
    use crate::benchmark::{InstantPacer, MetricId};
    use crate::core::clock::FixedClock;
    use crate::sys::fake::FakeMachine;

    fn plan(samples: u32) -> SamplingPlan {
        SamplingPlan {
            samples,
            interval_ms: 0,
            settle_ms: 0,
        }
    }

    fn benchmark(machine: &FakeMachine, database: &mut Database, samples: u32) -> BenchmarkReport {
        let facts = SystemFacts::gather(machine).expect("facts");
        run_benchmark(
            machine,
            &facts,
            database,
            plan(samples),
            None,
            &FixedClock::stepping(chrono::Utc::now(), 1),
            &InstantPacer,
        )
        .expect("benchmark")
    }

    #[test]
    fn the_first_run_is_stored_and_has_nothing_to_compare_against() {
        let machine = FakeMachine::windows_11();
        let mut database = Database::open_in_memory().expect("database");
        let report = benchmark(&machine, &mut database, 10);

        assert!(report.is_first_run());
        assert!(report.comparison.is_none());
        assert_eq!(report.stored_run_count, 1);
        assert!(report.unreadable_metrics.is_empty());

        // Real statistics from the fake machine's readings.
        let processes = report
            .summary
            .get(MetricId::ProcessCount)
            .expect("process count");
        assert_eq!(processes.median, 162.0);
        assert_eq!(processes.sample_count, 10);

        let cpu = report.summary.get(MetricId::CpuBusyPercent).expect("cpu");
        // The fake CPU series has 10 entries; one is consumed by priming.
        assert_eq!(cpu.sample_count, 9);
    }

    #[test]
    fn benchmarking_never_writes_to_the_machine() {
        let machine = FakeMachine::windows_11();
        let mut database = Database::open_in_memory().expect("database");
        benchmark(&machine, &mut database, 10);
        assert!(machine.writes().is_empty());
    }

    #[test]
    fn a_second_identical_run_reports_no_clear_difference_anywhere() {
        // The most important anti-placebo property: measuring twice on an
        // unchanged machine must not manufacture an improvement.
        let machine = FakeMachine::windows_11();
        let mut database = Database::open_in_memory().expect("database");
        benchmark(&machine, &mut database, 10);
        let second = benchmark(&machine, &mut database, 10);

        let comparison = second.comparison.expect("comparison");
        assert_eq!(comparison.improved_count(), 0);
        assert_eq!(comparison.regressed_count(), 0);
        for metric in &comparison.metrics {
            assert_eq!(
                metric.classification,
                Classification::NoClearDifference,
                "{} should show no difference",
                metric.metric.key()
            );
        }
    }

    #[test]
    fn a_genuine_memory_gain_is_reported_as_an_improvement() {
        let mut database = Database::open_in_memory().expect("database");
        let before = FakeMachine::windows_11().with_available_memory(20 * 1024 * 1024 * 1024);
        benchmark(&before, &mut database, 10);

        // 1 GB more available memory: well past the 128 MiB floor.
        let after = FakeMachine::windows_11().with_available_memory(21 * 1024 * 1024 * 1024);
        let report = benchmark(&after, &mut database, 10);

        let comparison = report.comparison.expect("comparison");
        let memory = comparison
            .get(MetricId::MemoryAvailableBytes)
            .expect("memory");
        assert_eq!(memory.classification, Classification::Improved);
        assert_eq!(memory.describe_delta(), "+1.00 GB");
    }

    #[test]
    fn a_small_memory_change_is_not_claimed_as_an_improvement() {
        let mut database = Database::open_in_memory().expect("database");
        let before = FakeMachine::windows_11().with_available_memory(20 * 1024 * 1024 * 1024);
        benchmark(&before, &mut database, 10);

        // 64 MiB: a real arithmetic difference, below the reporting floor.
        let after = FakeMachine::windows_11()
            .with_available_memory(20 * 1024 * 1024 * 1024 + 64 * 1024 * 1024);
        let report = benchmark(&after, &mut database, 10);

        assert_eq!(
            report
                .comparison
                .expect("comparison")
                .get(MetricId::MemoryAvailableBytes)
                .expect("memory")
                .classification,
            Classification::NoClearDifference
        );
    }

    #[test]
    fn a_short_run_cannot_support_a_comparison() {
        let machine = FakeMachine::windows_11();
        let mut database = Database::open_in_memory().expect("database");
        benchmark(&machine, &mut database, 10);
        // Four samples is below the five-sample minimum.
        let report = benchmark(&machine, &mut database, 4);

        let comparison = report.comparison.expect("comparison");
        for metric in &comparison.metrics {
            assert_eq!(
                metric.classification,
                Classification::InsufficientData,
                "{} should be insufficient",
                metric.metric.key()
            );
        }
    }

    #[test]
    fn a_metric_that_never_read_is_named_rather_than_silently_missing() {
        let machine = FakeMachine::windows_11().with_cpu_series(vec![]);
        let mut database = Database::open_in_memory().expect("database");
        let report = benchmark(&machine, &mut database, 10);

        assert_eq!(report.unreadable_metrics, vec!["CPU activity".to_string()]);
        assert!(report.summary.get(MetricId::CpuBusyPercent).is_none());
    }

    #[test]
    fn a_windows_upgrade_between_runs_is_flagged_on_the_comparison() {
        let machine = FakeMachine::windows_11();
        let mut database = Database::open_in_memory().expect("database");
        benchmark(&machine, &mut database, 10);

        let facts = {
            let mut facts = SystemFacts::gather(&machine).expect("facts");
            facts.windows.build = 27000;
            facts.windows.display_version = Some("25H2".into());
            facts
        };
        let report = run_benchmark(
            &machine,
            &facts,
            &mut database,
            plan(10),
            None,
            &FixedClock::stepping(chrono::Utc::now(), 1),
            &InstantPacer,
        )
        .expect("benchmark");

        assert!(report.comparison.expect("comparison").windows_build_changed);
    }

    #[test]
    fn raw_samples_are_persisted_so_a_better_analysis_can_be_applied_later() {
        let machine = FakeMachine::windows_11();
        let mut database = Database::open_in_memory().expect("database");
        let report = benchmark(&machine, &mut database, 10);

        let stored = database.benchmark_run(report.run_id).expect("load");
        assert_eq!(stored.samples.len(), 10 * MetricId::ALL.len());
        assert_eq!(
            stored.values_for(MetricId::CpuBusyPercent).len(),
            9,
            "every individual reading must survive, not just the summary"
        );
    }
}
