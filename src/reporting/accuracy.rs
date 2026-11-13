//! How accurate scheduling's estimates were: what was predicted for the member a
//! request ran on against what happened there, how run times compared between
//! members against how placement converts them, and how today's run estimates
//! would have done (`backtest`).
use crate::{
    reporting::{
        backtest::{self, Backtest},
        facts::{Estimator, Fact},
        statistics::{mean, nearest_rank, seconds, sorted},
    },
    scheduling::{
        learning::{
            RunSample,
            ratios::{RatioScope, SpeedRatios},
        },
        parameters::Parameters,
        placed::Warmth,
        placement::Level,
        request::Kind,
    },
};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Accuracy {
    pub wait: Groups,
    pub sync: Groups,
    pub run: Groups,
    pub speed_conversion: Vec<SpeedConversion>,
    pub backtest: Backtest,
}

/// What accuracy is judged against beyond the requests it covers.
pub struct Context<'a> {
    /// Every request gathered, whatever the filters: what history learns from.
    pub history: &'a [Fact],
    /// The master, whose records placement learns from.
    pub master: Option<&'a str>,
    /// Members' mean measured speeds over the period.
    pub speeds: &'a BTreeMap<String, f64>,
    /// The reporting member's parameters.
    pub parameters: &'a Parameters,
}

/// One estimate's agreement, per member over every label, and per label and
/// member. Sync groups are also split by the copy's warmth, and placement's run
/// estimates by the level of history they came from.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Groups {
    pub by_member: Vec<Agreement>,
    /// Placement's run estimates per member and level.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub by_level: Vec<Agreement>,
    /// The [`MAXIMUM_LABEL_GROUPS`] groups with the most samples, in key order.
    pub by_label: Vec<Agreement>,
    /// Label groups left out past that, with fewer samples.
    pub by_label_omitted: usize,
}

/// The most label groups an estimate lists, so a fleet with many labels still
/// gets a report of bounded size.
pub const MAXIMUM_LABEL_GROUPS: usize = 200;

/// How predictions compared with what happened, for one group.
#[derive(Debug, Serialize, PartialEq)]
pub struct Agreement {
    pub member: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warmth: Option<Warmth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    pub estimator: Estimator,
    pub samples: usize,
    /// Of them, predictions that were the prior rather than history.
    pub priors: usize,
    pub predicted_median_seconds: f64,
    pub actual_median_seconds: f64,
    /// Median of actual ÷ predicted, over predictions above zero: above 1,
    /// estimates were too low.
    pub ratio_median: Option<f64>,
    /// Mean of actual − predicted: above 0, estimates were too low.
    pub bias_seconds: f64,
    /// Mean of |actual − predicted|.
    pub mean_absolute_error_seconds: f64,
}

/// One prediction and what happened, in seconds.
#[derive(Clone, Copy, Debug)]
struct Pair {
    predicted: f64,
    actual: f64,
    prior: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    member: String,
    label: Option<String>,
    warmth: Option<Warmth>,
    level: Option<Level>,
    estimator: Estimator,
}

fn agreement(key: Key, pairs: &[Pair]) -> Agreement {
    let median = |values: Vec<f64>| nearest_rank(&sorted(values), 0.5).unwrap_or(0.0);
    let errors: Vec<f64> = pairs.iter().map(|p| p.actual - p.predicted).collect();
    Agreement {
        member: key.member,
        label: key.label,
        warmth: key.warmth,
        level: key.level,
        estimator: key.estimator,
        samples: pairs.len(),
        priors: pairs.iter().filter(|p| p.prior).count(),
        predicted_median_seconds: median(pairs.iter().map(|p| p.predicted).collect()),
        actual_median_seconds: median(pairs.iter().map(|p| p.actual).collect()),
        ratio_median: nearest_rank(
            &sorted(
                pairs
                    .iter()
                    .filter(|p| p.predicted > 0.0)
                    .map(|p| p.actual / p.predicted)
                    .collect(),
            ),
            0.5,
        ),
        bias_seconds: mean(&errors).unwrap_or(0.0),
        mean_absolute_error_seconds: mean(&errors.iter().map(|e| e.abs()).collect::<Vec<_>>())
            .unwrap_or(0.0),
    }
}

/// Pairs collected per member and per label and member.
#[derive(Default)]
struct Collector {
    by_member: BTreeMap<Key, Vec<Pair>>,
    by_level: BTreeMap<Key, Vec<Pair>>,
    by_label: BTreeMap<Key, Vec<Pair>>,
}

impl Collector {
    fn add(
        &mut self,
        fact: &Fact,
        member: &str,
        warmth: Option<Warmth>,
        level: Option<Level>,
        pair: Pair,
    ) {
        let estimator = fact.predicted.as_ref().expect("predicted").estimator;
        let key = |label: Option<String>, level: Option<Level>| Key {
            member: member.to_owned(),
            label,
            warmth,
            level,
            estimator,
        };
        self.by_member
            .entry(key(None, None))
            .or_default()
            .push(pair);
        if level.is_some() {
            self.by_level
                .entry(key(None, level))
                .or_default()
                .push(pair);
        }
        if let Some(label) = &fact.label {
            self.by_label
                .entry(key(Some(label.clone()), None))
                .or_default()
                .push(pair);
        }
    }

    fn groups(self) -> Groups {
        let all = |groups: BTreeMap<Key, Vec<Pair>>| -> Vec<Agreement> {
            groups
                .into_iter()
                .map(|(key, pairs)| agreement(key, &pairs))
                .collect()
        };
        let by_member = all(self.by_member);
        let by_level = all(self.by_level);
        let mut labelled: Vec<(Key, Vec<Pair>)> = self.by_label.into_iter().collect();
        let omitted = labelled.len().saturating_sub(MAXIMUM_LABEL_GROUPS);
        if omitted > 0 {
            labelled.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
            labelled.truncate(MAXIMUM_LABEL_GROUPS);
            labelled.sort_by(|a, b| a.0.cmp(&b.0));
        }
        Groups {
            by_member,
            by_level,
            by_label: labelled
                .into_iter()
                .map(|(key, pairs)| agreement(key, &pairs))
                .collect(),
            by_label_omitted: omitted,
        }
    }
}

/// The accuracy of the predictions in `facts`, the requests a report covers.
pub fn accuracy(facts: &[Fact], context: &Context) -> Accuracy {
    let (mut wait, mut sync, mut run) = (
        Collector::default(),
        Collector::default(),
        Collector::default(),
    );
    for fact in facts.iter().filter(|f| !f.nested) {
        let (Some(member), Some(predicted)) = (&fact.member, &fact.predicted) else {
            continue;
        };
        if let (Some(p), Some(a)) = (predicted.wait_ms, fact.waited_ms) {
            let pair = Pair {
                predicted: seconds(p),
                actual: seconds(a),
                prior: false,
            };
            wait.add(fact, member, None, None, pair);
        }
        if let (Some(p), Some(a), Some(warmth)) = (predicted.sync_ms, fact.sync_ms, fact.warmth) {
            let pair = Pair {
                predicted: seconds(p),
                actual: seconds(a),
                prior: predicted.sync_prior,
            };
            sync.add(fact, member, Some(warmth), None, pair);
        }
        if fact.completed
            && fact.kind == Kind::Run
            && let (Some(p), Some(a)) = (predicted.run_ms, fact.run_ms)
        {
            let pair = Pair {
                predicted: seconds(p),
                actual: seconds(a),
                prior: predicted.run_prior,
            };
            run.add(fact, member, None, predicted.run_level, pair);
        }
    }
    Accuracy {
        wait: wait.groups(),
        sync: sync.groups(),
        run: run.groups(),
        speed_conversion: speed_conversion(facts, context),
        backtest: backtest::backtest(
            facts,
            context.history,
            context.master,
            context.speeds,
            context.parameters,
        ),
    }
}

/// For a label with completed warm runs on two members: how their run times
/// compared, against the ratio placement would convert them by had the label
/// not run on one of them.
#[derive(Debug, Serialize, PartialEq)]
pub struct SpeedConversion {
    pub label: String,
    pub faster: String,
    pub slower: String,
    /// Runs on the faster and the slower member.
    pub samples: [usize; 2],
    /// Median runs on the faster and the slower member.
    pub median_seconds: [f64; 2],
    /// Median run on the slower member ÷ median run on the faster.
    pub observed_ratio: f64,
    /// Mean measured speed of the faster ÷ of the slower, over the period.
    pub measured_ratio: f64,
    /// The ratio the two members' runs of other labels teach, as placement
    /// learns it, when they teach one.
    pub learned: Option<LearnedConversion>,
    /// What placement would convert by: the learned ratio, else the measured.
    pub converted_by: ConvertedBy,
    pub converted_ratio: f64,
    /// observed ÷ converted: above 1, the slower member is slower than
    /// conversion says.
    pub error: f64,
    /// observed ÷ measured, what conversion by measured speed alone would miss.
    pub measured_error: f64,
}

/// A speed ratio learned from other labels' runs.
#[derive(Debug, Serialize, PartialEq)]
pub struct LearnedConversion {
    pub ratio: f64,
    pub labels: usize,
    pub scope: RatioScope,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConvertedBy {
    Learned,
    Measured,
}

/// Runs a label needs on each of two members before their times are compared.
pub const CONVERSION_SAMPLES: usize = 3;

/// Speed ratios as placement learns them: from the requests made on the
/// master, whose records it reads, or every member's when no master is known.
fn learned_ratios(context: &Context) -> SpeedRatios {
    let samples: Vec<RunSample> = context
        .history
        .iter()
        .filter(|f| context.master.is_none_or(|m| f.calling_member == m))
        .filter_map(Fact::run_sample)
        .collect();
    SpeedRatios::learn(&samples, &context.parameters.estimates)
}

fn speed_conversion(facts: &[Fact], context: &Context) -> Vec<SpeedConversion> {
    let speeds = context.speeds;
    let samples: Vec<RunSample> = facts
        .iter()
        .filter(|f| !f.nested)
        .filter_map(Fact::run_sample)
        .collect();
    let ratios = learned_ratios(context);
    // Run times by label, then by member, and each label's family.
    let mut runs: BTreeMap<&str, BTreeMap<&str, Vec<f64>>> = BTreeMap::new();
    let mut families: BTreeMap<&str, Option<&str>> = BTreeMap::new();
    let mut newest_first: Vec<&RunSample> = samples.iter().filter(|s| !s.cold).collect();
    newest_first.sort_by_key(|s| std::cmp::Reverse(s.at));
    for sample in newest_first {
        families
            .entry(&sample.label)
            .or_insert(sample.family.as_deref());
        runs.entry(&sample.label)
            .or_default()
            .entry(&sample.member)
            .or_default()
            .push(sample.run.as_secs_f64());
    }
    let mut out = Vec::new();
    for (label, by_member) in runs {
        let family = families.get(label).copied().flatten();
        // (member, median run, samples, speed)
        let members: Vec<(&str, f64, usize, f64)> = by_member
            .into_iter()
            .filter(|(_, v)| v.len() >= CONVERSION_SAMPLES)
            .filter_map(|(member, v)| {
                let samples = v.len();
                let median = nearest_rank(&sorted(v), 0.5)?;
                Some((member, median, samples, *speeds.get(member)?))
            })
            .collect();
        for (i, a) in members.iter().enumerate() {
            for b in &members[i + 1..] {
                let (faster, slower) = if a.3 >= b.3 { (a, b) } else { (b, a) };
                if faster.1 <= 0.0 || slower.3 <= 0.0 {
                    continue;
                }
                let observed = slower.1 / faster.1;
                let measured = faster.3 / slower.3;
                let learned = ratios
                    .between(faster.0, slower.0, family, Some(label))
                    .map(|l| LearnedConversion {
                        ratio: l.ratio,
                        labels: l.labels,
                        scope: l.scope,
                    });
                let (converted_by, converted) = match &learned {
                    Some(learned) => (ConvertedBy::Learned, learned.ratio),
                    None => (ConvertedBy::Measured, measured),
                };
                out.push(SpeedConversion {
                    label: label.to_owned(),
                    faster: faster.0.to_owned(),
                    slower: slower.0.to_owned(),
                    samples: [faster.2, slower.2],
                    median_seconds: [faster.1, slower.1],
                    observed_ratio: observed,
                    measured_ratio: measured,
                    learned,
                    converted_by,
                    converted_ratio: converted,
                    error: observed / converted,
                    measured_error: observed / measured,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reporting::fixtures::{self, Run},
        scheduling::placement::Source,
    };

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// "build" on the devbox: predicted 10 s, took 23 s, three times, waits
    /// predicted 2 s; on the mini, predicted and took 20 s. "test" ran pinned on
    /// the mini against the label median of 4 s.
    fn facts() -> Vec<Fact> {
        let mut records = Vec::new();
        for (n, waited) in [(1, 2.0), (2, 4.0), (3, 6.0)] {
            records.push(fixtures::placed(Run {
                n,
                member: "devbox",
                actual_run: 23.0,
                waited,
                ..Run::default()
            }));
        }
        for n in 4..=6 {
            records.push(fixtures::placed(Run {
                n,
                predicted_run: 20.0,
                actual_run: 20.0,
                predicted_wait: 0.0,
                waited: 0.0,
                run_from: Source::Prior,
                ..Run::default()
            }));
        }
        records.push(fixtures::placed(Run {
            n: 7,
            label: "build",
            member: "devbox",
            warmth: Warmth::Cold,
            ..Run::default()
        }));
        records.push(fixtures::pinned(8, "test", "mini", 8.0));
        let mut nested = fixtures::pinned(9, "test", "mini", 100.0);
        nested.nested_in = Some(nested.run_id.clone());
        records.push(nested);
        records.iter().filter_map(Fact::of).collect()
    }

    fn speeds() -> BTreeMap<String, f64> {
        BTreeMap::from([("devbox".into(), 0.5), ("mini".into(), 1.0)])
    }

    /// The accuracy of `facts`, learning from them alone, without a master.
    fn judge(facts: &[Fact], speeds: &BTreeMap<String, f64>) -> Accuracy {
        accuracy(
            facts,
            &Context {
                history: facts,
                master: None,
                speeds,
                parameters: &Parameters::default(),
            },
        )
    }

    fn conversions(facts: &[Fact], speeds: &BTreeMap<String, f64>) -> Vec<SpeedConversion> {
        speed_conversion(
            facts,
            &Context {
                history: facts,
                master: None,
                speeds,
                parameters: &Parameters::default(),
            },
        )
    }

    fn find<'a>(
        agreements: &'a [Agreement],
        member: &str,
        label: Option<&str>,
        warmth: Option<Warmth>,
    ) -> &'a Agreement {
        agreements
            .iter()
            .find(|a| a.member == member && a.label.as_deref() == label && a.warmth == warmth)
            .unwrap_or_else(|| panic!("{member} {label:?} {warmth:?} in {agreements:#?}"))
    }

    #[test]
    fn run_estimates_by_label_and_member() {
        let accuracy = judge(&facts(), &speeds());
        let devbox = find(&accuracy.run.by_label, "devbox", Some("build"), None);
        // Three warm runs of 23 s and the cold one of 10 s, all predicted 10 s.
        assert_eq!(devbox.samples, 4);
        assert_eq!(devbox.estimator, Estimator::Placement);
        assert_eq!(devbox.priors, 0);
        assert_eq!(devbox.predicted_median_seconds, 10.0);
        assert_eq!(devbox.actual_median_seconds, 23.0);
        assert!(close(devbox.ratio_median.unwrap(), 2.3));
        assert!(close(devbox.bias_seconds, 39.0 / 4.0));
        assert!(close(devbox.mean_absolute_error_seconds, 39.0 / 4.0));
        let mini = find(&accuracy.run.by_label, "mini", Some("build"), None);
        assert_eq!((mini.samples, mini.priors), (3, 3));
        assert_eq!(mini.ratio_median, Some(1.0));
        assert_eq!(mini.bias_seconds, 0.0);
        let test = find(&accuracy.run.by_label, "mini", Some("test"), None);
        assert_eq!(test.estimator, Estimator::LabelMedian);
        assert_eq!(test.samples, 1, "the nested run is left out");
        assert_eq!(test.ratio_median, Some(2.0));
        assert!(close(test.bias_seconds, 4.0));
        // Per member, every label of one estimator together.
        assert_eq!(accuracy.run.by_member.len(), 3);
        let all_mini = accuracy
            .run
            .by_member
            .iter()
            .find(|a| a.member == "mini" && a.estimator == Estimator::Placement)
            .unwrap();
        assert_eq!(all_mini.samples, 3);
    }

    #[test]
    fn sync_estimates_by_warmth_and_waits_from_arrival() {
        let accuracy = judge(&facts(), &speeds());
        let warm = find(&accuracy.sync.by_member, "devbox", None, Some(Warmth::Warm));
        assert_eq!((warm.samples, warm.priors), (3, 3));
        assert_eq!(warm.predicted_median_seconds, 3.0);
        assert_eq!(warm.actual_median_seconds, 1.0);
        assert!(close(warm.bias_seconds, -2.0));
        let cold = find(
            &accuracy.sync.by_label,
            "devbox",
            Some("build"),
            Some(Warmth::Cold),
        );
        assert_eq!(cold.samples, 1);
        assert!(
            accuracy.sync.by_member.iter().all(|a| a.member == "devbox"),
            "nothing is synced on the calling member"
        );
        let wait = find(&accuracy.wait.by_member, "devbox", None, None);
        // Predicted 2 s each, waited 2, 4, 6 and 2 s.
        assert_eq!(wait.samples, 4);
        assert_eq!(wait.actual_median_seconds, 2.0);
        assert!(close(wait.bias_seconds, 1.5));
        assert!(close(wait.mean_absolute_error_seconds, 1.5));
        let mini_wait = find(&accuracy.wait.by_member, "mini", None, None);
        assert_eq!(mini_wait.ratio_median, None, "no prediction above zero");
        assert_eq!(mini_wait.samples, 3, "pinned runs predict no wait");
    }

    #[test]
    fn label_groups_are_capped_by_samples() {
        let mut facts = facts();
        let template = facts
            .iter()
            .find(|f| f.label.as_deref() == Some("test"))
            .unwrap()
            .clone();
        for n in 0..MAXIMUM_LABEL_GROUPS + 10 {
            facts.push(Fact {
                label: Some(format!("one-off {n}")),
                ..template.clone()
            });
        }
        let run = judge(&facts, &speeds()).run;
        assert_eq!(run.by_label.len(), MAXIMUM_LABEL_GROUPS);
        assert_eq!(
            run.by_label_omitted, 13,
            "210 one-offs, \"test\" and two \"build\" groups"
        );
        assert!(
            run.by_label
                .iter()
                .any(|a| a.label.as_deref() == Some("build") && a.samples == 4)
        );
        let mut lease = template;
        lease.kind = Kind::Lease;
        assert!(
            judge(&[lease], &speeds()).run.by_member.is_empty(),
            "leases are not runs"
        );
    }

    #[test]
    fn placements_run_estimates_are_grouped_by_level() {
        let run = judge(&facts(), &speeds()).run;
        let levels: Vec<(&str, Option<Level>, usize)> = run
            .by_level
            .iter()
            .map(|a| (a.member.as_str(), a.level, a.samples))
            .collect();
        assert_eq!(
            levels,
            [
                ("devbox", Some(Level::Label), 4),
                ("mini", Some(Level::Prior), 3)
            ],
            "the label median of pinned runs has no level"
        );
    }

    /// Labels "a", "b" and "c" ran three times on each member: 10 s on the mini
    /// and 20, 22 and 30 s on the devbox, whose measured speed says 2×.
    fn shared_labels() -> Vec<Fact> {
        let mut facts = Vec::new();
        let mut n = 0;
        for (label, devbox) in [("a", 20.0), ("b", 22.0), ("c", 30.0)] {
            for _ in 0..3 {
                n += 1;
                facts.push(Fact::of(&fixtures::pinned(n, label, "mini", 10.0)).unwrap());
                n += 1;
                facts.push(Fact::of(&fixtures::pinned(n, label, "devbox", devbox)).unwrap());
            }
        }
        facts
    }

    #[test]
    fn speed_conversion_compares_with_the_ratio_other_labels_teach() {
        let conversion = conversions(&shared_labels(), &speeds());
        assert_eq!(conversion.len(), 3);
        let c = conversion.iter().find(|c| c.label == "c").unwrap();
        assert_eq!(c.median_seconds, [10.0, 30.0]);
        assert!(close(c.observed_ratio, 3.0));
        assert_eq!(c.measured_ratio, 2.0);
        // Without "c", "a" and "b" teach 2.0 and 2.2.
        let learned = c.learned.as_ref().unwrap();
        assert_eq!((learned.labels, learned.scope), (2, RatioScope::Fleet));
        assert!(close(learned.ratio, 4.4f64.sqrt()));
        assert_eq!(c.converted_by, ConvertedBy::Learned);
        assert!(close(c.error, 3.0 / 4.4f64.sqrt()));
        assert!(close(c.measured_error, 1.5));
        // A learned ratio needs a label besides the one checked.
        let alone: Vec<Fact> = shared_labels()
            .into_iter()
            .filter(|f| f.label.as_deref() == Some("a"))
            .collect();
        let a = &conversions(&alone, &speeds())[0];
        assert_eq!(a.learned, None);
        assert_eq!(a.converted_by, ConvertedBy::Measured);
        assert!(close(a.error, a.measured_error));
    }

    #[test]
    fn speed_conversion_compares_warm_runs_with_measured_speeds() {
        let conversion = conversions(&facts(), &speeds());
        assert_eq!(conversion.len(), 1);
        let build = &conversion[0];
        assert_eq!(
            (build.faster.as_str(), build.slower.as_str()),
            ("mini", "devbox")
        );
        assert_eq!(build.samples, [3, 3], "the cold run is left out");
        assert!(close(build.observed_ratio, 23.0 / 20.0));
        assert_eq!(build.measured_ratio, 2.0);
        assert!(close(build.error, 0.575));
        assert!(conversions(&facts(), &BTreeMap::new()).is_empty());
    }
}
