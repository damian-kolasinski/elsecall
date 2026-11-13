//! Machine CPU attribution by family. Grant-derived outside load is circular,
//! so fit an idle baseline and active family counts to total machine CPU.
use crate::{load_log::buckets::Bucket, reporting::facts::Fact, scheduling::record::SizeBasis};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Demand {
    pub member: String,
    pub family: String,
    pub runs: usize,
    pub fitted_runs: usize,
    pub declared_cores: Option<f64>,
    pub process_tree_cores: Option<f64>,
    pub effective_cores: Option<f64>,
    /// Sensitivity band: residual error plus idle-load variation, not a calibrated confidence interval.
    pub interval_cores: Option<[f64; 2]>,
    pub fitted_minutes: usize,
    pub idle_minutes: usize,
    pub outside_baseline_cores: Option<f64>,
    pub residual_rmse_cores: Option<f64>,
    pub status: String,
}
pub fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let n = values.len();
    Some((values[(n - 1) / 2] + values[n / 2]) / 2.0)
}
pub fn family(f: &Fact) -> Option<&str> {
    f.family.as_deref().or(f.label.as_deref())
}
/// Commands and leases use their command/holder interval; nested work shares its
/// parent's resources and must not be counted twice.
pub fn interval(f: &Fact) -> Option<(u64, u64)> {
    if f.nested {
        return None;
    }
    let r = f.resources.as_ref()?;
    let start = r.started_at?.millis();
    let end = r.ended_at?.millis();
    (end > start).then_some((start, end))
}
pub fn overlap(start: u64, end: u64, at: u64, until: u64) -> f64 {
    end.min(until).saturating_sub(start.max(at)) as f64
}
/// Inverse of a small symmetric matrix using pivoted elimination; no ridge:
/// indistinguishable families must stay unidentified rather than learn sizes.
fn inverse(mut a: Vec<Vec<f64>>) -> Option<Vec<Vec<f64>>> {
    let n = a.len();
    let scale = a.iter().flatten().copied().fold(0.0, f64::max);
    let mut inv = vec![vec![0.0; n]; n];
    for (i, row) in inv.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    for i in 0..n {
        let pivot = (i..n).max_by(|&j, &k| a[j][i].abs().total_cmp(&a[k][i].abs()))?;
        if a[pivot][i].abs() < scale * 1e-8 {
            return None;
        }
        a.swap(i, pivot);
        inv.swap(i, pivot);
        let d = a[i][i];
        for k in 0..n {
            a[i][k] /= d;
            inv[i][k] /= d;
        }
        for j in 0..n {
            if j == i {
                continue;
            }
            let factor = a[j][i];
            for k in 0..n {
                a[j][k] -= factor * a[i][k];
                inv[j][k] -= factor * inv[i][k];
            }
        }
    }
    Some(inv)
}

pub fn estimate(facts: &[Fact], loads: &[(&str, &[Bucket])]) -> Vec<Demand> {
    estimate_at_width(facts, loads, 60_000)
}
pub fn estimate_at_width(
    facts: &[Fact],
    loads: &[(&str, &[Bucket])],
    width_ms: u64,
) -> Vec<Demand> {
    loads
        .iter()
        .flat_map(|(member, buckets)| estimate_member(member, facts, buckets, width_ms))
        .collect()
}
fn estimate_member(member: &str, facts: &[Fact], buckets: &[Bucket], width_ms: u64) -> Vec<Demand> {
    let work: Vec<_> = facts
        .iter()
        .filter(|f| f.member.as_deref() == Some(member) && !f.nested)
        .collect();
    let mut grouped: BTreeMap<&str, Vec<&Fact>> = BTreeMap::new();
    for f in &work {
        if let Some(name) = family(f) {
            grouped.entry(name).or_default().push(f);
        }
    }
    let families: Vec<String> = grouped.keys().map(|s| (*s).to_owned()).collect();
    let mut out: Vec<_> = families
        .iter()
        .map(|name| {
            let runs = &grouped[name.as_str()];
            Demand {
                member: member.into(),
                family: name.clone(),
                runs: runs.len(),
                fitted_runs: 0,
                declared_cores: median(
                    runs.iter()
                        .filter(|f| {
                            f.resources
                                .as_ref()
                                .is_some_and(|r| r.size_basis == Some(SizeBasis::Declared))
                        })
                        .map(|f| f.size.as_f64())
                        .collect(),
                ),
                process_tree_cores: median(
                    runs.iter()
                        .filter_map(|f| {
                            let ms = f.run_ms.filter(|ms| *ms > 0)?;
                            Some(f.resources.as_ref()?.cpu_ms? as f64 / ms as f64)
                        })
                        .collect(),
                ),
                effective_cores: None,
                interval_cores: None,
                fitted_minutes: 0,
                idle_minutes: 0,
                outside_baseline_cores: None,
                residual_rmse_cores: None,
                status: "insufficient_independent_load_history".into(),
            }
        })
        .collect();
    if families.is_empty() {
        return out;
    }
    if buckets.is_empty() || work.iter().all(|f| interval(f).is_none()) {
        let reason = if buckets.is_empty() {
            "missing_load_history"
        } else {
            "missing_completed_execution_intervals"
        };
        for d in &mut out {
            d.status = reason.into();
        }
        return out;
    }
    // Bound the regression before allocating rows. Profiles still cover every
    // family, including those without usable telemetry.
    let since = buckets.first().unwrap().at.millis();
    let until = buckets.last().unwrap().at.millis() + width_ms;
    let mut fit_to_out: Vec<usize> = (0..families.len())
        .filter(|&i| {
            grouped[families[i].as_str()]
                .iter()
                .any(|f| interval(f).is_some_and(|(s, e)| overlap(s, e, since, until) > 0.0))
        })
        .collect();
    let exposure = |i: usize| {
        grouped[families[i].as_str()]
            .iter()
            .filter_map(|f| interval(f))
            .map(|(s, e)| overlap(s, e, since, until))
            .sum::<f64>()
    };
    fit_to_out.sort_by(|&a, &b| exposure(b).total_cmp(&exposure(a)).then(a.cmp(&b)));
    fit_to_out.truncate(32);
    fit_to_out.sort_unstable();
    let families: Vec<_> = fit_to_out.iter().map(|&i| families[i].clone()).collect();
    let p = families.len() + 1;
    let mut rows = Vec::new();
    let mut idle = Vec::new();
    for b in buckets {
        if b.seconds * 1000 < width_ms * 9 / 10 || b.memory_pressure_seconds > 0 {
            continue;
        }
        let Some(cpu) = b.cpu_mean.filter(|v| *v < 0.98 && *v >= 0.0) else {
            continue;
        };
        let at = b.at.millis();
        let until = at + width_ms;
        let mut x = vec![0.0; p];
        x[0] = 1.0;
        let mut grants = 0.0;
        let mut unknown = false;
        for f in &work {
            // Unfinished work makes the covered minute unsuitable for fitting.
            let Some((start, end)) = interval(f) else {
                if f.resources
                    .as_ref()
                    .and_then(|r| r.started_at)
                    .is_some_and(|s| s.millis() < until)
                    && f.unfinished
                {
                    unknown = true;
                }
                continue;
            };
            let weight = overlap(start, end, at, until) / width_ms as f64;
            if weight == 0.0 {
                continue;
            }
            grants += weight * f.size.as_f64();
            if let Some(i) = family(f).and_then(|name| families.iter().position(|s| s == name)) {
                x[i + 1] += weight;
            } else {
                unknown = true;
            }
        }
        // Reject missing records, sync-only capacity and unobserved carry-in.
        if unknown || (grants - b.in_use_mean).abs() > 0.25 {
            continue;
        }
        let y = cpu * b.weighted_cores.as_f64();
        if x[1..].iter().all(|n| *n == 0.0) {
            idle.push(y);
        }
        rows.push((x, y, at));
    }
    // Historical families without any observed load must not make the fit
    // singular. Bound fitting cost by the 32 most-exposed families; discard
    // rows containing excluded work rather than attributing it to others.
    let mut indices: Vec<usize> = (1..p)
        .filter(|&i| {
            rows.iter().map(|(x, _, _)| x[i]).sum::<f64>() >= 1.0
                && rows.iter().map(|(x, _, _)| x[i] * x[i]).sum::<f64>() >= 0.05
        })
        .collect();
    indices.sort_by(|&a, &b| {
        let sum = |i: usize| rows.iter().map(|(x, _, _)| x[i]).sum::<f64>();
        sum(b).total_cmp(&sum(a)).then(a.cmp(&b))
    });
    indices.truncate(32);
    indices.sort_unstable();
    rows.retain(|(x, _, _)| (1..p).all(|i| indices.contains(&i) || x[i] == 0.0));
    for (x, _, _) in &mut rows {
        *x = std::iter::once(x[0])
            .chain(indices.iter().map(|&i| x[i]))
            .collect();
    }
    let p = indices.len() + 1;
    for &i in &indices {
        out[fit_to_out[i - 1]].fitted_minutes = rows.len();
        out[fit_to_out[i - 1]].idle_minutes = idle.len();
        out[fit_to_out[i - 1]].fitted_runs = work
            .iter()
            .filter(|f| family(f) == Some(families[i - 1].as_str()))
            .filter(|f| {
                interval(f).is_some_and(|(start, end)| {
                    rows.iter()
                        .any(|(_, _, at)| overlap(start, end, *at, *at + width_ms) > 0.0)
                })
            })
            .count();
    }
    for d in &mut out {
        d.status = "insufficient_family_exposure_in_usable_load".into();
    }
    let reason = if idle.len() < 3 {
        Some("insufficient_idle_minutes_requires_3")
    } else if rows.len() < p + 3 {
        Some("insufficient_residual_degrees_of_freedom_requires_3")
    } else {
        None
    };
    for &i in &indices {
        out[fit_to_out[i - 1]].status = reason
            .unwrap_or("insufficient_independent_load_history")
            .into();
    }
    if indices.is_empty() || reason.is_some() {
        return out;
    }
    let mut gram = vec![vec![0.0; p]; p];
    let mut rhs = vec![0.0; p];
    for (x, y, _) in &rows {
        for i in 0..p {
            rhs[i] += x[i] * y;
            for j in 0..p {
                gram[i][j] += x[i] * x[j];
            }
        }
    }
    let Some(inv) = inverse(gram) else {
        for &i in &indices {
            out[fit_to_out[i - 1]].status = "confounded_families".into();
        }
        return out;
    };
    let beta: Vec<f64> = inv
        .iter()
        .map(|row| row.iter().zip(&rhs).map(|(a, b)| a * b).sum())
        .collect();
    let sse: f64 = rows
        .iter()
        .map(|(x, y, _)| (y - x.iter().zip(&beta).map(|(a, b)| a * b).sum::<f64>()).powi(2))
        .sum();
    let variance = sse / (rows.len() - p) as f64;
    idle.sort_by(f64::total_cmp);
    let spread = (idle[idle.len() * 9 / 10] - idle[idle.len() / 10]).max(0.1);
    for (i, &index) in indices.iter().enumerate() {
        let d = &mut out[fit_to_out[index - 1]];
        d.outside_baseline_cores = Some(beta[0].max(0.0));
        d.residual_rmse_cores = Some((sse / rows.len() as f64).sqrt());
        let value = beta[i + 1];
        // A deliberately broad sensitivity band. Minutes may be correlated;
        // this is not a calibrated statistical confidence interval.
        let half = 4.5 * (variance * inv[i + 1][i + 1].max(0.0)).sqrt() + spread;
        if value < 0.0 || beta[0] < -spread {
            d.status = "confounded_background_load".into();
            continue;
        }
        d.effective_cores = Some(value);
        d.interval_cores = Some([(value - half).max(0.0), value + half]);
        d.status = "estimated_with_idle_baseline".into();
    }
    out
}

/// Clear disagreement needs both a meaningful absolute gap and an interval
/// wholly outside ±25% of the declared median. No finding for unknown demand.
pub fn disagreement(d: &Demand) -> bool {
    match (d.declared_cores, d.effective_cores, d.interval_cores) {
        (Some(size), Some(value), Some([low, high])) => {
            d.fitted_runs >= 5
                && (size - value).abs() >= 0.5
                && (low > size * 1.25 || high < size * 0.75)
        }
        _ => false,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        reporting::{
            facts::ResourceUse,
            fixtures::{self, T0},
        },
        scheduling::{capacity::Cores, timestamp::Timestamp},
    };
    pub fn run(n: u64, name: &str, start: u64, end: u64, size: f64) -> Fact {
        let mut f = Fact::of(&fixtures::pinned(
            n,
            name,
            "mini",
            (end - start) as f64 / 1000.0,
        ))
        .unwrap();
        f.size = Cores::from_f64(size).unwrap();
        f.family = Some(name.into());
        f.requested_at = Timestamp::from_millis(T0 + start);
        f.run_ms = Some(end - start);
        f.resources = Some(ResourceUse {
            started_at: Some(Timestamp::from_millis(T0 + start)),
            ended_at: Some(Timestamp::from_millis(T0 + end)),
            cpu_ms: Some((end - start) / 2),
            size_basis: Some(SizeBasis::Declared),
        });
        f
    }
    pub fn bucket(min: u64, cpu_cores: f64, grants: f64) -> Bucket {
        let mut b = fixtures::bucket(min, cpu_cores / 10.0, grants, 0, 0, 1.0);
        b.seconds = 60;
        b.weighted_cores = Cores::from_f64(10.0).unwrap();
        b.capacity = Cores::from_f64(8.0).unwrap();
        // Deliberately circular: subtracting this would teach declared grants.
        b.outside_mean = Some((cpu_cores - grants).max(0.0) / 10.0);
        b
    }
    #[test]
    fn recovers_two_true_family_demands_not_process_tree_or_declared_sizes() {
        let mut facts = Vec::new();
        let mut buckets = Vec::new();
        let mut n = 0;
        for minute in 0..24 {
            let (a, b) = match minute % 4 {
                0 => (0, 0),
                1 => (1, 0),
                2 => (0, 1),
                _ => (1, 1),
            };
            let at = minute * 60_000;
            if a > 0 {
                n += 1;
                facts.push(run(n, "a", at, at + 60_000, 3.0));
            }
            if b > 0 {
                n += 1;
                facts.push(run(n, "b", at, at + 60_000, 2.0));
            }
            buckets.push(bucket(
                minute,
                1.0 + a as f64 * 4.5 + b as f64,
                3.0 * a as f64 + 2.0 * b as f64,
            ));
        }
        let result = estimate(&facts, &[("mini", &buckets)]);
        assert!((result[0].effective_cores.unwrap() - 4.5).abs() < 1e-8);
        assert!((result[1].effective_cores.unwrap() - 1.0).abs() < 1e-8);
        assert_eq!(result[0].process_tree_cores, Some(0.5));
        assert_eq!(result[0].declared_cores, Some(3.0));
        assert!(result.iter().all(disagreement));
        for d in &result {
            let [lo, hi] = d.interval_cores.unwrap();
            assert!(lo < d.effective_cores.unwrap() && hi > d.effective_cores.unwrap());
        }
    }
    #[test]
    fn unsupported_historical_families_do_not_disable_supported_work() {
        let mut facts = vec![run(999, "old", 0, 1000, 1.0)];
        let mut buckets = Vec::new();
        for m in 10..26 {
            let busy = m % 2 == 1;
            if busy {
                facts.push(run(m, "a", m * 60_000, (m + 1) * 60_000, 2.0));
            }
            buckets.push(bucket(
                m,
                if busy { 4.0 } else { 1.0 },
                if busy { 2.0 } else { 0.0 },
            ));
        }
        let d = estimate(&facts, &[("mini", &buckets)]);
        assert!((d[0].effective_cores.unwrap() - 3.0).abs() < 1e-8);
        assert_eq!(d[1].family, "old");
        assert_eq!(d[1].effective_cores, None);
        // report uses all context to fit, then applies label/member filters.
        use crate::reporting::{
            document::{Gathered, MemberInput, Query, RecordsRead, document},
            window::Window,
        };
        let until = Timestamp::from_millis(T0 + 26 * 60_000);
        let gathered = Gathered {
            this_member: "mini".into(),
            master: Some("mini".into()),
            fleet_error: None,
            members: vec![MemberInput {
                member: "mini".into(),
                outside: Ok(Default::default()),
                load: Ok(buckets),
                records: Ok(RecordsRead::default()),
            }],
            facts,
            leftovers: vec![],
        };
        let mut q = Query {
            window: Window::new(Timestamp::from_millis(T0), until, until).unwrap(),
            series_width: std::time::Duration::from_secs(60),
            member: None,
            label: None,
            parameters: Default::default(),
        };
        let report = document(&q, &gathered);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.kind == "effective_demand" && f.label.as_deref() == Some("a"))
        );
        q.label = Some("old".into());
        let report = document(&q, &gathered);
        assert_eq!(report.metrics.demand.len(), 1);
        assert_eq!(report.metrics.demand[0].family, "old");
        assert!(!report.findings.iter().any(|f| f.kind == "effective_demand"));
    }

    #[test]
    fn one_fitted_run_with_old_runs_cannot_produce_a_disagreement_finding() {
        let mut facts = vec![run(1, "a", 10 * 60_000, 20 * 60_000, 1.0)];
        for n in 2..=6 {
            facts.push(run(n, "a", 0, 1000, 1.0));
        }
        let buckets: Vec<_> = (5..25)
            .map(|m| {
                bucket(
                    m,
                    if (10..20).contains(&m) { 4.0 } else { 1.0 },
                    if (10..20).contains(&m) { 1.0 } else { 0.0 },
                )
            })
            .collect();
        let d = estimate(&facts, &[("mini", &buckets)]).remove(0);
        assert_eq!(d.runs, 6);
        assert_eq!(d.fitted_runs, 1);
        assert!((d.effective_cores.unwrap() - 3.0).abs() < 1e-8);
        assert!(!disagreement(&d));
    }

    #[test]
    fn collinear_work_missing_idle_and_missing_inventory_are_not_true_demand() {
        let mut facts = Vec::new();
        let mut buckets = Vec::new();
        for m in 0..12 {
            let busy = m % 2 == 1;
            if busy {
                facts.push(run(m * 2 + 1, "a", m * 60_000, (m + 1) * 60_000, 3.0));
                facts.push(run(m * 2 + 2, "b", m * 60_000, (m + 1) * 60_000, 3.0));
            }
            buckets.push(bucket(
                m,
                if busy { 7.0 } else { 1.0 },
                if busy { 6.0 } else { 0.0 },
            ));
        }
        let estimates = estimate(&facts, &[("mini", &buckets)]);
        assert!(
            estimates
                .iter()
                .all(|d| d.status == "confounded_families" && d.effective_cores.is_none())
        );
        let missing = estimate(&facts[..1], &[("mini", &buckets)]);
        assert!(missing.iter().all(|d| d.effective_cores.is_none()));
        let no_idle: Vec<_> = buckets
            .into_iter()
            .filter(|b| b.in_use_mean > 0.0)
            .collect();
        assert!(
            estimate(&facts, &[("mini", &no_idle)])
                .iter()
                .all(|d| d.effective_cores.is_none())
        );
    }
    #[test]
    fn fractional_minute_overlap_and_nested_work_do_not_double_demand() {
        let mut facts = Vec::new();
        let mut buckets = Vec::new();
        for m in 0..12 {
            let busy = m % 2 == 1;
            if busy {
                let f = run(m, "a", m * 60_000 + 15_000, m * 60_000 + 45_000, 2.0);
                let mut nested = f.clone();
                nested.nested = true;
                nested.run_id = format!("r-{:012x}", m + 100);
                facts.extend([f, nested]);
            }
            buckets.push(bucket(
                m,
                if busy { 2.5 } else { 1.0 },
                if busy { 1.0 } else { 0.0 },
            ));
        }
        let d = estimate(&facts, &[("mini", &buckets)]).remove(0);
        assert!((d.effective_cores.unwrap() - 3.0).abs() < 1e-8);
        assert_eq!(d.runs, 6);
    }

    #[test]
    fn unidentified_demand_names_the_missing_evidence() {
        let facts = vec![run(1, "build", 0, 600_000, 2.0)];
        assert_eq!(
            estimate(&facts, &[("mini", &[])])[0].status,
            "missing_load_history"
        );
        let load: Vec<_> = (0..10).map(|m| bucket(m, 3.0, 2.0)).collect();
        let d = estimate(&facts, &[("mini", &load)]).remove(0);
        assert_eq!(d.status, "insufficient_idle_minutes_requires_3");
        assert_eq!(d.idle_minutes, 0);
        assert_eq!(d.fitted_minutes, 10);
        assert!(d.effective_cores.is_none());
    }
}
