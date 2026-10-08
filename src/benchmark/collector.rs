//! Metric collection and the sampling loop.

use std::time::Duration;

use crate::benchmark::model::{
    BenchmarkRun, BenchmarkSample, MetricId, RunEnvironment, SamplingPlan,
};
use crate::core::clock::Clock;
use crate::core::error::Result;
use crate::sys::SystemFacts;
use crate::sys::traits::{CpuSampler, Machine};

/// One metric, one reading.
///
/// A collector returns `Ok(None)` when the metric genuinely has no value for
/// this sample (the first CPU reading has no interval behind it) and `Err` when
/// the read failed. Both are recorded; neither is replaced with a number.
pub trait MetricCollector {
    fn metric(&self) -> MetricId;
    fn collect(&mut self) -> Result<Option<f64>>;
}

struct AvailableMemory<'a>(&'a dyn Machine);

impl MetricCollector for AvailableMemory<'_> {
    fn metric(&self) -> MetricId {
        MetricId::MemoryAvailableBytes
    }

    fn collect(&mut self) -> Result<Option<f64>> {
        Ok(Some(
            self.0.info().memory()?.available_physical_bytes as f64,
        ))
    }
}

struct MemoryLoad<'a>(&'a dyn Machine);

impl MetricCollector for MemoryLoad<'_> {
    fn metric(&self) -> MetricId {
        MetricId::MemoryLoadPercent
    }

    fn collect(&mut self) -> Result<Option<f64>> {
        Ok(Some(f64::from(self.0.info().memory()?.load_percent)))
    }
}

struct CpuBusy(Box<dyn CpuSampler>);

impl MetricCollector for CpuBusy {
    fn metric(&self) -> MetricId {
        MetricId::CpuBusyPercent
    }

    fn collect(&mut self) -> Result<Option<f64>> {
        self.0.sample_busy_percent()
    }
}

struct ProcessCount<'a>(&'a dyn Machine);

impl MetricCollector for ProcessCount<'_> {
    fn metric(&self) -> MetricId {
        MetricId::ProcessCount
    }

    fn collect(&mut self) -> Result<Option<f64>> {
        Ok(Some(f64::from(self.0.processes().process_count()?)))
    }
}

struct RunningServices<'a>(&'a dyn Machine);

impl MetricCollector for RunningServices<'_> {
    fn metric(&self) -> MetricId {
        MetricId::ServiceRunningCount
    }

    fn collect(&mut self) -> Result<Option<f64>> {
        Ok(Some(f64::from(self.0.services().summarise()?.running)))
    }
}

/// How the sampler waits between readings. Tests substitute a no-op so the
/// suite does not spend eleven seconds sleeping.
pub trait Pacer {
    fn settle(&self, duration: Duration);
    fn between_samples(&self, duration: Duration);
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SleepPacer;

impl Pacer for SleepPacer {
    fn settle(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn between_samples(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// A pacer that does not wait. Used by tests, and never by the CLI.
#[derive(Debug, Default, Clone, Copy)]
pub struct InstantPacer;

impl Pacer for InstantPacer {
    fn settle(&self, _duration: Duration) {}
    fn between_samples(&self, _duration: Duration) {}
}

/// Collects a full benchmark run.
///
/// The settling period exists because the act of starting MinWin itself
/// disturbs the machine, and because a user typically runs this right after
/// doing something else. It also primes the CPU sampler, so that the first
/// recorded sample already has an interval behind it instead of being lost.
pub fn collect_run(
    machine: &dyn Machine,
    facts: &SystemFacts,
    plan: SamplingPlan,
    clock: &dyn Clock,
    pacer: &dyn Pacer,
) -> Result<BenchmarkRun> {
    let started_at = clock.now();
    let total_physical_bytes = machine.info().memory()?.total_physical_bytes;
    let uptime_seconds_at_start = machine.info().uptime_seconds()?;

    let mut collectors: Vec<Box<dyn MetricCollector + '_>> = vec![
        Box::new(AvailableMemory(machine)),
        Box::new(MemoryLoad(machine)),
        Box::new(CpuBusy(machine.cpu_sampler()?)),
        Box::new(ProcessCount(machine)),
        Box::new(RunningServices(machine)),
    ];

    // Prime: discard one round of readings so the CPU sampler has a baseline.
    for collector in collectors.iter_mut() {
        let _ = collector.collect();
    }
    pacer.settle(Duration::from_millis(plan.settle_ms));

    let mut samples = Vec::with_capacity(plan.samples as usize * collectors.len());
    for index in 0..plan.samples {
        if index > 0 {
            pacer.between_samples(Duration::from_millis(plan.interval_ms));
        }
        let captured_at = clock.now();
        for collector in collectors.iter_mut() {
            let metric = collector.metric();
            let (value, error) = match collector.collect() {
                Ok(Some(value)) => (Some(value), None),
                Ok(None) => (None, Some("no value available for this sample".to_string())),
                Err(error) => {
                    tracing::warn!(metric = metric.key(), %error, "a metric could not be read");
                    (None, Some(error.to_string()))
                }
            };
            samples.push(BenchmarkSample {
                metric,
                sample_index: index,
                captured_at,
                value,
                error,
            });
        }
    }

    Ok(BenchmarkRun {
        id: None,
        started_at,
        finished_at: clock.now(),
        plan,
        environment: RunEnvironment {
            minwin_version: crate::core::MINWIN_VERSION.to_string(),
            windows_label: facts.windows.label(),
            windows_build: facts.windows.build,
            elevated: facts.elevated,
            total_physical_bytes,
            uptime_seconds_at_start,
        },
        samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::clock::FixedClock;
    use crate::sys::fake::FakeMachine;

    fn run_against(machine: &FakeMachine, plan: SamplingPlan) -> BenchmarkRun {
        let facts = SystemFacts::gather(machine).expect("facts");
        collect_run(
            machine,
            &facts,
            plan,
            &FixedClock::stepping(chrono::Utc::now(), 1),
            &InstantPacer,
        )
        .expect("run")
    }

    #[test]
    fn a_run_collects_every_metric_for_every_sample() {
        let machine = FakeMachine::windows_11();
        let plan = SamplingPlan {
            samples: 10,
            interval_ms: 0,
            settle_ms: 0,
        };
        let run = run_against(&machine, plan);

        assert_eq!(run.samples.len(), 10 * MetricId::ALL.len());
        for metric in MetricId::ALL {
            assert_eq!(
                run.samples.iter().filter(|s| s.metric == metric).count(),
                10,
                "{} should have ten samples",
                metric.key()
            );
        }
    }

    #[test]
    fn sample_indices_are_dense_and_ordered() {
        let machine = FakeMachine::windows_11();
        let run = run_against(
            &machine,
            SamplingPlan {
                samples: 4,
                interval_ms: 0,
                settle_ms: 0,
            },
        );
        let indices: Vec<u32> = run
            .samples
            .iter()
            .filter(|s| s.metric == MetricId::ProcessCount)
            .map(|s| s.sample_index)
            .collect();
        assert_eq!(indices, vec![0, 1, 2, 3]);
    }

    #[test]
    fn priming_means_the_first_cpu_sample_already_has_a_value() {
        // The fake CPU series is consumed one entry per call. The priming call
        // takes the first entry, so sample 0 must still have a real value.
        let machine = FakeMachine::windows_11().with_cpu_series(vec![9.9, 1.0, 2.0, 3.0]);
        let run = run_against(
            &machine,
            SamplingPlan {
                samples: 3,
                interval_ms: 0,
                settle_ms: 0,
            },
        );
        let cpu: Vec<Option<f64>> = run
            .samples
            .iter()
            .filter(|s| s.metric == MetricId::CpuBusyPercent)
            .map(|s| s.value)
            .collect();
        assert_eq!(cpu, vec![Some(1.0), Some(2.0), Some(3.0)]);
    }

    #[test]
    fn an_exhausted_metric_records_the_gap_instead_of_a_zero() {
        let machine = FakeMachine::windows_11().with_cpu_series(vec![1.0, 2.0]);
        let run = run_against(
            &machine,
            SamplingPlan {
                samples: 3,
                interval_ms: 0,
                settle_ms: 0,
            },
        );
        let cpu: Vec<Option<f64>> = run
            .samples
            .iter()
            .filter(|s| s.metric == MetricId::CpuBusyPercent)
            .map(|s| s.value)
            .collect();
        assert_eq!(cpu, vec![Some(2.0), None, None]);

        let missing: Vec<&BenchmarkSample> = run
            .samples
            .iter()
            .filter(|s| s.metric == MetricId::CpuBusyPercent && s.value.is_none())
            .collect();
        assert!(missing.iter().all(|s| s.error.is_some()));
    }

    #[test]
    fn the_environment_records_the_machine_the_run_happened_on() {
        let machine = FakeMachine::windows_11();
        let run = run_against(
            &machine,
            SamplingPlan {
                samples: 2,
                interval_ms: 0,
                settle_ms: 0,
            },
        );
        assert_eq!(run.environment.windows_build, 26100);
        assert_eq!(run.environment.windows_label, "Windows 11 24H2");
        assert_eq!(
            run.environment.total_physical_bytes,
            32 * 1024 * 1024 * 1024
        );
        assert_eq!(run.environment.minwin_version, crate::core::MINWIN_VERSION);
    }

    #[test]
    fn benchmarking_never_writes_to_the_machine() {
        let machine = FakeMachine::windows_11();
        run_against(
            &machine,
            SamplingPlan {
                samples: 5,
                interval_ms: 0,
                settle_ms: 0,
            },
        );
        assert!(
            machine.writes().is_empty(),
            "benchmarking must be read-only, saw {:?}",
            machine.writes()
        );
    }

    #[test]
    fn summarising_a_collected_run_produces_statistics_for_each_metric() {
        let machine = FakeMachine::windows_11();
        let run = run_against(
            &machine,
            SamplingPlan {
                samples: 10,
                interval_ms: 0,
                settle_ms: 0,
            },
        );
        let summary = run.summarise();
        // The fake returns a constant process count, so the IQR must be zero.
        let processes = summary.get(MetricId::ProcessCount).expect("processes");
        assert_eq!(processes.median, 162.0);
        assert_eq!(processes.interquartile_range, 0.0);
        assert_eq!(processes.sample_count, 10);
    }
}
