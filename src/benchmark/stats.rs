//! Order statistics.
//!
//! MinWin summarises with the median and the interquartile range rather than
//! the mean and the standard deviation. Idle-system samples are routinely
//! disturbed by one short burst of background work; a mean absorbs that burst
//! into the headline number, while a median does not.
//!
//! Quartiles use linear interpolation between order statistics — the method
//! R calls `type 7` and NumPy uses by default. It is stated here because
//! "the IQR" is ambiguous without naming the estimator.

/// Sorts a copy of the values with NaNs removed.
fn sorted_finite(values: &[f64]) -> Vec<f64> {
    let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sorted
}

/// The p-th quantile (`p` in `0.0..=1.0`) by linear interpolation.
pub fn quantile(values: &[f64], p: f64) -> Option<f64> {
    let sorted = sorted_finite(values);
    quantile_of_sorted(&sorted, p)
}

fn quantile_of_sorted(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    if sorted.len() == 1 {
        return Some(sorted[0]);
    }
    let p = p.clamp(0.0, 1.0);
    let position = p * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        return Some(sorted[lower]);
    }
    let weight = position - lower as f64;
    Some(sorted[lower] * (1.0 - weight) + sorted[upper] * weight)
}

pub fn median(values: &[f64]) -> Option<f64> {
    quantile(values, 0.5)
}

pub fn minimum(values: &[f64]) -> Option<f64> {
    sorted_finite(values).first().copied()
}

pub fn maximum(values: &[f64]) -> Option<f64> {
    sorted_finite(values).last().copied()
}

/// The interquartile range: `p75 - p25`. MinWin uses this as its measure of
/// run-to-run spread, and therefore as the yardstick for whether a difference
/// between two runs is worth mentioning at all.
pub fn interquartile_range(values: &[f64]) -> Option<f64> {
    let sorted = sorted_finite(values);
    let lower = quantile_of_sorted(&sorted, 0.25)?;
    let upper = quantile_of_sorted(&sorted, 0.75)?;
    Some(upper - lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOLERANCE: f64 = 1e-9;

    fn close(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("expected a value");
        assert!(
            (actual - expected).abs() < TOLERANCE,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn an_empty_sample_set_has_no_statistics() {
        assert_eq!(median(&[]), None);
        assert_eq!(interquartile_range(&[]), None);
        assert_eq!(minimum(&[]), None);
        assert_eq!(maximum(&[]), None);
    }

    #[test]
    fn a_single_sample_is_its_own_median_with_zero_spread() {
        close(median(&[4.0]), 4.0);
        close(interquartile_range(&[4.0]), 0.0);
        close(minimum(&[4.0]), 4.0);
        close(maximum(&[4.0]), 4.0);
    }

    #[test]
    fn an_odd_count_takes_the_middle_value() {
        close(median(&[3.0, 1.0, 2.0]), 2.0);
    }

    #[test]
    fn an_even_count_interpolates_the_two_middle_values() {
        close(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
    }

    #[test]
    fn the_median_ignores_a_single_extreme_burst() {
        // One sample 50x the others must not move the headline number much;
        // this is the whole reason MinWin reports medians.
        let quiet = [2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0];
        let with_burst = [2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 100.0];
        close(median(&quiet), 2.0);
        close(median(&with_burst), 2.0);
    }

    #[test]
    fn quartiles_match_the_type_7_definition() {
        // NumPy: percentile([1..10], 25) == 3.25, 75 == 7.75, IQR == 4.5
        let values: Vec<f64> = (1..=10).map(f64::from).collect();
        close(quantile(&values, 0.25), 3.25);
        close(quantile(&values, 0.75), 7.75);
        close(interquartile_range(&values), 4.5);
    }

    #[test]
    fn quartiles_of_a_constant_series_have_zero_range() {
        let values = [7.0; 10];
        close(interquartile_range(&values), 0.0);
    }

    #[test]
    fn quantile_bounds_are_the_extremes() {
        let values = [5.0, 1.0, 9.0, 3.0];
        close(quantile(&values, 0.0), 1.0);
        close(quantile(&values, 1.0), 9.0);
    }

    #[test]
    fn non_finite_samples_are_discarded_rather_than_poisoning_the_summary() {
        let values = [1.0, f64::NAN, 2.0, f64::INFINITY, 3.0];
        close(median(&values), 2.0);
        close(maximum(&values), 3.0);
    }

    #[test]
    fn the_input_slice_is_not_reordered() {
        let values = [3.0, 1.0, 2.0];
        let _ = median(&values);
        assert_eq!(values, [3.0, 1.0, 2.0]);
    }
}
