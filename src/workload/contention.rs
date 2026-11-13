//! Observational duration curves, paired by exact label and warmth before
//! pooling into a family. These are associations, not causal slowdown claims.
use crate::{
    load_log::buckets::Bucket,
    reporting::facts::Fact,
    workload::demand::{family, interval, median, overlap},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Point {
    pub other_declared_cores: [f64; 2],
    pub runs: usize,
    pub duration_ratio_median: f64,
    pub duration_ratio_range: [f64; 2],
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Curve {
    pub member: String,
    pub family: String,
    pub matched_runs: usize,
    pub unmatched_runs: usize,
    pub points: Vec<Point>,
    pub status: String,
}
pub fn derive(facts: &[Fact], loads: &[(&str, &[Bucket])]) -> Vec<Curve> {
    let mut out = Vec::new();
    for (member, buckets) in loads {
        let work: Vec<_> = facts
            .iter()
            .filter(|f| f.member.as_deref() == Some(*member) && !f.nested)
            .collect();
        // family -> (label, warmth, other load, run seconds)
        let mut groups: BTreeMap<String, Vec<(&str, String, f64, f64)>> = BTreeMap::new();
        for f in &work {
            if !f.completed || f.kind != crate::scheduling::request::Kind::Run {
                continue;
            }
            let (Some(name), Some(label), Some((start, end)), Some(ms)) = (
                family(f),
                f.label.as_deref(),
                interval(f),
                f.run_ms.filter(|ms| *ms >= 1000),
            ) else {
                continue;
            };
            let span = (end - start) as f64;
            let coverage: f64 = buckets
                .iter()
                .map(|b| {
                    overlap(start, end, b.at.millis(), b.at.millis() + 60_000) * b.seconds as f64
                        / 60.0
                })
                .sum();
            if coverage < span * 0.9 {
                continue;
            }
            let competing: f64 = work
                .iter()
                .filter(|g| g.run_id != f.run_id)
                .filter_map(|g| {
                    let resources = g.resources.as_ref()?;
                    let s = resources.started_at?.millis();
                    // Right-censored grants still occupy capacity through the
                    // completed target's end. They are never an idle baseline.
                    let e = resources.ended_at.map_or(end, |at| at.millis());
                    Some(overlap(s, e, start, end) * g.size.as_f64() / span)
                })
                .sum();
            // Outside proxy is shown in raw load; do not treat it as measured
            // independent demand. Curves use known competing grants only.
            groups.entry(name.into()).or_default().push((
                label,
                format!("{:?}", f.warmth),
                competing,
                ms as f64 / 1000.0,
            ));
        }
        for (name, runs) in groups {
            let mut bins: [Vec<f64>; 4] = Default::default();
            let mut unmatched = 0;
            for (label, warmth, other, seconds) in &runs {
                let baseline: Vec<_> = runs
                    .iter()
                    .filter(|(l, w, o, _)| l == label && w == warmth && *o < 0.5)
                    .map(|(_, _, _, s)| *s)
                    .collect();
                if baseline.len() < 3 {
                    unmatched += 1;
                    continue;
                }
                let base = median(baseline).unwrap();
                let bin = if *other < 0.5 {
                    0
                } else if *other < 2.0 {
                    1
                } else if *other < 4.0 {
                    2
                } else {
                    3
                };
                bins[bin].push(seconds / base);
            }
            let ranges = [[0.0, 0.5], [0.5, 2.0], [2.0, 4.0], [4.0, f64::MAX]];
            let points: Vec<_> = bins
                .into_iter()
                .enumerate()
                .filter(|(_, v)| !v.is_empty())
                .map(|(i, values)| Point {
                    other_declared_cores: ranges[i],
                    runs: values.len(),
                    duration_ratio_median: median(values.clone()).unwrap(),
                    duration_ratio_range: [
                        values.iter().copied().reduce(f64::min).unwrap(),
                        values.iter().copied().reduce(f64::max).unwrap(),
                    ],
                })
                .collect();
            let enough = points.iter().filter(|p| p.runs >= 3).count() >= 2;
            out.push(Curve {
                member: (*member).into(),
                family: name,
                matched_runs: runs.len() - unmatched,
                unmatched_runs: unmatched,
                points,
                status: if enough {
                    "observational_curve"
                } else {
                    "insufficient_comparable_concurrency"
                }
                .into(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::demand::tests::{bucket, run};
    #[test]
    fn unfinished_competing_work_is_not_an_uncontended_baseline() {
        let mut facts = Vec::new();
        for n in 0..6 {
            let at = n * 180_000;
            facts.push(run(n + 1, "test", at, at + 60_000, 1.0));
        }
        let mut held = run(100, "background", 0, 1_800_000, 4.0);
        held.unfinished = true;
        held.resources.as_mut().unwrap().ended_at = None;
        held.completed = false;
        facts.push(held);
        let buckets: Vec<_> = (0..20).map(|m| bucket(m, 5.0, 5.0)).collect();
        let curves = derive(&facts, &[("mini", &buckets)]);
        let curve = curves.iter().find(|c| c.family == "test").unwrap();
        assert_eq!(curve.unmatched_runs, 6);
        assert!(curve.points.is_empty());
    }

    #[test]
    fn known_concurrency_doubles_matched_runs_without_mixing_labels() {
        let mut facts = Vec::new();
        for n in 0..6 {
            let at = n * 180_000;
            let busy = n >= 3;
            let duration = if busy { 120_000 } else { 60_000 };
            facts.push(run(n + 1, "test", at, at + duration, 1.0));
            if busy {
                facts.push(run(n + 100, "background", at, at + duration, 3.0));
            }
        }
        let buckets: Vec<_> = (0..18).map(|m| bucket(m, 5.0, 4.0)).collect();
        let curves = derive(&facts, &[("mini", &buckets)]);
        let c = curves.iter().find(|c| c.family == "test").unwrap();
        assert_eq!(c.status, "observational_curve");
        assert_eq!(c.points.len(), 2);
        assert_eq!(c.points[0].duration_ratio_median, 1.0);
        assert_eq!(c.points[1].duration_ratio_median, 2.0);
        assert_eq!(c.points[1].runs, 3);
        let c = curves.iter().find(|c| c.family == "background").unwrap();
        assert_eq!(c.unmatched_runs, 3);
        assert!(c.points.is_empty());
    }
}
