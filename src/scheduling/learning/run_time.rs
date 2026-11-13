//! How long a label's run takes on a member, from history. Pure.
//!
//! A run is estimated from the first of these that has enough trusted samples
//! (warm and cold runs apart, each the latest `window`):
//! 1. **Label:** the median of the label's own runs on the member, from
//!    `minimum_runs`.
//! 2. **Converted:** the median of the label's runs on every member, each
//!    converted to this member, from `minimum_runs`. A run on another member is
//!    converted by the speed ratio learned from labels both members ran (see
//!    [`super::ratios`]), else by their measured speeds; a run that neither
//!    converts is left out.
//! 3. **Plan:** a request that declares a sequential plan of `n` units, with too
//!    few runs of its label for either (for such a request, only the label's
//!    runs that observed a plan of `n` units count as its own), takes a line of run time against units
//!    (see [`super::plan_fit`]) through the latest `window` runs of its label and
//!    family whose wrappers observed their plans, each converted to this member,
//!    from `minimum_family_runs` of them in at least two sizes. A family's runs
//!    are mostly of other sizes, so their median says little about a plan many
//!    times as long.
//! 4. **Family:** a label with too few runs for either takes its family's: the
//!    runs of the other labels that matched the same `.elsecall.toml` rule, on
//!    this member, from `minimum_family_runs`. Runs shorter than `short_run` are
//!    left out, since a no-op says nothing about the work its family does.
//! 5. **Converted family:** the family's runs on every member, converted.
//!
//! **A label's few runs and its family.** A family is a stand-in for the runs the
//! label lacks, not a replacement for those it has. With `k` of the label's runs
//! (converted to this member) and `minimum_runs` needed, the family's median fills
//! the other `minimum_runs − k` places, and the estimate is the geometric mean of
//! all `minimum_runs`: the first run moves a three-run estimate a third of the
//! way, in proportion, from the family towards the label. Once the label has
//! `minimum_runs` runs anywhere they convert from, its own history takes over
//! completely.
//!
//! Without a family (no rule matched) or enough of its runs, there is no
//! estimate, and placement uses its prior.
use super::{RunSample, median, plan_fit::PlanFit, ratios::SpeedRatios};
use crate::scheduling::parameters::Estimates;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How many runs from other members were converted, and how.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversions {
    /// By a speed ratio learned from run times.
    pub learned: usize,
    /// By measured speeds.
    pub measured: usize,
}

/// Where a run estimate came from.
#[derive(Clone, Debug, PartialEq)]
pub enum Basis {
    /// The label's own runs on the member.
    Label { samples: usize },
    /// The label's runs on every member, converted to this one.
    Converted {
        samples: usize,
        conversions: Conversions,
    },
    /// A line through the observed plans of the label and its family, every
    /// member's converted to this one, at the request's declared `units`.
    Plan {
        samples: usize,
        units: u32,
        conversions: Conversions,
    },
    /// The family's runs on the member, and the label's few, which may have been
    /// converted from other members.
    Family {
        samples: usize,
        label_samples: usize,
        conversions: Conversions,
    },
    /// The family's runs on every member, converted, and the label's few.
    ConvertedFamily {
        samples: usize,
        label_samples: usize,
        conversions: Conversions,
    },
}

impl Basis {
    /// Its place in the order estimates are tried: the label's own runs first.
    fn rank(&self) -> u8 {
        match self {
            Basis::Label { .. } => 0,
            Basis::Converted { .. } => 1,
            Basis::Plan { .. } => 2,
            Basis::Family { .. } => 3,
            Basis::ConvertedFamily { .. } => 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunEstimate {
    pub run: Duration,
    pub basis: Basis,
}

/// A candidate's run estimates: warm, and cold when its copy is cold.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunEstimates {
    pub warm: Option<RunEstimate>,
    /// Only when it comes from history at least as close to the label as the
    /// warm estimate's; otherwise placement takes the warm one times its cold
    /// factor. A family's few cold runs never override the label's own warm
    /// history.
    pub cold: Option<RunEstimate>,
}

/// What history says about one label's runs: its own, its family's, and the
/// speed ratios between members.
#[derive(Clone, Debug)]
pub struct RunHistory {
    family: Option<String>,
    /// The label's runs, newest first.
    own: Vec<RunSample>,
    /// The family's runs of other labels, newest first, none shorter than
    /// `short_run`.
    kin: Vec<RunSample>,
    /// The units of the request's declared sequential plan.
    plan: Option<u32>,
    /// The label's and family's runs that observed their plans, newest first.
    plans: Vec<RunSample>,
    ratios: SpeedRatios,
    estimates: Estimates,
}

impl Default for RunHistory {
    fn default() -> Self {
        Self {
            family: None,
            own: Vec::new(),
            kin: Vec::new(),
            plan: None,
            plans: Vec::new(),
            ratios: SpeedRatios::default(),
            estimates: crate::scheduling::parameters::Parameters::default().estimates,
        }
    }
}

/// One run as it would take on the member being estimated.
struct Converted {
    seconds: f64,
    how: Option<How>,
}
#[derive(Clone, Copy)]
enum How {
    Learned,
    Measured,
}

fn conversions(runs: &[Converted]) -> Conversions {
    Conversions {
        learned: runs
            .iter()
            .filter(|r| matches!(r.how, Some(How::Learned)))
            .count(),
        measured: runs
            .iter()
            .filter(|r| matches!(r.how, Some(How::Measured)))
            .count(),
    }
}

/// The shortest time a geometric mean takes, so a run recorded as 0 ms counts.
const SHORTEST: f64 = 0.001;

impl RunHistory {
    /// The history of `label`, in `family` when a rule matched it, from its own
    /// `runs`, `family_runs` (any runs of the family; the label's own and short
    /// ones are left out here) and the speed `ratios` learned across labels.
    pub fn new(
        label: &str,
        family: Option<&str>,
        mut runs: Vec<RunSample>,
        family_runs: Vec<RunSample>,
        ratios: SpeedRatios,
        estimates: &Estimates,
    ) -> Self {
        runs.retain(|s| s.label == label);
        runs.sort_by_key(|s| std::cmp::Reverse(s.at));
        let mut kin: Vec<RunSample> = match family {
            Some(family) => family_runs
                .into_iter()
                .filter(|s| s.family.as_deref() == Some(family) && s.label != label)
                .filter(|s| s.run >= estimates.short_run)
                .collect(),
            None => Vec::new(),
        };
        kin.sort_by_key(|s| std::cmp::Reverse(s.at));
        let mut plans: Vec<RunSample> = runs
            .iter()
            .chain(&kin)
            .filter(|s| s.units.is_some())
            .cloned()
            .collect();
        plans.sort_by_key(|s| std::cmp::Reverse(s.at));
        Self {
            family: family.map(str::to_owned),
            own: runs,
            kin,
            plan: None,
            plans,
            ratios,
            estimates: estimates.clone(),
        }
    }

    /// The same history for a request that declares a sequential plan of
    /// `units`; `None` for opaque work. The label's own runs then count only
    /// when they observed a plan of the same size: a label reused for plans of
    /// other sizes says nothing about this one. Every observed plan still
    /// counts towards the line.
    pub fn with_plan(mut self, units: Option<u32>) -> Self {
        if let Some(units) = units {
            self.own.retain(|s| s.units == Some(units));
        }
        self.plan = units;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.own.is_empty() && self.kin.is_empty()
    }

    /// `sample` as a run on `member`, when it converts.
    fn convert(
        &self,
        sample: &RunSample,
        member: &str,
        speed_of: &dyn Fn(&str) -> Option<f64>,
    ) -> Option<Converted> {
        let seconds = sample.run.as_secs_f64();
        if sample.member == member {
            return Some(Converted { seconds, how: None });
        }
        if let Some(learned) =
            self.ratios
                .between(&sample.member, member, self.family.as_deref(), None)
        {
            return Some(Converted {
                seconds: seconds * learned.ratio,
                how: Some(How::Learned),
            });
        }
        let ratio = speed_of(&sample.member)? / speed_of(member)?;
        Some(Converted {
            seconds: seconds * ratio,
            how: Some(How::Measured),
        })
    }

    /// The latest `window` of `samples` of this warmth that convert to `member`.
    fn pool(
        &self,
        samples: &[RunSample],
        member: &str,
        cold: bool,
        speed_of: &dyn Fn(&str) -> Option<f64>,
    ) -> Vec<Converted> {
        samples
            .iter()
            .filter(|s| s.cold == cold)
            .filter_map(|s| self.convert(s, member, speed_of))
            .take(self.estimates.window)
            .collect()
    }

    /// The label's `few` runs, each standing for itself, and `family` standing
    /// for the rest of `minimum_runs`, as a geometric mean.
    fn stand_in(&self, few: &[Converted], family: f64) -> f64 {
        let places = self.estimates.minimum_runs.max(few.len());
        let logs: f64 = few
            .iter()
            .map(|r| r.seconds.max(SHORTEST).ln())
            .sum::<f64>()
            + (places - few.len()) as f64 * family.max(SHORTEST).ln();
        (logs / places as f64).exp()
    }

    /// The expected run time on `member` into a warm (`cold` false) or cold copy.
    /// `speed_of` gives each member's measured speed, when known.
    pub fn run_on(
        &self,
        member: &str,
        cold: bool,
        speed_of: &dyn Fn(&str) -> Option<f64>,
    ) -> Option<RunEstimate> {
        let trust = &self.estimates;
        let estimate = |seconds: f64, basis: Basis| {
            Some(RunEstimate {
                run: Duration::try_from_secs_f64(seconds).ok()?,
                basis,
            })
        };
        let here: Vec<f64> = self
            .own
            .iter()
            .filter(|s| s.member == member && s.cold == cold)
            .take(trust.window)
            .map(|s| s.run.as_secs_f64())
            .collect();
        if here.len() >= trust.minimum_runs {
            let samples = here.len();
            return estimate(median(here)?, Basis::Label { samples });
        }
        let own = self.pool(&self.own, member, cold, speed_of);
        if own.len() >= trust.minimum_runs {
            return estimate(
                median(own.iter().map(|r| r.seconds).collect())?,
                Basis::Converted {
                    samples: own.len(),
                    conversions: conversions(&own),
                },
            );
        }
        if let Some(units) = self.plan
            && let Some(estimate) = self.plan_on(units, member, cold, speed_of)
        {
            return Some(estimate);
        }
        let kin_here: Vec<f64> = self
            .kin
            .iter()
            .filter(|s| s.member == member && s.cold == cold)
            .take(trust.window)
            .map(|s| s.run.as_secs_f64())
            .collect();
        if kin_here.len() >= trust.minimum_family_runs {
            let samples = kin_here.len();
            return estimate(
                self.stand_in(&own, median(kin_here)?),
                Basis::Family {
                    samples,
                    label_samples: own.len(),
                    conversions: conversions(&own),
                },
            );
        }
        let kin = self.pool(&self.kin, member, cold, speed_of);
        if kin.len() >= trust.minimum_family_runs {
            let mut all = conversions(&kin);
            let few = conversions(&own);
            all.learned += few.learned;
            all.measured += few.measured;
            return estimate(
                self.stand_in(&own, median(kin.iter().map(|r| r.seconds).collect())?),
                Basis::ConvertedFamily {
                    samples: kin.len(),
                    label_samples: own.len(),
                    conversions: all,
                },
            );
        }
        None
    }

    /// A plan of `units` on `member`, from the line through observed plans.
    fn plan_on(
        &self,
        units: u32,
        member: &str,
        cold: bool,
        speed_of: &dyn Fn(&str) -> Option<f64>,
    ) -> Option<RunEstimate> {
        let (sizes, runs): (Vec<u32>, Vec<Converted>) = self
            .plans
            .iter()
            .filter(|s| s.cold == cold)
            .filter_map(|s| Some((s.units?, self.convert(s, member, speed_of)?)))
            .take(self.estimates.window)
            .unzip();
        if runs.len() < self.estimates.minimum_family_runs {
            return None;
        }
        let points: Vec<(u32, f64)> = sizes
            .iter()
            .zip(&runs)
            .map(|(units, run)| (*units, run.seconds))
            .collect();
        let fit = PlanFit::of(&points)?;
        Some(RunEstimate {
            run: Duration::try_from_secs_f64(fit.seconds(units)).ok()?,
            basis: Basis::Plan {
                samples: runs.len(),
                units,
                conversions: conversions(&runs),
            },
        })
    }

    /// The warm estimate on `member`, and the cold one when `cold` (the copy is
    /// cold), each as [`Self::run_on`] makes it, except that a cold estimate
    /// from further from the label than the warm one is left out.
    pub fn estimates_on(
        &self,
        member: &str,
        cold: bool,
        speed_of: &dyn Fn(&str) -> Option<f64>,
    ) -> RunEstimates {
        let warm = self.run_on(member, false, speed_of);
        let cold = cold
            .then(|| self.run_on(member, true, speed_of))
            .flatten()
            .filter(|c| {
                warm.as_ref()
                    .is_none_or(|w| c.basis.rank() <= w.basis.rank())
            });
        RunEstimates { warm, cold }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::learning::tests::{defaults, labelled, sample};

    fn speeds(member: &str) -> Option<f64> {
        match member {
            "fast" => Some(2.0),
            "slow" => Some(1.0),
            _ => None,
        }
    }
    fn close(a: Duration, b: f64) -> bool {
        (a.as_secs_f64() - b).abs() < 1e-6
    }
    fn label_only(samples: Vec<RunSample>, estimates: &Estimates) -> RunHistory {
        RunHistory::new(
            "build",
            None,
            samples,
            Vec::new(),
            SpeedRatios::default(),
            estimates,
        )
    }

    #[test]
    fn a_members_own_runs_win_once_trusted() {
        let mut samples = vec![
            sample("slow", 5, 10.0, None, false),
            sample("slow", 4, 12.0, None, false),
        ];
        let history = label_only(samples.clone(), &defaults());
        assert_eq!(
            history.run_on("slow", false, &speeds),
            None,
            "two are too few"
        );
        samples.push(sample("slow", 3, 11.0, None, false));
        samples.push(sample("slow", 2, 99.0, None, true));
        let history = label_only(samples.clone(), &defaults());
        assert_eq!(
            history.run_on("slow", false, &speeds),
            Some(RunEstimate {
                run: Duration::from_secs(11),
                basis: Basis::Label { samples: 3 }
            })
        );
        assert_eq!(
            history.run_on("slow", true, &speeds),
            None,
            "cold runs are learned apart"
        );
        let stricter = Estimates {
            minimum_runs: 4,
            ..defaults()
        };
        assert_eq!(
            label_only(samples.clone(), &stricter).run_on("slow", false, &speeds),
            None,
            "four needed"
        );
        let narrow = Estimates {
            window: 1,
            minimum_runs: 1,
            minimum_family_runs: 1,
            ..defaults()
        };
        assert_eq!(
            label_only(samples, &narrow)
                .run_on("slow", false, &speeds)
                .unwrap()
                .run,
            Duration::from_secs(10),
            "only the latest run counts"
        );
    }

    #[test]
    fn other_members_runs_convert_by_measured_speed_without_learned_ratios() {
        let history = label_only(
            vec![
                sample("slow", 3, 10.0, None, false),
                sample("slow", 2, 12.0, None, false),
                sample("slow", 1, 11.0, None, false),
                sample("unmeasured", 0, 1.0, None, false),
            ],
            &defaults(),
        );
        let fast = history.run_on("fast", false, &speeds).unwrap();
        assert_eq!(fast.run, Duration::from_secs_f64(5.5));
        assert_eq!(
            fast.basis,
            Basis::Converted {
                samples: 3,
                conversions: Conversions {
                    learned: 0,
                    measured: 3
                }
            }
        );
        assert_eq!(
            history.run_on("unmeasured", false, &speeds),
            None,
            "an unmeasured member cannot convert work to time"
        );
    }

    #[test]
    fn learned_ratios_convert_before_measured_speeds() {
        // "lint" ran on both: three times as long on "fast" as on "slow", whatever
        // their measured speeds say.
        let lint: Vec<RunSample> = (0..3)
            .flat_map(|i| {
                [
                    labelled("lint", None, "slow", i, 10.0, false, None),
                    labelled("lint", None, "fast", i, 30.0, false, None),
                ]
            })
            .collect();
        let build = vec![
            sample("slow", 3, 10.0, None, false),
            sample("slow", 2, 12.0, None, false),
            sample("slow", 1, 11.0, None, false),
        ];
        let ratios = SpeedRatios::learn(&[lint, build.clone()].concat(), &defaults());
        let history = RunHistory::new("build", None, build, Vec::new(), ratios, &defaults());
        let fast = history.run_on("fast", false, &speeds).unwrap();
        assert!(close(fast.run, 33.0), "{fast:?}");
        assert_eq!(
            fast.basis,
            Basis::Converted {
                samples: 3,
                conversions: Conversions {
                    learned: 3,
                    measured: 0
                }
            }
        );
        // "unmeasured" has no measured speed, but no learned ratio either.
        assert_eq!(history.run_on("unmeasured", false, &speeds), None);
    }

    /// "test *" on "slow": "test a" took 10 s three times, "test b" 40 s twice,
    /// and a no-op "test c" 0.1 s five times.
    fn family() -> Vec<RunSample> {
        let f = Some("test *");
        let mut runs = Vec::new();
        for i in 0..3 {
            runs.push(labelled("test a", f, "slow", 10 + i, 10.0, false, None));
        }
        for i in 0..2 {
            runs.push(labelled("test b", f, "slow", 20 + i, 40.0, false, None));
        }
        for i in 0..5 {
            runs.push(labelled("test c", f, "slow", 30 + i, 0.1, false, None));
        }
        runs.push(labelled(
            "build",
            Some("build *"),
            "slow",
            40,
            500.0,
            false,
            None,
        ));
        runs
    }

    fn of_family(label: &str, own: Vec<RunSample>) -> RunHistory {
        RunHistory::new(
            label,
            Some("test *"),
            own,
            family(),
            SpeedRatios::default(),
            &defaults(),
        )
    }

    #[test]
    fn a_label_without_history_takes_its_familys_without_short_runs() {
        let history = of_family("test new", Vec::new());
        // 10, 10, 10, 40, 40 (the no-op runs and other families left out).
        assert_eq!(
            history.run_on("slow", false, &speeds),
            Some(RunEstimate {
                run: Duration::from_secs(10),
                basis: Basis::Family {
                    samples: 5,
                    label_samples: 0,
                    conversions: Conversions::default()
                }
            })
        );
        let no_family = RunHistory::new(
            "test new",
            None,
            Vec::new(),
            family(),
            SpeedRatios::default(),
            &defaults(),
        );
        assert_eq!(
            no_family.run_on("slow", false, &speeds),
            None,
            "a request no rule matched has no family"
        );
        // The family's own runs of the label are the label's, not the family's.
        let test_b = of_family("test b", Vec::new());
        assert_eq!(
            test_b.run_on("slow", false, &speeds).unwrap().basis,
            Basis::Family {
                samples: 3,
                label_samples: 0,
                conversions: Conversions::default()
            }
        );
        let stricter = Estimates {
            minimum_family_runs: 6,
            ..defaults()
        };
        assert_eq!(
            RunHistory::new(
                "test new",
                Some("test *"),
                Vec::new(),
                family(),
                SpeedRatios::default(),
                &stricter
            )
            .run_on("slow", false, &speeds),
            None
        );
    }

    #[test]
    fn a_labels_few_runs_move_the_family_estimate_until_they_take_over() {
        let own = |runs: &[f64]| -> Vec<RunSample> {
            runs.iter()
                .enumerate()
                .map(|(i, r)| {
                    labelled(
                        "test new",
                        Some("test *"),
                        "slow",
                        100 + i as u64,
                        *r,
                        false,
                        None,
                    )
                })
                .collect()
        };
        // One run of 80 s and the family's 10 s for the other two places.
        let one = of_family("test new", own(&[80.0]))
            .run_on("slow", false, &speeds)
            .unwrap();
        assert!(close(one.run, (80.0f64 * 10.0 * 10.0).cbrt()), "{one:?}");
        assert_eq!(
            one.basis,
            Basis::Family {
                samples: 5,
                label_samples: 1,
                conversions: Conversions::default()
            }
        );
        let two = of_family("test new", own(&[80.0, 80.0]))
            .run_on("slow", false, &speeds)
            .unwrap();
        assert!(close(two.run, (80.0f64 * 80.0 * 10.0).cbrt()));
        let three = of_family("test new", own(&[80.0, 80.0, 0.05]))
            .run_on("slow", false, &speeds)
            .unwrap();
        assert_eq!(
            three,
            RunEstimate {
                run: Duration::from_secs(80),
                basis: Basis::Label { samples: 3 }
            },
            "its own history takes over, short runs and all"
        );
    }

    #[test]
    fn a_labels_runs_elsewhere_convert_before_its_family_is_asked() {
        let elsewhere: Vec<RunSample> = (0..3)
            .map(|i| labelled("test new", Some("test *"), "fast", i, 4.0, false, None))
            .collect();
        let history = of_family("test new", elsewhere.clone());
        let slow = history.run_on("slow", false, &speeds).unwrap();
        assert!(close(slow.run, 8.0));
        assert!(matches!(slow.basis, Basis::Converted { samples: 3, .. }));
        // Two runs elsewhere are too few: the family on "slow" fills the third place.
        let history = of_family("test new", elsewhere[..2].to_vec());
        let slow = history.run_on("slow", false, &speeds).unwrap();
        assert!(close(slow.run, (8.0f64 * 8.0 * 10.0).cbrt()), "{slow:?}");
        assert_eq!(
            slow.basis,
            Basis::Family {
                samples: 5,
                label_samples: 2,
                conversions: Conversions {
                    learned: 0,
                    measured: 2
                }
            }
        );
    }

    #[test]
    fn a_familys_cold_runs_never_override_the_labels_warm_history() {
        let f = Some("test *");
        // "test heavy" took 300 s warm three times; "test lite" 5 s cold.
        let mut runs: Vec<RunSample> = (0..3)
            .map(|i| labelled("test heavy", f, "slow", i, 300.0, false, None))
            .collect();
        let lite: Vec<RunSample> = (0..3)
            .map(|i| labelled("test lite", f, "slow", 10 + i, 5.0, true, None))
            .collect();
        let history = |runs: Vec<RunSample>| {
            RunHistory::new(
                "test heavy",
                f,
                runs,
                lite.clone(),
                SpeedRatios::default(),
                &defaults(),
            )
        };
        let estimates = history(runs.clone()).estimates_on("slow", true, &speeds);
        assert_eq!(estimates.warm.unwrap().basis, Basis::Label { samples: 3 });
        assert_eq!(
            estimates.cold, None,
            "placement takes the warm 300 s times its cold factor"
        );
        // The label's own cold runs are as close as its warm ones.
        runs.extend((0..3).map(|i| labelled("test heavy", f, "slow", 20 + i, 700.0, true, None)));
        let estimates = history(runs).estimates_on("slow", true, &speeds);
        assert_eq!(estimates.cold.unwrap().run, Duration::from_secs(700));
        // Without warm history of its own, the family's cold runs stand in.
        let estimates = history(Vec::new()).estimates_on("slow", true, &speeds);
        assert_eq!(estimates.warm, None);
        assert!(matches!(
            estimates.cold.unwrap().basis,
            Basis::Family { samples: 3, .. }
        ));
        assert_eq!(
            history(Vec::new())
                .estimates_on("slow", false, &speeds)
                .cold,
            None,
            "a warm copy needs no cold estimate"
        );
    }

    #[test]
    fn a_family_only_elsewhere_converts() {
        let history = of_family("test new", Vec::new());
        let fast = history.run_on("fast", false, &speeds).unwrap();
        // The family's 10 s on "slow" is 5 s on the twice-as-fast member.
        assert!(close(fast.run, 5.0), "{fast:?}");
        assert_eq!(
            fast.basis,
            Basis::ConvertedFamily {
                samples: 5,
                label_samples: 0,
                conversions: Conversions {
                    learned: 0,
                    measured: 5
                }
            }
        );
        assert_eq!(history.run_on("unmeasured", false, &speeds), None);
        assert_eq!(
            history.run_on("slow", true, &speeds),
            None,
            "cold runs of the family are learned apart too"
        );
    }

    /// A run of `label` in the family that observed a plan of `units`.
    fn observed(label: &str, member: &str, at: u64, units: u32, run: f64) -> RunSample {
        RunSample {
            units: Some(units),
            ..labelled(label, Some("test *"), member, at, run, false, None)
        }
    }

    /// The family, with plans of 2, 5 and 10 units observed on "slow" taking
    /// 100 s plus 50 s a unit.
    fn with_plans() -> Vec<RunSample> {
        let mut runs = family();
        for (at, units) in [(50, 2), (51, 5), (52, 10)] {
            let seconds = 100.0 + 50.0 * f64::from(units);
            runs.push(observed(
                &format!("test {units}"),
                "slow",
                at,
                units,
                seconds,
            ));
        }
        runs
    }

    fn planned(label: &str, own: Vec<RunSample>, units: Option<u32>) -> RunHistory {
        RunHistory::new(
            label,
            Some("test *"),
            own,
            with_plans(),
            SpeedRatios::default(),
            &defaults(),
        )
        .with_plan(units)
    }

    #[test]
    fn a_declared_plan_takes_the_line_through_its_familys_observed_plans() {
        let estimate = planned("test 40", Vec::new(), Some(40))
            .run_on("slow", false, &speeds)
            .unwrap();
        assert!(close(estimate.run, 2100.0), "{estimate:?}");
        assert_eq!(
            estimate.basis,
            Basis::Plan {
                samples: 3,
                units: 40,
                conversions: Conversions::default()
            }
        );
        // On the twice-as-fast member, each observed plan converts first.
        let fast = planned("test 40", Vec::new(), Some(40))
            .run_on("fast", false, &speeds)
            .unwrap();
        assert!(close(fast.run, 1050.0), "{fast:?}");
        assert_eq!(
            fast.basis,
            Basis::Plan {
                samples: 3,
                units: 40,
                conversions: Conversions {
                    learned: 0,
                    measured: 3
                }
            }
        );
    }

    #[test]
    fn opaque_work_and_too_few_or_uniform_plans_keep_the_family() {
        let family_level = |history: RunHistory| {
            matches!(
                history.run_on("slow", false, &speeds).unwrap().basis,
                Basis::Family { .. }
            )
        };
        assert!(family_level(planned("test new", Vec::new(), None)));
        let two_plans = RunHistory::new(
            "test 40",
            Some("test *"),
            Vec::new(),
            with_plans()
                .into_iter()
                .filter(|s| s.at.millis() != 52)
                .collect(),
            SpeedRatios::default(),
            &defaults(),
        )
        .with_plan(Some(40));
        assert!(family_level(two_plans), "three observed plans are needed");
        let mut uniform = family();
        for at in 50..53 {
            uniform.push(observed("test 5", "slow", at, 5, 350.0));
        }
        let uniform = RunHistory::new(
            "test 40",
            Some("test *"),
            Vec::new(),
            uniform,
            SpeedRatios::default(),
            &defaults(),
        )
        .with_plan(Some(40));
        assert!(family_level(uniform), "one size says nothing per unit");
    }

    #[test]
    fn a_labels_own_trusted_runs_still_win_over_a_plan() {
        let own: Vec<RunSample> = (60..63)
            .map(|at| observed("test 40", "slow", at, 40, 900.0))
            .collect();
        let estimate = planned("test 40", own, Some(40))
            .run_on("slow", false, &speeds)
            .unwrap();
        assert_eq!(
            estimate,
            RunEstimate {
                run: Duration::from_secs(900),
                basis: Basis::Label { samples: 3 }
            }
        );
    }

    #[test]
    fn a_labels_runs_of_other_plan_sizes_do_not_stand_for_its_own() {
        let own: Vec<RunSample> = (60..63)
            .map(|at| observed("test", "slow", at, 1, 150.0))
            .collect();
        let estimate = planned("test", own.clone(), Some(40))
            .run_on("slow", false, &speeds)
            .unwrap();
        assert!(
            matches!(estimate.basis, Basis::Plan { samples: 6, .. }),
            "{estimate:?}"
        );
        let same = planned("test", own, Some(1))
            .run_on("slow", false, &speeds)
            .unwrap();
        assert_eq!(same.basis, Basis::Label { samples: 3 });
    }

    #[test]
    fn cold_plans_are_learned_apart_from_warm_ones() {
        assert_eq!(
            planned("test 40", Vec::new(), Some(40)).run_on("slow", true, &speeds),
            None
        );
    }
}
