//! Seeded shuffled coordinate sweeps with random restarts. Evaluation and I/O
//! belong to the caller. All generated points remain inside the declared grid.
use super::space::{Settings, Space};
use serde_json::Value;
pub struct Random(u64);
impl Random {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.next() as usize % (i + 1);
            items.swap(i, j);
        }
    }
}
pub fn neighbors(space: &Space, center: &Settings, random: &mut Random) -> Vec<Settings> {
    let mut points = Vec::new();
    for (key, values) in space {
        for value in std::iter::once(None).chain(values.iter().map(Some)) {
            let mut point = center.clone();
            if let Some(v) = value {
                point.insert(key.clone(), v.clone());
            } else {
                point.remove(key);
            }
            if point != *center {
                points.push(point);
            }
        }
    }
    random.shuffle(&mut points);
    points
}
pub fn restart(space: &Space, random: &mut Random) -> Settings {
    space
        .iter()
        .filter_map(|(key, values)| {
            let i = random.next() as usize % (values.len() + 1);
            values.get(i).map(|v| (key.clone(), v.clone()))
        })
        .collect()
}
#[derive(Debug, Clone)]
pub struct Quality {
    pub tier: u8,
    pub changes: usize,
    pub mean: f64,
    pub strongest: f64,
    pub violations: Vec<f64>,
}
pub fn quality(report: &Value) -> Quality {
    // Sensitivity envelopes describe risk; only the realistic point trial gates
    // and ranks a proposal. Acceptance still requires observed A/B evidence.
    let metrics = [&report["metrics"]];
    let has = |s: &str| metrics.iter().any(|m| m["status"] == s);
    let mean = report["metrics"]["objective"]["overhead"]["mean"]
        .as_f64()
        .unwrap_or(f64::INFINITY);
    let tier = if has("invalid")
        || has("unscorable")
        || metrics.iter().any(|m| m.is_null())
        || !mean.is_finite()
    {
        3
    } else if has("infeasible") {
        2
    } else if has("validation_needed") {
        1
    } else {
        0
    };
    let violations = [
        "admission_reserve_excess_core_seconds",
        "busy_rate_excess",
        "p90_slowdown_excess",
        "extra_other_refusals",
        "failed_run_excess",
    ]
    .iter()
    .map(|key| {
        metrics
            .iter()
            .map(|m| m["violation_vector"][key].as_f64().unwrap_or(f64::INFINITY))
            .fold(0.0, f64::max)
    })
    .collect();
    Quality {
        changes: report["changed_settings"].as_u64().unwrap_or(0) as usize,
        tier,
        mean,
        strongest: report["metrics"]["strongest"]["fraction"]
            .as_f64()
            .unwrap_or(0.0),
        violations,
    }
}
/// Constraints precede the primary overhead objective; strongest placement is
/// only a tie break. Absolute overhead may be zero or negative.
pub fn order(points: &[(Settings, Quality)]) -> Vec<usize> {
    let mut result: Vec<_> = (0..points.len()).collect();
    result.sort_by(|a, b| {
        let (sa, qa) = &points[*a];
        let (sb, qb) = &points[*b];
        qa.tier.cmp(&qb.tier).then_with(|| {
            if qa.tier >= 2 {
                for (a, b) in qa.violations.iter().zip(&qb.violations) {
                    let cmp = a.total_cmp(b);
                    if !cmp.is_eq() {
                        return cmp;
                    }
                }
            }
            qa.mean
                .total_cmp(&qb.mean)
                .then_with(|| qb.strongest.total_cmp(&qa.strongest))
                .then(qa.changes.cmp(&qb.changes))
                .then(sa.cmp(sb))
        })
    });
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    #[test]
    fn deterministic_bounded_sweeps_and_restarts() {
        let space = Space::from([
            ("x".into(), vec!["1".into(), "2".into()]),
            ("y".into(), vec!["3".into()]),
        ]);
        let a = neighbors(&space, &Settings::new(), &mut Random::new(7));
        assert_eq!(a, neighbors(&space, &Settings::new(), &mut Random::new(7)));
        assert_ne!(a, neighbors(&space, &Settings::new(), &mut Random::new(8)));
        for p in a
            .into_iter()
            .chain((0..50).map(|s| restart(&space, &mut Random::new(s))))
        {
            for (k, v) in p {
                assert!(space[&k].contains(&v));
            }
        }
    }
    #[test]
    fn ranks_by_mean_request_overhead_even_when_charged_latency_prefers_the_other() {
        let report = |h, charged| serde_json::json!({"metrics":{"status":"validation_needed","objective":{"overhead":{"mean":h},"mean_charged_seconds":charged}},"sensitivity":{"lower":{"status":"validation_needed"},"upper":{"status":"validation_needed"}}});
        let a = quality(&report(0.2, 100.0));
        let b = quality(&report(0.3, 1.0));
        assert_eq!(
            order(&[(Settings::new(), b), (Settings::new(), a)]),
            vec![1, 0]
        );
        assert_eq!(
            order(&[
                (Settings::new(), q(1, 0.0, 0.0)),
                (Settings::new(), q(1, -0.1, 0.0))
            ]),
            vec![1, 0]
        );
    }
    fn q(tier: u8, mean: f64, strongest: f64) -> Quality {
        Quality {
            changes: 0,
            tier,
            mean,
            strongest,
            violations: vec![0.0; 4],
        }
    }
    #[test]
    fn feasibility_precedes_latency_and_preference_uses_fixed_band() {
        let p = vec![
            (Settings::new(), q(1, 1.0, 0.0)),
            (BTreeMap::from([("a".into(), "1".into())]), q(0, 100.0, 0.0)),
            (Settings::new(), q(0, 100.9, 1.0)),
            (Settings::new(), q(0, 101.8, 1.0)),
        ];
        assert_eq!(order(&p), vec![1, 2, 3, 0]);
    }
    #[test]
    fn upper_envelope_infeasibility_does_not_gate_the_point() {
        let report = serde_json::json!({"metrics":{"status":"validation_needed","objective":{"overhead":{"mean":1}}},"sensitivity":{"lower":{"status":"validation_needed"},"upper":{"status":"infeasible"}}});
        assert_eq!(quality(&report).tier, 1);
    }
}
