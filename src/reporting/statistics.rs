//! Summaries the report's metrics share.
use serde::Serialize;

/// The value at nearest rank `q`, in (0, 1], of `sorted`.
pub fn nearest_rank(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    Some(sorted[rank - 1])
}

pub fn sorted(mut values: Vec<f64>) -> Vec<f64> {
    values.sort_by(f64::total_cmp);
    values
}

pub fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

/// Seconds from milliseconds.
pub fn seconds(ms: u64) -> f64 {
    ms as f64 / 1000.0
}

/// A set of durations: how many, their mean and nearest-rank percentiles.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Distribution {
    pub count: usize,
    pub mean_seconds: Option<f64>,
    pub p50_seconds: Option<f64>,
    pub p90_seconds: Option<f64>,
    pub max_seconds: Option<f64>,
}

impl Distribution {
    pub fn of(values: Vec<f64>) -> Self {
        let values = sorted(values);
        Distribution {
            count: values.len(),
            mean_seconds: mean(&values),
            p50_seconds: nearest_rank(&values, 0.5),
            p90_seconds: nearest_rank(&values, 0.9),
            max_seconds: values.last().copied(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let values = sorted(vec![5.0, 1.0, 4.0, 2.0, 3.0]);
        assert_eq!(nearest_rank(&values, 0.5), Some(3.0));
        assert_eq!(nearest_rank(&values, 0.9), Some(5.0));
        assert_eq!(nearest_rank(&values, 0.2), Some(1.0));
        assert_eq!(nearest_rank(&[], 0.5), None);
        let four = sorted(vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(nearest_rank(&four, 0.5), Some(2.0), "the lower middle");
        assert_eq!(
            Distribution::of(vec![2.0, 1.0, 6.0]),
            Distribution {
                count: 3,
                mean_seconds: Some(3.0),
                p50_seconds: Some(2.0),
                p90_seconds: Some(6.0),
                max_seconds: Some(6.0),
            }
        );
        assert_eq!(Distribution::of(Vec::new()).count, 0);
    }
}
