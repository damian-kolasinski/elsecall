//! How long a declared sequential plan takes, from runs whose plans were
//! observed. Pure.
//!
//! A plan of `n` units takes a fixed overhead (building, booting, collecting
//! results) and then time per unit: `seconds = overhead + per_unit × n`. The line
//! is fitted by Theil–Sen: the per-unit time is the median of the slopes between
//! every two runs of different sizes, and the overhead the median of what each
//! run leaves once its units are taken out. A median line ignores the occasional
//! run that took far longer or far shorter, which a least-squares line would
//! follow. Neither part is ever negative: a plan never takes less time for having
//! more units, nor less than nothing.
//!
//! Runs of a single size say nothing about the time per unit, so a fit needs at
//! least two sizes.
use super::median;

/// A fitted line of plan run time against its units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlanFit {
    overhead: f64,
    per_unit: f64,
}

impl PlanFit {
    /// The line through `runs`, each its units and its seconds; `None` when they
    /// do not have two different sizes.
    pub fn of(runs: &[(u32, f64)]) -> Option<Self> {
        let slopes: Vec<f64> = runs
            .iter()
            .enumerate()
            .flat_map(|(i, a)| runs[i + 1..].iter().map(move |b| (a, b)))
            .filter(|(a, b)| a.0 != b.0)
            .map(|(a, b)| (b.1 - a.1) / (f64::from(b.0) - f64::from(a.0)))
            .collect();
        let per_unit = median(slopes)?.max(0.0);
        let overhead = median(
            runs.iter()
                .map(|(units, seconds)| seconds - per_unit * f64::from(*units))
                .collect(),
        )?
        .max(0.0);
        Some(Self { overhead, per_unit })
    }

    /// The expected seconds of a plan of `units`.
    pub fn seconds(&self, units: u32) -> f64 {
        self.overhead + self.per_unit * f64::from(units)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn a_line_through_plans_of_several_sizes_extrapolates_to_a_larger_one() {
        let fit = PlanFit::of(&[(2, 300.0), (5, 600.0), (10, 1100.0)]).unwrap();
        assert!(close(fit.seconds(0), 100.0));
        assert!(close(fit.seconds(39), 4000.0));
    }

    #[test]
    fn one_run_far_off_the_line_does_not_move_it() {
        let runs = [
            (2, 300.0),
            (3, 400.0),
            (5, 600.0),
            (8, 900.0),
            (10, 1100.0),
            (4, 9000.0),
        ];
        let fit = PlanFit::of(&runs).unwrap();
        assert!(close(fit.seconds(20), 2100.0), "{}", fit.seconds(20));
    }

    #[test]
    fn plans_of_one_size_say_nothing_about_the_time_per_unit() {
        assert_eq!(PlanFit::of(&[]), None);
        assert_eq!(PlanFit::of(&[(3, 300.0)]), None);
        assert_eq!(PlanFit::of(&[(3, 300.0), (3, 500.0)]), None);
    }

    #[test]
    fn neither_the_time_per_unit_nor_the_overhead_goes_below_zero() {
        let shrinking = PlanFit::of(&[(2, 900.0), (10, 300.0)]).unwrap();
        assert!(close(shrinking.seconds(2), shrinking.seconds(40)));
        assert!(shrinking.seconds(40) > 0.0);
        let steep = PlanFit::of(&[(2, 0.0), (4, 1000.0)]).unwrap();
        assert!(close(steep.seconds(0), 0.0));
        assert!(close(steep.seconds(6), 3000.0));
    }
}
