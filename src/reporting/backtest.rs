//! How well today's run estimates would have done over the period: every
//! completed run, estimated again from the runs requested before it, against
//! what it took. Pure.
//!
//! Recorded predictions say how the estimates of their time did, made by
//! whatever version and history they had. A backtest asks the question again
//! of this version, over the same runs, so a change to the estimates, or to
//! `[estimates]`, can be judged on real history before and after:
//! - **History** is what placement reads: the master's records, which are the
//!   requests made on it, requested within `recency` before the run, at most
//!   the latest [`learning::RATIO_RECORDS_READ`]. Runs still going when the
//!   estimated run was requested count too: facts keep no end time.
//! - **Estimate** is placement's run estimate for the member the run ran on,
//!   without contention: learned (label, converted or family), else the
//!   `run_prior` at the member's mean measured speed; into a cold copy, learned
//!   cold runs, else the warm estimate times `cold_run_factor`.
//! - **Baseline** is the same estimate from the label's own runs alone,
//!   converted between members by measured speed: what placement estimated
//!   before families and learned speed ratios.
//! - At most the latest [`MAXIMUM_RUNS`] runs of the period are estimated.
use crate::{
    reporting::{
        facts::Fact,
        statistics::{mean, nearest_rank, sorted},
    },
    scheduling::{
        learning::{self, RunHistory, RunSample, ratios::SpeedRatios},
        parameters::Parameters,
        placement::{Level, Source},
    },
};
use serde::Serialize;
use std::collections::BTreeMap;

/// The most runs one report estimates again.
pub const MAXIMUM_RUNS: usize = 500;

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Backtest {
    /// Runs estimated again.
    pub runs: usize,
    /// The period had more completed runs than one report estimates.
    pub limited: bool,
    /// The member whose requests were the history, the master's; absent when
    /// no master is known and every member's requests were.
    pub history_member: Option<String>,
    pub all: Comparison,
    /// By the level today's estimate came from.
    pub by_level: Vec<Comparison>,
}

/// Today's estimates against the baseline, over the same runs.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Comparison {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    pub samples: usize,
    pub estimate: Score,
    pub baseline: Score,
}

/// How a set of predictions compared with what happened.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Score {
    /// Predictions that were the prior.
    pub priors: usize,
    /// Median of actual ÷ predicted: above 1, estimates were too low.
    pub ratio_median: Option<f64>,
    /// Median of how many times off a prediction was, either way (1 is exact).
    pub error_factor_median: Option<f64>,
    pub mean_absolute_error_seconds: Option<f64>,
}

#[derive(Clone, Copy)]
struct Prediction {
    seconds: f64,
    level: Level,
}

/// The shortest time a ratio takes, so a run recorded as 0 ms counts.
const SHORTEST: f64 = 0.001;

fn score(pairs: &[(f64, Prediction)]) -> Score {
    let ratios: Vec<f64> = pairs
        .iter()
        .map(|(actual, p)| actual.max(SHORTEST) / p.seconds.max(SHORTEST))
        .collect();
    let factors: Vec<f64> = ratios.iter().map(|r| r.max(1.0 / r)).collect();
    let errors: Vec<f64> = pairs
        .iter()
        .map(|(actual, p)| (actual - p.seconds).abs())
        .collect();
    Score {
        priors: pairs
            .iter()
            .filter(|(_, p)| p.level == Level::Prior)
            .count(),
        ratio_median: nearest_rank(&sorted(ratios), 0.5),
        error_factor_median: nearest_rank(&sorted(factors), 0.5),
        mean_absolute_error_seconds: mean(&errors),
    }
}

/// Estimates `runs` (the period's facts, filtered as the report is) again from
/// `history` (every fact gathered), made on `master` when it is known.
/// `speeds` are members' mean measured speeds over the period.
pub fn backtest(
    runs: &[Fact],
    history: &[Fact],
    master: Option<&str>,
    speeds: &BTreeMap<String, f64>,
    parameters: &Parameters,
) -> Backtest {
    let estimates = &parameters.estimates;
    let mut known: Vec<RunSample> = history
        .iter()
        .filter(|f| master.is_none_or(|m| f.calling_member == m))
        .filter_map(Fact::run_sample)
        .collect();
    known.sort_by_key(|s| s.at);
    let mut targets: Vec<RunSample> = runs
        .iter()
        .filter(|f| !f.nested)
        .filter_map(Fact::run_sample)
        .collect();
    targets.sort_by_key(|s| s.at);
    let limited = targets.len() > MAXIMUM_RUNS;
    let targets = &targets[targets.len().saturating_sub(MAXIMUM_RUNS)..];
    let speed_of = |member: &str| speeds.get(member).copied();
    let typical = {
        let measured = sorted(speeds.values().copied().collect());
        measured.get(measured.len() / 2).copied().unwrap_or(1.0)
    };
    let mut pairs: Vec<(f64, Prediction, Prediction)> = Vec::new();
    for target in targets {
        let since = target
            .at
            .millis()
            .saturating_sub(estimates.recency.as_millis() as u64);
        let before = known.partition_point(|s| s.at < target.at);
        let from = known[..before].partition_point(|s| s.at.millis() < since);
        let window =
            &known[from.max(before.saturating_sub(learning::RATIO_RECORDS_READ as usize))..before];
        let own: Vec<RunSample> = window
            .iter()
            .filter(|s| s.label == target.label)
            .cloned()
            .collect();
        let kin: Vec<RunSample> = match &target.family {
            Some(family) => window
                .iter()
                .filter(|s| s.family.as_ref() == Some(family))
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        let today = RunHistory::new(
            &target.label,
            target.family.as_deref(),
            own.clone(),
            kin,
            SpeedRatios::learn(window, estimates),
            estimates,
        );
        let baseline = RunHistory::new(
            &target.label,
            None,
            own,
            Vec::new(),
            SpeedRatios::default(),
            estimates,
        );
        let speed = speed_of(&target.member).unwrap_or(typical);
        // As placement scores it: the learned cold run, else the warm estimate
        // (learned, else the prior) times the cold factor into a cold copy.
        let predict = |history: &RunHistory| -> Prediction {
            let learned = history.estimates_on(&target.member, target.cold, &speed_of);
            let of = |e: learning::RunEstimate| Prediction {
                seconds: e.run.as_secs_f64(),
                level: Source::of(e.basis).level().expect("a run source"),
            };
            let warm = learned.warm.map(of).unwrap_or(Prediction {
                seconds: parameters.placement.run_prior.as_secs_f64() / speed,
                level: Level::Prior,
            });
            match (target.cold, learned.cold) {
                (true, Some(cold)) => of(cold),
                (true, None) => Prediction {
                    seconds: warm.seconds * parameters.placement.cold_run_factor,
                    ..warm
                },
                (false, _) => warm,
            }
        };
        pairs.push((
            target.run.as_secs_f64(),
            predict(&today),
            predict(&baseline),
        ));
    }
    let comparison = |level: Option<Level>, pairs: &[&(f64, Prediction, Prediction)]| Comparison {
        level,
        samples: pairs.len(),
        estimate: score(&pairs.iter().map(|(a, e, _)| (*a, *e)).collect::<Vec<_>>()),
        baseline: score(&pairs.iter().map(|(a, _, b)| (*a, *b)).collect::<Vec<_>>()),
    };
    let mut by_level: BTreeMap<Level, Vec<&(f64, Prediction, Prediction)>> = BTreeMap::new();
    for pair in &pairs {
        by_level.entry(pair.1.level).or_default().push(pair);
    }
    Backtest {
        runs: pairs.len(),
        limited,
        history_member: master.map(str::to_owned),
        all: comparison(None, &pairs.iter().collect::<Vec<_>>()),
        by_level: by_level
            .into_iter()
            .map(|(level, pairs)| comparison(Some(level), &pairs))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::fixtures;

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    /// A run on `member`, requested there `n` seconds into the period.
    fn run(n: u64, label: &str, family: Option<&str>, member: &str, seconds: f64) -> Fact {
        let mut fact = Fact::of(&fixtures::pinned(n, label, member, seconds)).unwrap();
        fact.family = family.map(str::to_owned);
        fact
    }

    /// The mini is the master. "test a" took 10 s three times there; then a new
    /// "test b" of the same family took 12 s, "lint" 5 s, and "test c", made on
    /// the devbox, 25 s there. The devbox's own 1 s "test a" is not the master's
    /// history.
    fn history() -> Vec<Fact> {
        let family = Some("test *");
        vec![
            run(1, "test a", family, "mini", 10.0),
            run(2, "test a", family, "mini", 10.0),
            run(3, "test a", family, "mini", 10.0),
            run(5, "test a", family, "devbox", 1.0),
            run(10, "test b", family, "mini", 12.0),
            run(11, "lint", None, "mini", 5.0),
            run(12, "test c", family, "devbox", 25.0),
        ]
    }

    fn speeds() -> BTreeMap<String, f64> {
        BTreeMap::from([("devbox".into(), 0.5), ("mini".into(), 1.0)])
    }

    fn level(backtest: &Backtest, level: Level) -> &Comparison {
        backtest
            .by_level
            .iter()
            .find(|c| c.level == Some(level))
            .unwrap_or_else(|| panic!("{level:?} in {backtest:#?}"))
    }

    #[test]
    fn each_run_is_estimated_again_from_the_masters_earlier_runs() {
        let facts = history();
        let backtest = backtest(
            &facts,
            &facts,
            Some("mini"),
            &speeds(),
            &Parameters::default(),
        );
        assert_eq!((backtest.runs, backtest.limited), (7, false));
        assert_eq!(backtest.history_member.as_deref(), Some("mini"));
        // "test b": its family's 10 s on the mini (the baseline: the 60 s
        // prior); "test c": the family's four runs there converted to the
        // devbox at measured speeds, 20 s (the baseline: the prior of 120 s).
        let family = level(&backtest, Level::Family);
        assert_eq!(family.samples, 2);
        assert!(close(
            family.estimate.mean_absolute_error_seconds,
            (2.0 + 5.0) / 2.0
        ));
        assert!(close(
            family.baseline.mean_absolute_error_seconds,
            (48.0 + 95.0) / 2.0
        ));
        assert_eq!((family.estimate.priors, family.baseline.priors), (0, 2));
        // The devbox's "test a", from the mini's three at measured speeds:
        // 20 s, both ways.
        let converted = level(&backtest, Level::Converted);
        assert_eq!(converted.samples, 1);
        assert!(close(converted.estimate.mean_absolute_error_seconds, 19.0));
        assert!(close(converted.baseline.mean_absolute_error_seconds, 19.0));
        // The first three runs and "lint" had nothing to go on.
        let prior = level(&backtest, Level::Prior);
        assert_eq!(prior.samples, 4);
        assert_eq!(backtest.all.samples, 7);
        assert_eq!(backtest.all.estimate.priors, 4);
        assert_eq!(backtest.all.baseline.priors, 6);
        // The baseline is typically 6× off: the prior against 10 s runs.
        assert!(close(backtest.all.baseline.error_factor_median, 6.0));
    }

    #[test]
    fn without_a_master_every_members_runs_are_history_and_the_latest_runs_count() {
        let facts = history();
        let all = backtest(&facts, &facts, None, &speeds(), &Parameters::default());
        assert_eq!(all.history_member, None);
        // The devbox's own 1 s "test a" now counts: it is the label's history
        // for nothing, and too short for "test c"'s family, so the levels stay.
        assert_eq!(level(&all, Level::Converted).samples, 1);
        assert_eq!(level(&all, Level::Family).samples, 2);
        let many: Vec<Fact> = (0..MAXIMUM_RUNS as u64 + 3)
            .map(|n| run(n, "lint", None, "mini", 5.0))
            .collect();
        let limited = backtest(&many, &many, None, &speeds(), &Parameters::default());
        assert_eq!((limited.runs, limited.limited), (MAXIMUM_RUNS, true));
        assert!(
            close(limited.all.estimate.ratio_median, 1.0),
            "learned from the earlier ones"
        );
    }

    #[test]
    fn a_cold_run_without_cold_history_is_the_warm_estimate_times_the_cold_factor() {
        let mut facts = history();
        let mut cold = run(20, "test a", Some("test *"), "mini", 30.0);
        cold.warmth = Some(crate::scheduling::placed::Warmth::Cold);
        facts.push(cold);
        let only_cold: Vec<Fact> = facts[facts.len() - 1..].to_vec();
        let backtest = backtest(
            &only_cold,
            &facts,
            Some("mini"),
            &speeds(),
            &Parameters::default(),
        );
        // 10 s warm, three times over.
        let label = level(&backtest, Level::Label);
        assert!(close(label.estimate.ratio_median, 1.0));
        assert!(close(label.baseline.ratio_median, 1.0));
    }
}
