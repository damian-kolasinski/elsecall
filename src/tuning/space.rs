//! Finite, explicit grids; validation delegates to the simulator's service and
//! consumer parsers. No alternate parameter validation lives in the search.
use crate::{
    failure::Result,
    scheduling::{capacity::Cores, parameters::KEYS},
    simulation::CandidateSettings,
    workload::trace::Trace,
};
use std::collections::{BTreeMap, BTreeSet};
pub type Settings = BTreeMap<String, String>;
pub type Space = BTreeMap<String, Vec<String>>;
pub fn arguments(settings: &Settings) -> Vec<String> {
    settings.iter().map(|(k, v)| format!("{k}={v}")).collect()
}
pub fn validate(trace: &Trace, settings: &Settings) -> Result<()> {
    CandidateSettings::parse(trace, &arguments(settings)).map(|_| ())
}
pub fn check(trace: &Trace, space: &Space) -> Result<()> {
    if space.is_empty() || space.len() > 128 || space.values().any(|v| v.is_empty() || v.len() > 32)
    {
        return Err(super::invalid(
            "Space requires 1–128 keys, each with 1–32 values.",
        ));
    }
    for (key, values) in space {
        if !KEYS.iter().any(|(k, _)| *k == key)
            && key != "hold_limit"
            && !key.starts_with("label.")
            && !key.starts_with("member.")
        {
            return Err(super::invalid(
                "Use canonical section.key parameter names in the space.",
            ));
        }
        if values.iter().collect::<BTreeSet<_>>().len() != values.len() {
            return Err(super::invalid("Space values must be distinct."));
        }
        for value in values {
            validate(trace, &BTreeMap::from([(key.clone(), value.clone())]))?;
        }
    }
    Ok(())
}
pub fn defaults(trace: &Trace) -> Space {
    let mut space = Space::new();
    for (key, values) in [
        ("placement.keep_local_margin", vec!["0", "0.1", "0.25"]),
        ("placement.keep_local_margin_floor", vec!["0s", "2s", "5s"]),
        ("placement.run_prior", vec!["15s", "60s", "240s"]),
        ("placement.sync_prior_warm", vec!["0s", "3s", "10s"]),
        (
            "placement.sync_prior_cold_with_history",
            vec!["10s", "30s", "90s"],
        ),
        ("placement.sync_prior_cold", vec!["30s", "120s", "300s"]),
        ("placement.cold_run_factor", vec!["1", "3", "6"]),
        ("admission.saturated_utilization", vec!["0.7", "0.8", "0.9"]),
        ("admission.memory_pressure", vec!["warning", "critical"]),
        ("hold_limit", vec!["30s", "120s", "600s"]),
    ] {
        space.insert(key.into(), values.into_iter().map(String::from).collect());
    }
    // Exact labels only: glob metacharacters require an explicit operator space.
    let labels: BTreeSet<_> = trace
        .requests
        .iter()
        .filter_map(|r| r.fact.label.as_ref())
        .collect();
    for label in labels
        .into_iter()
        .filter(|l| !l.contains(['*', '?']))
        .take(64.min(118_usize.saturating_sub(trace.members.len())))
    {
        let sizes: Vec<_> = trace
            .requests
            .iter()
            .filter(|r| r.fact.label.as_ref() == Some(label))
            .map(|r| r.fact.size)
            .collect();
        let max = sizes.iter().copied().max().unwrap();
        let values: BTreeSet<_> = [0.25, 0.5, 1.0, 2.0]
            .into_iter()
            .map(|scale| {
                Cores::from_millis(
                    ((max.millis() as f64 * scale).round() as u32).clamp(1, 1_000_000_000),
                )
                .to_string()
            })
            .collect();
        space.insert(format!("label.{label}.size"), values.into_iter().collect());
    }
    for member in &trace.members {
        if let Some(floor) = member
            .scheduler_settings
            .iter()
            .map(|s| s.capacity.reserved)
            .max()
        {
            let ceiling = member
                .scheduler_settings
                .iter()
                .map(|s| s.capacity.weighted_cores)
                .min()
                .unwrap();
            let values: BTreeSet<_> = [
                floor,
                floor.saturating_add(Cores::from_millis(500)),
                floor.saturating_add(Cores::ONE),
            ]
            .into_iter()
            .filter(|v| *v <= ceiling)
            .map(|v| v.to_string())
            .collect();
            if !values.is_empty() {
                space.insert(
                    format!("member.{}.reserved_cores", member.member),
                    values.into_iter().collect(),
                );
            }
        }
    }
    space
}

/// Count effective changes, rather than spelling-equivalent explicit overrides.
/// Historical heterogeneity is retained: an override changing any snapshot counts.
pub fn change_count(trace: &Trace, settings: &Settings) -> Result<usize> {
    let mut count = 0;
    let combined = CandidateSettings::parse(trace, &arguments(settings))?;
    for (key, value) in settings {
        let single = CandidateSettings::parse(trace, &[format!("{key}={value}")])?;
        let changed = if let Some(name) = key
            .strip_prefix("member.")
            .and_then(|k| k.strip_suffix(".reserved_cores"))
        {
            let member = trace
                .members
                .iter()
                .find(|m| m.member == name)
                .expect("validated member");
            member.scheduler_settings.iter().any(|s| {
                single
                    .capacity(name, &s.capacity)
                    .expect("validated capacity")
                    != s.capacity
            })
        } else if key.starts_with("label.") {
            let mut without = settings.clone();
            without.remove(key);
            let without = CandidateSettings::parse(trace, &arguments(&without))?;
            trace.requests.iter().any(|r| {
                combined.size(r).unwrap_or(r.fact.size) != without.size(r).unwrap_or(r.fact.size)
            })
        } else if key == "hold_limit" {
            trace
                .requests
                .iter()
                .any(|r| combined.urgency(r) != r.urgency)
        } else {
            trace.members.iter().any(|m| {
                m.scheduler_settings.is_empty()
                    || m.scheduler_settings.iter().any(|s| {
                        single
                            .parameters(&s.parameters)
                            .expect("validated parameters")
                            != s.parameters
                    })
            })
        };
        count += usize::from(changed);
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn trace() -> Trace {
        serde_json::from_value(serde_json::json!({"trace_version":1,"since":"2026-11-03T00:00:00Z","until":"2026-11-04T00:00:00Z","incomplete":false,"master":"mini","members":[{"member":"mini","errors":[],"load":[],"scheduler_settings":[{"at":"2026-11-03T00:00:00Z","parameters":{},"capacity":{"weighted_cores":4,"reserved":1,"capacity":3}}]}],"requests":[],"leftovers":[],"demand":[],"contention":[],"limits":[]})).unwrap()
    }
    #[test]
    fn defaults_are_valid_and_inside_documented_bounds() {
        let t = trace();
        let grid = defaults(&t);
        check(&t, &grid).unwrap();
        assert_eq!(grid["member.mini.reserved_cores"], ["1", "1.5", "2"]);
        assert_eq!(grid["placement.cold_run_factor"], ["1", "3", "6"]);
        assert_eq!(grid["hold_limit"], ["30s", "120s", "600s"]);
    }
    #[test]
    fn service_validation_and_reserve_floor_are_reused() {
        let t = trace();
        for (key, value) in [
            ("member.mini.reserved_cores", "0"),
            ("label.work.size", "0"),
            ("placement.run_prior", "0s"),
            ("admission.saturated_utilization", "1.1"),
            ("hold_limit", "25h"),
        ] {
            assert!(validate(&t, &Settings::from([(key.into(), value.into())])).is_err());
        }
        let combo = Settings::from([
            ("estimates.window".into(), "5".into()),
            ("estimates.minimum_runs".into(), "6".into()),
        ]);
        assert!(validate(&t, &combo).is_err());
        assert!(check(&t, &Space::new()).is_err());
        assert!(
            check(
                &t,
                &Space::from([("placement.run_prior".into(), vec!["1s".into(); 33])])
            )
            .is_err()
        );
    }
    #[test]
    fn equivalent_explicit_values_do_not_count_as_changed() {
        let t = trace();
        assert_eq!(
            change_count(
                &t,
                &Settings::from([
                    ("placement.keep_local_margin".into(), "0.10".into()),
                    ("member.mini.reserved_cores".into(), "1.000".into())
                ])
            )
            .unwrap(),
            0
        );
        assert_eq!(
            change_count(
                &t,
                &Settings::from([("placement.keep_local_margin".into(), "0.2".into())])
            )
            .unwrap(),
            1
        );
    }
}
