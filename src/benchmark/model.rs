//! Benchmark data model.
//!
//! Four concepts are kept strictly separate, because conflating them is how
//! optimisation tools end up claiming improvements they never measured:
//!
//! 1. **Samples** — raw readings with a timestamp. Always persisted.
//! 2. **Summary** — order statistics over one run's samples.
//! 3. **Comparison** — the arithmetic difference between two summaries.
//! 4. **Interpretation** — whether that difference means anything.
//!
//! Steps 1 and 2 are facts. Step 3 is arithmetic. Only step 4 is a judgement,
//! and it is allowed to answer "no clear difference" or "unknown".

use serde::{Deserialize, Serialize};

/// The metrics MinWin measures. Deliberately short: a metric is only here if
/// it can be read reliably and cheaply from a documented interface.
///
/// Notably absent is boot time. Windows does expose boot timing through the
/// `Microsoft-Windows-Diagnostics-Performance` event log, but the channel is
/// not always enabled, reading it needs elevation, and the figure it reports
/// is heavily influenced by what the user had installed at the time. MinWin
/// would rather omit the number than print a misleading one. See `docs/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricId {
    MemoryAvailableBytes,
    MemoryLoadPercent,
    CpuBusyPercent,
    ProcessCount,
    ServiceRunningCount,
}

/// Whether a lower or higher reading is better — or whether MinWin declines to
/// say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricDirection {
    HigherIsBetter,
    LowerIsBetter,
    /// MinWin reports the difference but refuses to label it. Running service
    /// count is the example: fewer running services is not inherently better,
    /// it depends entirely on which ones.
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricUnit {
    Bytes,
    Percent,
    Count,
}

impl MetricId {
    pub const ALL: [MetricId; 5] = [
        MetricId::MemoryAvailableBytes,
        MetricId::MemoryLoadPercent,
        MetricId::CpuBusyPercent,
        MetricId::ProcessCount,
        MetricId::ServiceRunningCount,
    ];

    /// The stable identifier persisted in the database. Changing one of these
    /// strings would orphan existing samples, so they are treated as a schema.
    pub fn key(self) -> &'static str {
        match self {
            Self::MemoryAvailableBytes => "memory.available_bytes",
            Self::MemoryLoadPercent => "memory.load_percent",
            Self::CpuBusyPercent => "cpu.busy_percent",
            Self::ProcessCount => "process.count",
            Self::ServiceRunningCount => "service.running_count",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|id| id.key() == key)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::MemoryAvailableBytes => "Available RAM",
            Self::MemoryLoadPercent => "Memory in use",
            Self::CpuBusyPercent => "CPU activity",
            Self::ProcessCount => "Process count",
            Self::ServiceRunningCount => "Running services",
        }
    }

    pub fn unit(self) -> MetricUnit {
        match self {
            Self::MemoryAvailableBytes => MetricUnit::Bytes,
            Self::MemoryLoadPercent | Self::CpuBusyPercent => MetricUnit::Percent,
            Self::ProcessCount | Self::ServiceRunningCount => MetricUnit::Count,
        }
    }

    pub fn direction(self) -> MetricDirection {
        match self {
            Self::MemoryAvailableBytes => MetricDirection::HigherIsBetter,
            // Fewer processes at idle is the stated goal of a "lighter"
            // Windows, so this one does get a direction.
            Self::MemoryLoadPercent | Self::CpuBusyPercent | Self::ProcessCount => {
                MetricDirection::LowerIsBetter
            }
            Self::ServiceRunningCount => MetricDirection::Neutral,
        }
    }

    /// The smallest difference between two runs' medians that MinWin is willing
    /// to describe as a difference at all.
    ///
    /// These are engineering floors, not statistics. They were chosen so that
    /// ordinary idle-system variation on a normal desktop does not get reported
    /// as a change. See `docs/benchmarking.md` for the reasoning behind each
    /// number, and treat them as tunable rather than authoritative.
    pub fn minimum_meaningful_delta(self) -> f64 {
        match self {
            // 128 MiB: smaller shifts happen constantly as caches breathe.
            Self::MemoryAvailableBytes => 128.0 * 1024.0 * 1024.0,
            // 2 percentage points on both percentage metrics.
            Self::MemoryLoadPercent | Self::CpuBusyPercent => 2.0,
            // 3 processes: a browser tab or a single update check.
            Self::ProcessCount => 3.0,
            Self::ServiceRunningCount => 2.0,
        }
    }

    /// Renders a reading in the unit a person expects to read.
    pub fn format_value(self, value: f64) -> String {
        match self.unit() {
            MetricUnit::Bytes => format_bytes(value),
            MetricUnit::Percent => format!("{value:.1}%"),
            MetricUnit::Count => format!("{:.0}", value.round()),
        }
    }

    /// Renders a *difference*, always signed, so that a comparison line cannot
    /// be mistaken for an absolute reading.
    pub fn format_delta(self, delta: f64) -> String {
        let sign = if delta > 0.0 { "+" } else { "-" };
        let magnitude = delta.abs();
        match self.unit() {
            MetricUnit::Bytes => format!("{sign}{}", format_bytes(magnitude)),
            MetricUnit::Percent => format!("{sign}{magnitude:.1} points"),
            MetricUnit::Count => format!("{sign}{:.0}", magnitude.round()),
        }
    }
}

/// Binary units, because that is what Windows reports and what Task Manager
/// shows.
pub fn format_bytes(value: f64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    if value >= GIB {
        format!("{:.2} GB", value / GIB)
    } else if value >= MIB {
        format!("{:.0} MB", value / MIB)
    } else if value >= KIB {
        format!("{:.0} KB", value / KIB)
    } else {
        format!("{value:.0} B")
    }
}

/// One reading of one metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkSample {
    pub metric: MetricId,
    /// Zero-based index within the run, so that samples keep their order even
    /// when timestamps collide.
    pub sample_index: u32,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    /// `None` when the metric could not be read for this sample. MinWin stores
    /// the failure rather than substituting a value.
    pub value: Option<f64>,
    pub error: Option<String>,
}

/// How a run was configured, recorded so a later comparison can tell whether
/// two runs are even comparable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SamplingPlan {
    pub samples: u32,
    pub interval_ms: u64,
    pub settle_ms: u64,
}

impl Default for SamplingPlan {
    fn default() -> Self {
        // Ten samples one second apart, after a two-second settle. Long enough
        // to see past a single background burst, short enough that a user will
        // actually wait for it.
        Self {
            samples: 10,
            interval_ms: 1000,
            settle_ms: 2000,
        }
    }
}

impl SamplingPlan {
    /// Comparison needs a minimum number of samples before it will classify
    /// anything. Below this, MinWin reports `InsufficientData`.
    pub const MINIMUM_FOR_COMPARISON: u32 = 5;

    pub fn estimated_duration_ms(&self) -> u64 {
        self.settle_ms + self.samples.saturating_sub(1) as u64 * self.interval_ms
    }
}

/// Facts about the machine that are single-valued rather than sampled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEnvironment {
    pub minwin_version: String,
    pub windows_label: String,
    pub windows_build: u32,
    pub elevated: bool,
    pub total_physical_bytes: u64,
    pub uptime_seconds_at_start: u64,
}

/// A complete benchmark session: how it was configured, what machine it ran
/// on, and every raw reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkRun {
    /// Database id, absent until the run has been stored.
    pub id: Option<i64>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
    pub plan: SamplingPlan,
    pub environment: RunEnvironment,
    pub samples: Vec<BenchmarkSample>,
}

impl BenchmarkRun {
    pub fn values_for(&self, metric: MetricId) -> Vec<f64> {
        self.samples
            .iter()
            .filter(|sample| sample.metric == metric)
            .filter_map(|sample| sample.value)
            .collect()
    }

    pub fn summarise(&self) -> BenchmarkSummary {
        BenchmarkSummary {
            metrics: MetricId::ALL
                .into_iter()
                .filter_map(|metric| MetricSummary::of(metric, &self.values_for(metric)))
                .collect(),
        }
    }
}

/// Order statistics for one metric within one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricSummary {
    pub metric: MetricId,
    pub sample_count: u32,
    pub median: f64,
    pub minimum: f64,
    pub maximum: f64,
    pub interquartile_range: f64,
}

impl MetricSummary {
    /// `None` when there is nothing to summarise, rather than a zero that would
    /// read as a measurement.
    pub fn of(metric: MetricId, values: &[f64]) -> Option<Self> {
        Some(Self {
            metric,
            sample_count: values.len() as u32,
            median: super::stats::median(values)?,
            minimum: super::stats::minimum(values)?,
            maximum: super::stats::maximum(values)?,
            interquartile_range: super::stats::interquartile_range(values)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkSummary {
    pub metrics: Vec<MetricSummary>,
}

impl BenchmarkSummary {
    pub fn get(&self, metric: MetricId) -> Option<&MetricSummary> {
        self.metrics.iter().find(|entry| entry.metric == metric)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn sample(metric: MetricId, index: u32, value: Option<f64>) -> BenchmarkSample {
        BenchmarkSample {
            metric,
            sample_index: index,
            captured_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            value,
            error: value.is_none().then(|| "unavailable".to_string()),
        }
    }

    fn run_with(samples: Vec<BenchmarkSample>) -> BenchmarkRun {
        let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        BenchmarkRun {
            id: None,
            started_at: at,
            finished_at: at,
            plan: SamplingPlan::default(),
            environment: RunEnvironment {
                minwin_version: "0.1.0".into(),
                windows_label: "Windows 11 24H2".into(),
                windows_build: 26100,
                elevated: false,
                total_physical_bytes: 0,
                uptime_seconds_at_start: 0,
            },
            samples,
        }
    }

    #[test]
    fn metric_keys_round_trip_and_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for metric in MetricId::ALL {
            assert_eq!(MetricId::from_key(metric.key()), Some(metric));
            assert!(seen.insert(metric.key()), "duplicate key {}", metric.key());
        }
        assert_eq!(MetricId::from_key("nope"), None);
    }

    #[test]
    fn running_service_count_has_no_direction_because_fewer_is_not_better() {
        assert_eq!(
            MetricId::ServiceRunningCount.direction(),
            MetricDirection::Neutral
        );
        assert_eq!(
            MetricId::MemoryAvailableBytes.direction(),
            MetricDirection::HigherIsBetter
        );
        assert_eq!(
            MetricId::CpuBusyPercent.direction(),
            MetricDirection::LowerIsBetter
        );
    }

    #[test]
    fn failed_readings_are_excluded_from_statistics_not_counted_as_zero() {
        let run = run_with(vec![
            sample(MetricId::ProcessCount, 0, Some(160.0)),
            sample(MetricId::ProcessCount, 1, None),
            sample(MetricId::ProcessCount, 2, Some(162.0)),
        ]);
        let values = run.values_for(MetricId::ProcessCount);
        assert_eq!(values, vec![160.0, 162.0]);

        let summary = run.summarise();
        let processes = summary.get(MetricId::ProcessCount).expect("summary");
        assert_eq!(processes.sample_count, 2);
        assert_eq!(processes.median, 161.0);
    }

    #[test]
    fn a_metric_with_no_readable_samples_is_absent_from_the_summary() {
        let run = run_with(vec![sample(MetricId::CpuBusyPercent, 0, None)]);
        assert!(run.summarise().get(MetricId::CpuBusyPercent).is_none());
    }

    #[test]
    fn deltas_are_always_signed_so_they_cannot_read_as_absolutes() {
        assert_eq!(MetricId::ProcessCount.format_delta(-7.0), "-7");
        assert_eq!(MetricId::ProcessCount.format_delta(7.0), "+7");
        assert_eq!(MetricId::CpuBusyPercent.format_delta(-1.25), "-1.2 points");
        assert_eq!(
            MetricId::MemoryAvailableBytes.format_delta(430.0 * 1024.0 * 1024.0),
            "+430 MB"
        );
    }

    #[test]
    fn values_render_in_the_unit_a_person_expects() {
        assert_eq!(
            MetricId::MemoryAvailableBytes.format_value(21.4 * 1024.0 * 1024.0 * 1024.0),
            "21.40 GB"
        );
        assert_eq!(MetricId::CpuBusyPercent.format_value(1.84), "1.8%");
        assert_eq!(MetricId::ProcessCount.format_value(162.4), "162");
    }

    #[test]
    fn byte_formatting_crosses_each_unit_boundary() {
        assert_eq!(format_bytes(512.0), "512 B");
        assert_eq!(format_bytes(2048.0), "2 KB");
        assert_eq!(format_bytes(5.0 * 1024.0 * 1024.0), "5 MB");
        assert_eq!(format_bytes(3.0 * 1024.0 * 1024.0 * 1024.0), "3.00 GB");
    }

    #[test]
    fn the_default_plan_is_ten_samples_and_reports_its_duration() {
        let plan = SamplingPlan::default();
        assert_eq!(plan.samples, 10);
        assert_eq!(plan.interval_ms, 1000);
        // 2s settle + 9 intervals.
        assert_eq!(plan.estimated_duration_ms(), 11_000);
    }
}
