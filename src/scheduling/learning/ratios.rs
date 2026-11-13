//! How much longer the same work takes on one member than on another, learned
//! from labels both have run. Pure.
//!
//! A measured speed is a short benchmark, taken under whatever load the member
//! carries, so it drifts and converts real work only roughly. Where two members
//! have both run the same labels, the ratio of their run times says the same
//! thing from the work itself:
//! - A label counts for a pair of members when each has at least `minimum_runs`
//!   warm runs of it (the latest `window`) and the median on each is at least
//!   `short_run`: shorter runs are mostly start-up and say nothing about speed.
//! - The pair's ratio is the median of those labels' ratios, taken in log space
//!   so that the ratio one way is exactly the inverse of the other.
//! - Labels of the request's own family are preferred: when at least
//!   `minimum_ratio_labels` of them count, only they do. Otherwise every label
//!   does, from the same minimum. Below it, there is no learned ratio and
//!   conversion falls back to measured speeds.
use super::RunSample;
use crate::scheduling::parameters::Estimates;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One label's trusted warm median on each member that has one.
#[derive(Clone, Debug, PartialEq)]
struct LabelTimes {
    label: String,
    family: Option<String>,
    medians: BTreeMap<String, f64>,
}

/// Speed ratios between members, learned from their run times.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpeedRatios {
    labels: Vec<LabelTimes>,
    minimum_labels: usize,
}

/// Which labels a learned ratio came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RatioScope {
    /// Labels of the same family.
    Family,
    /// Labels of any family.
    Fleet,
}

/// A run on one member times `ratio` is the expected run on another.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LearnedRatio {
    pub ratio: f64,
    pub labels: usize,
    pub scope: RatioScope,
}

impl SpeedRatios {
    /// Learns from `samples`, any labels' runs in any order.
    pub fn learn(samples: &[RunSample], estimates: &Estimates) -> Self {
        let mut newest: Vec<&RunSample> = samples.iter().filter(|s| !s.cold).collect();
        newest.sort_by_key(|s| std::cmp::Reverse(s.at));
        /// The family of a label's newest run, and each member's newest runs.
        type Runs<'a> = (Option<&'a str>, BTreeMap<&'a str, Vec<f64>>);
        let mut by_label: BTreeMap<&str, Runs> = BTreeMap::new();
        for sample in newest {
            let (_, members) = by_label
                .entry(&sample.label)
                .or_insert_with(|| (sample.family.as_deref(), BTreeMap::new()));
            let runs = members.entry(&sample.member).or_default();
            if runs.len() < estimates.window {
                runs.push(sample.run.as_secs_f64());
            }
        }
        let short = estimates.short_run.as_secs_f64();
        let labels = by_label
            .into_iter()
            .filter_map(|(label, (family, members))| {
                let medians: BTreeMap<String, f64> = members
                    .into_iter()
                    .filter(|(_, runs)| runs.len() >= estimates.minimum_runs)
                    .filter_map(|(member, runs)| Some((member.to_owned(), super::median(runs)?)))
                    .filter(|(_, median)| *median >= short && *median > 0.0)
                    .collect();
                (medians.len() >= 2).then(|| LabelTimes {
                    label: label.to_owned(),
                    family: family.map(str::to_owned),
                    medians,
                })
            })
            .collect();
        Self {
            labels,
            minimum_labels: estimates.minimum_ratio_labels,
        }
    }

    /// The ratio that converts a run on `from` into one on `to`, preferring
    /// labels of `family`, and leaving out the label `except` (a report asks
    /// what the ratio would have been without the label it checks).
    pub fn between(
        &self,
        from: &str,
        to: &str,
        family: Option<&str>,
        except: Option<&str>,
    ) -> Option<LearnedRatio> {
        let shared: Vec<(&LabelTimes, f64)> = self
            .labels
            .iter()
            .filter(|l| Some(l.label.as_str()) != except)
            .filter_map(|l| Some((l, l.medians.get(to)? / l.medians.get(from)?)))
            .collect();
        let of = |scope: RatioScope, ratios: Vec<f64>| {
            (ratios.len() >= self.minimum_labels && !ratios.is_empty()).then(|| LearnedRatio {
                ratio: log_median(&ratios),
                labels: ratios.len(),
                scope,
            })
        };
        let kin: Vec<f64> = shared
            .iter()
            .filter(|(l, _)| family.is_some() && l.family.as_deref() == family)
            .map(|(_, r)| *r)
            .collect();
        of(RatioScope::Family, kin)
            .or_else(|| of(RatioScope::Fleet, shared.iter().map(|(_, r)| *r).collect()))
    }
}

/// The median of ratios in log space: for an even count, the geometric mean of
/// the middle two, so that inverting every ratio inverts the median.
fn log_median(ratios: &[f64]) -> f64 {
    let mut logs: Vec<f64> = ratios.iter().map(|r| r.ln()).collect();
    logs.sort_by(f64::total_cmp);
    let middle = logs.len() / 2;
    let log = if logs.len() % 2 == 1 {
        logs[middle]
    } else {
        (logs[middle - 1] + logs[middle]) / 2.0
    };
    log.exp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::learning::tests::{defaults, labelled};

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// Three runs of `label` on `member`, all taking `run` seconds.
    fn runs(label: &str, family: Option<&str>, member: &str, run: f64) -> Vec<RunSample> {
        (0..3)
            .map(|i| labelled(label, family, member, i, run, false, None))
            .collect()
    }

    #[test]
    fn a_pair_learns_the_median_ratio_of_labels_both_ran_either_way() {
        let samples = [
            runs("a", None, "mini", 10.0),
            runs("a", None, "devbox", 20.0),
            runs("b", None, "mini", 10.0),
            runs("b", None, "devbox", 12.0),
            runs("c", None, "mini", 5.0),
        ]
        .concat();
        let ratios = SpeedRatios::learn(&samples, &defaults());
        let there = ratios.between("mini", "devbox", None, None).unwrap();
        assert_eq!((there.labels, there.scope), (2, RatioScope::Fleet));
        assert!(close(there.ratio, (2.0f64 * 1.2).sqrt()), "{there:?}");
        let back = ratios.between("devbox", "mini", None, None).unwrap();
        assert!(close(back.ratio * there.ratio, 1.0), "inverse either way");
        let without_a = ratios.between("mini", "devbox", None, Some("a")).unwrap();
        assert!(close(without_a.ratio, 1.2));
        assert_eq!(ratios.between("mini", "elsewhere", None, None), None);
    }

    #[test]
    fn the_familys_labels_are_preferred_once_there_are_enough() {
        let samples = [
            runs("test a", Some("test *"), "mini", 10.0),
            runs("test a", Some("test *"), "devbox", 30.0),
            runs("build", Some("build *"), "mini", 10.0),
            runs("build", Some("build *"), "devbox", 10.0),
            runs("lint", None, "mini", 10.0),
            runs("lint", None, "devbox", 10.0),
        ]
        .concat();
        let ratios = SpeedRatios::learn(&samples, &defaults());
        let family = ratios
            .between("mini", "devbox", Some("test *"), None)
            .unwrap();
        assert_eq!((family.scope, family.labels), (RatioScope::Family, 1));
        assert!(close(family.ratio, 3.0));
        let fleet = ratios
            .between("mini", "devbox", Some("deploy *"), None)
            .unwrap();
        assert_eq!((fleet.scope, fleet.labels), (RatioScope::Fleet, 3));
        assert!(close(fleet.ratio, 1.0));
        let stricter = Estimates {
            minimum_ratio_labels: 2,
            ..defaults()
        };
        let strict = SpeedRatios::learn(&samples, &stricter);
        assert_eq!(
            strict
                .between("mini", "devbox", Some("test *"), None)
                .unwrap()
                .scope,
            RatioScope::Fleet,
            "one family label is too few"
        );
        let strictest = Estimates {
            minimum_ratio_labels: 4,
            ..defaults()
        };
        assert_eq!(
            SpeedRatios::learn(&samples, &strictest).between("mini", "devbox", None, None),
            None
        );
    }

    #[test]
    fn only_trusted_warm_runs_of_some_length_count() {
        let mut samples = [
            runs("quick", None, "mini", 0.2),
            runs("quick", None, "devbox", 0.9),
            runs("cold", None, "mini", 10.0),
        ]
        .concat();
        // Two warm runs on the devbox are too few; cold ones never count.
        samples.extend((0..2).map(|i| labelled("few", None, "devbox", i, 10.0, false, None)));
        samples.extend((0..3).map(|i| labelled("few", None, "mini", i, 10.0, false, None)));
        samples.extend((0..3).map(|i| labelled("cold", None, "devbox", i, 50.0, true, None)));
        let ratios = SpeedRatios::learn(&samples, &defaults());
        assert_eq!(ratios.between("mini", "devbox", None, None), None);
        let anything = Estimates {
            short_run: std::time::Duration::ZERO,
            ..defaults()
        };
        let learned = SpeedRatios::learn(&samples, &anything)
            .between("mini", "devbox", None, None)
            .unwrap();
        assert!(
            close(learned.ratio, 4.5),
            "short runs count without the floor"
        );
        // Only the latest `window` runs of a member count.
        let narrow = Estimates {
            window: 3,
            ..defaults()
        };
        let mut drifted = [
            runs("a", None, "mini", 10.0),
            runs("a", None, "devbox", 20.0),
        ]
        .concat();
        drifted.extend((10..13).map(|i| labelled("a", None, "devbox", i, 40.0, false, None)));
        let latest = SpeedRatios::learn(&drifted, &narrow)
            .between("mini", "devbox", None, None)
            .unwrap();
        assert!(close(latest.ratio, 4.0));
    }
}
