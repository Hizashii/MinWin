//! Comparing two benchmark runs — conservatively.
//!
//! This module is where MinWin earns or loses its credibility, so the rules are
//! stated explicitly rather than buried in arithmetic.
//!
//! For each metric present in both runs:
//!
//! ```text
//! delta     = median(current) - median(baseline)
//! spread    = max(IQR(baseline), IQR(current))
//! threshold = max(metric.minimum_meaningful_delta, spread)
//!
//! if either run has fewer than 5 readable samples  -> InsufficientData
//! else if |delta| <= threshold                     -> NoClearDifference
//! else if the metric has no direction              -> Unknown (delta reported)
//! else                                             -> Improved / Regressed
//! ```
//!
//! What this is **not**: a significance test. There is no p-value, no
//! confidence interval and no distributional assumption. Using the observed
//! spread as the threshold means a noisy machine needs a larger difference
//! before MinWin will call it anything, which is the behaviour you want from a
//! tool that must not overclaim. A more rigorous treatment is future work; the
//! data model keeps every raw sample so it can be applied retroactively.

use serde::{Deserialize, Serialize};

use crate::benchmark::model::{
    BenchmarkSummary, MetricDirection, MetricId, MetricSummary, SamplingPlan,
};

/// MinWin's verdict on one metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    Improved,
    Regressed,
    /// The difference is within the noise MinWin is prepared to attribute to
    /// background activity.
    NoClearDifference,
    /// Too few readable samples in one or both runs.
    InsufficientData,
    /// A real difference, but the metric has no defensible direction, so MinWin
    /// reports the number and declines to judge it.
    Unknown,
}

impl Classification {
    pub fn label(self) -> &'static str {
        match self {
            Self::Improved => "improved",
            Self::Regressed => "regressed",
            Self::NoClearDifference => "difference within measurement noise",
            Self::InsufficientData => "insufficient data",
            Self::Unknown => "changed, direction not interpreted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricComparison {
    pub metric: MetricId,
    pub baseline: Option<MetricSummary>,
    pub current: Option<MetricSummary>,
    /// `current.median - baseline.median`, present whenever both runs have a
    /// summary for the metric.
    pub median_delta: Option<f64>,
    /// The threshold the delta was tested against, exposed so the user can see
    /// why MinWin stayed quiet.
    pub threshold: Option<f64>,
    pub classification: Classification,
}

impl MetricComparison {
    /// A rendered line such as `+430 MB` or `difference within measurement
    /// noise`, with no interpretation attached.
    pub fn describe_delta(&self) -> String {
        match self.median_delta {
            Some(delta) => self.metric.format_delta(delta),
            None => "not measured in both runs".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkComparison {
    pub baseline_run_id: Option<i64>,
    pub current_run_id: Option<i64>,
    pub baseline_started_at: chrono::DateTime<chrono::Utc>,
    pub current_started_at: chrono::DateTime<chrono::Utc>,
    /// True when the two runs saw different Windows builds, which makes the
    /// comparison unsound. Reported rather than silently ignored.
    pub windows_build_changed: bool,
    pub metrics: Vec<MetricComparison>,
}

impl BenchmarkComparison {
    /// Compares two summaries. Caller supplies run metadata; this function does
    /// only arithmetic and classification.
    pub fn build(
        baseline: &BenchmarkSummary,
        current: &BenchmarkSummary,
        baseline_run_id: Option<i64>,
        current_run_id: Option<i64>,
        baseline_started_at: chrono::DateTime<chrono::Utc>,
        current_started_at: chrono::DateTime<chrono::Utc>,
        windows_build_changed: bool,
    ) -> Self {
        let metrics = MetricId::ALL
            .into_iter()
            .map(|metric| compare_metric(metric, baseline.get(metric), current.get(metric)))
            .collect();

        Self {
            baseline_run_id,
            current_run_id,
            baseline_started_at,
            current_started_at,
            windows_build_changed,
            metrics,
        }
    }

    pub fn get(&self, metric: MetricId) -> Option<&MetricComparison> {
        self.metrics.iter().find(|entry| entry.metric == metric)
    }

    /// Metrics MinWin is prepared to describe as improved. Used only for a
    /// summary count; never for a headline percentage.
    pub fn improved_count(&self) -> usize {
        self.count_of(Classification::Improved)
    }

    pub fn regressed_count(&self) -> usize {
        self.count_of(Classification::Regressed)
    }

    fn count_of(&self, classification: Classification) -> usize {
        self.metrics
            .iter()
            .filter(|entry| entry.classification == classification)
            .count()
    }
}

fn compare_metric(
    metric: MetricId,
    baseline: Option<&MetricSummary>,
    current: Option<&MetricSummary>,
) -> MetricComparison {
    let (Some(baseline), Some(current)) = (baseline, current) else {
        return MetricComparison {
            metric,
            baseline: baseline.cloned(),
            current: current.cloned(),
            median_delta: None,
            threshold: None,
            classification: Classification::InsufficientData,
        };
    };

    let delta = current.median - baseline.median;
    let spread = baseline
        .interquartile_range
        .max(current.interquartile_range);
    let threshold = metric.minimum_meaningful_delta().max(spread);

    let minimum = SamplingPlan::MINIMUM_FOR_COMPARISON;
    let classification = if baseline.sample_count < minimum || current.sample_count < minimum {
        Classification::InsufficientData
    } else if delta.abs() <= threshold {
        Classification::NoClearDifference
    } else {
        match metric.direction() {
            MetricDirection::HigherIsBetter => {
                if delta > 0.0 {
                    Classification::Improved
                } else {
                    Classification::Regressed
                }
            }
            MetricDirection::LowerIsBetter => {
                if delta < 0.0 {
                    Classification::Improved
                } else {
                    Classification::Regressed
                }
            }
            MetricDirection::Neutral => Classification::Unknown,
        }
    };

    MetricComparison {
        metric,
        baseline: Some(baseline.clone()),
        current: Some(current.clone()),
        median_delta: Some(delta),
        threshold: Some(threshold),
        classification,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(metric: MetricId, median: f64, iqr: f64, count: u32) -> MetricSummary {
        MetricSummary {
            metric,
            sample_count: count,
            median,
            minimum: median - iqr,
            maximum: median + iqr,
            interquartile_range: iqr,
        }
    }

    fn classify(
        metric: MetricId,
        baseline: (f64, f64, u32),
        current: (f64, f64, u32),
    ) -> Classification {
        compare_metric(
            metric,
            Some(&summary(metric, baseline.0, baseline.1, baseline.2)),
            Some(&summary(metric, current.0, current.1, current.2)),
        )
        .classification
    }

    const MIB: f64 = 1024.0 * 1024.0;

    #[test]
    fn a_large_gain_in_available_memory_is_an_improvement() {
        assert_eq!(
            classify(
                MetricId::MemoryAvailableBytes,
                (20_000.0 * MIB, 50.0 * MIB, 10),
                (20_500.0 * MIB, 50.0 * MIB, 10),
            ),
            Classification::Improved
        );
    }

    #[test]
    fn a_large_loss_of_available_memory_is_a_regression() {
        assert_eq!(
            classify(
                MetricId::MemoryAvailableBytes,
                (20_500.0 * MIB, 50.0 * MIB, 10),
                (20_000.0 * MIB, 50.0 * MIB, 10),
            ),
            Classification::Regressed
        );
    }

    #[test]
    fn a_change_below_the_absolute_floor_is_not_reported_as_a_difference() {
        // 64 MiB is a real arithmetic difference but below the 128 MiB floor.
        assert_eq!(
            classify(
                MetricId::MemoryAvailableBytes,
                (20_000.0 * MIB, 1.0 * MIB, 10),
                (20_064.0 * MIB, 1.0 * MIB, 10),
            ),
            Classification::NoClearDifference
        );
    }

    #[test]
    fn a_noisy_machine_needs_a_bigger_difference_before_minwin_will_speak() {
        // Same 5-point CPU delta, but the second case has an IQR of 8 points,
        // so the threshold rises above the delta and MinWin stays quiet.
        assert_eq!(
            classify(MetricId::CpuBusyPercent, (8.0, 0.5, 10), (3.0, 0.5, 10)),
            Classification::Improved
        );
        assert_eq!(
            classify(MetricId::CpuBusyPercent, (8.0, 8.0, 10), (3.0, 8.0, 10)),
            Classification::NoClearDifference
        );
    }

    #[test]
    fn fewer_than_five_readable_samples_yields_insufficient_data() {
        assert_eq!(
            classify(MetricId::ProcessCount, (160.0, 1.0, 4), (120.0, 1.0, 10)),
            Classification::InsufficientData
        );
        assert_eq!(
            classify(MetricId::ProcessCount, (160.0, 1.0, 10), (120.0, 1.0, 4)),
            Classification::InsufficientData
        );
        // Exactly five is enough.
        assert_eq!(
            classify(MetricId::ProcessCount, (160.0, 1.0, 5), (120.0, 1.0, 5)),
            Classification::Improved
        );
    }

    #[test]
    fn a_neutral_metric_reports_the_delta_but_refuses_to_judge_it() {
        let comparison = compare_metric(
            MetricId::ServiceRunningCount,
            Some(&summary(MetricId::ServiceRunningCount, 118.0, 1.0, 10)),
            Some(&summary(MetricId::ServiceRunningCount, 110.0, 1.0, 10)),
        );
        assert_eq!(comparison.classification, Classification::Unknown);
        assert_eq!(comparison.median_delta, Some(-8.0));
        assert_eq!(comparison.describe_delta(), "-8");
    }

    #[test]
    fn a_metric_missing_from_one_run_is_insufficient_data_not_a_zero_delta() {
        let comparison = compare_metric(
            MetricId::CpuBusyPercent,
            Some(&summary(MetricId::CpuBusyPercent, 2.0, 0.5, 10)),
            None,
        );
        assert_eq!(comparison.classification, Classification::InsufficientData);
        assert_eq!(comparison.median_delta, None);
        assert_eq!(comparison.describe_delta(), "not measured in both runs");
    }

    #[test]
    fn an_identical_run_shows_no_differences_anywhere() {
        let mut summary_a = BenchmarkSummary { metrics: vec![] };
        for metric in MetricId::ALL {
            summary_a.metrics.push(summary(metric, 100.0, 1.0, 10));
        }
        let comparison = BenchmarkComparison::build(
            &summary_a,
            &summary_a,
            Some(1),
            Some(2),
            chrono::Utc::now(),
            chrono::Utc::now(),
            false,
        );
        assert_eq!(comparison.improved_count(), 0);
        assert_eq!(comparison.regressed_count(), 0);
        assert!(
            comparison
                .metrics
                .iter()
                .all(|entry| entry.classification == Classification::NoClearDifference)
        );
    }

    #[test]
    fn the_threshold_is_exposed_so_a_quiet_verdict_can_be_explained() {
        let comparison = compare_metric(
            MetricId::ProcessCount,
            Some(&summary(MetricId::ProcessCount, 160.0, 6.0, 10)),
            Some(&summary(MetricId::ProcessCount, 158.0, 4.0, 10)),
        );
        // max(floor of 3, spread of 6) == 6
        assert_eq!(comparison.threshold, Some(6.0));
        assert_eq!(comparison.classification, Classification::NoClearDifference);
    }

    #[test]
    fn a_delta_exactly_at_the_threshold_is_not_claimed() {
        // Boundary is inclusive on the "no difference" side by design.
        assert_eq!(
            classify(MetricId::ProcessCount, (160.0, 0.0, 10), (157.0, 0.0, 10)),
            Classification::NoClearDifference
        );
        assert_eq!(
            classify(MetricId::ProcessCount, (160.0, 0.0, 10), (156.0, 0.0, 10)),
            Classification::Improved
        );
    }
}
