//! Equal-weight distance outside the five busy-hour target bands.
use super::Shape;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize)]
pub(super) struct Deviation {
    lower: f64,
    upper: f64,
    observed: f64,
    /// Signed distance to the nearest band edge; zero inside the inclusive band.
    deviation: f64,
    normalised_distance: f64,
}

impl Deviation {
    fn new(target: f64, tolerance: f64, observed: f64) -> Self {
        let lower = target - tolerance;
        let upper = target + tolerance;
        let deviation = observed - observed.clamp(lower, upper);
        Self {
            lower,
            upper,
            observed,
            deviation,
            normalised_distance: deviation.abs() / (upper - lower),
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct Fit {
    pub distance: f64,
    pub deviations: BTreeMap<&'static str, Deviation>,
}

impl Fit {
    pub fn between(target: &Shape, observed: &Shape) -> Self {
        let deviations = BTreeMap::from([
            (
                "admitted_capacity_share",
                Deviation::new(
                    target.admitted_capacity_share,
                    0.10,
                    observed.admitted_capacity_share,
                ),
            ),
            (
                "queue_p50",
                Deviation::new(target.queue_p50, 1.0, observed.queue_p50),
            ),
            (
                "queue_p90",
                Deviation::new(target.queue_p90, 1.0, observed.queue_p90),
            ),
            (
                "queue_max",
                Deviation::new(target.queue_max, 1.0, observed.queue_max),
            ),
            (
                "wait_p90_ms",
                Deviation::new(
                    target.wait_p90_ms,
                    (target.wait_p90_ms * 0.25).max(10_000.0),
                    observed.wait_p90_ms,
                ),
            ),
        ]);
        let distance = deviations
            .values()
            .map(|d| d.normalised_distance)
            .sum::<f64>()
            / deviations.len() as f64;
        Self {
            distance,
            deviations,
        }
    }

    pub fn matched(&self) -> bool {
        self.distance == 0.0
    }
}

/// Zero-distance matches win; otherwise minimise mean normalised deviation.
/// Input order cannot affect ties: choose the smaller observed factor.
pub(super) fn prefer(candidate: (f64, &Fit), current: (f64, &Fit)) -> bool {
    candidate
        .1
        .distance
        .total_cmp(&current.1.distance)
        .then_with(|| candidate.0.total_cmp(&current.0))
        .is_lt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deviations_use_full_band_width_and_inclusive_edges() {
        for (observed, signed, normalised) in [
            (0.0, -1.0, 0.5),
            (1.0, 0.0, 0.0),
            (2.0, 0.0, 0.0),
            (3.0, 0.0, 0.0),
            (5.0, 2.0, 1.0),
        ] {
            let d = Deviation::new(2.0, 1.0, observed);
            assert_eq!(d.deviation, signed);
            assert_eq!(d.normalised_distance, normalised);
        }
    }

    #[test]
    fn run7_arms_select_two_without_rounding_away_its_misses() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/run7-calibration-shapes.json"
        ))
        .unwrap();
        let target: Shape = serde_json::from_value(fixture["target"].clone()).unwrap();
        let mut chosen: Option<(f64, Fit)> = None;
        // Reverse order to prove selection and tie-breaking do not depend on input order.
        for trial in fixture["trials"].as_array().unwrap().iter().rev() {
            let shape: Shape = serde_json::from_value(trial["shape"].clone()).unwrap();
            let factor = trial["load_factor"].as_f64().unwrap();
            let fit = Fit::between(&target, &shape);
            assert!(!fit.matched());
            if chosen
                .as_ref()
                .is_none_or(|(f, current)| prefer((factor, &fit), (*f, current)))
            {
                chosen = Some((factor, fit));
            }
        }
        let (factor, fit) = chosen.unwrap();
        assert_eq!(factor, 2.0);
        assert!((fit.distance - 0.200_325_893_995_119_4).abs() < 1e-12);
        assert_eq!(fit.deviations["queue_p50"].deviation, 2.0);
        assert!(
            (fit.deviations["admitted_capacity_share"].deviation - 0.0003258939951193973).abs()
                < 1e-12
        );
        for metric in ["queue_p90", "queue_max", "wait_p90_ms"] {
            assert_eq!(fit.deviations[metric].deviation, 0.0);
        }
        let matched = Fit::between(&target, &target);
        assert!(matched.matched());
        assert!(prefer((2.25, &matched), (factor, &fit)));
        assert!(prefer((1.25, &matched), (2.0, &matched)));
        assert!(!prefer((2.0, &fit), (1.25, &fit)));
    }
}
