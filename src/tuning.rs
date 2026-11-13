//! Offline settings search; this module owns evaluation, not scheduling policy.
mod proposal;
mod search;
mod space;
use crate::{
    failure::{Failure, Result},
    scheduling::timestamp::Timestamp,
    simulation::{self, Options},
    workload::trace::Trace,
};
use serde_json::{Value, json};
use space::Settings;
pub use space::Space;
use std::{collections::BTreeSet, time::Instant};
pub struct TuneOptions {
    pub load_factor: crate::replay_load::LoadFactor,
    pub space: Option<Space>,
    pub budget: usize,
    pub seed: u64,
    pub top: usize,
    pub day: Option<Timestamp>,
    pub held_out_day: Option<Timestamp>,
}
pub fn invalid(message: impl Into<String>) -> Failure {
    Failure::new(
        "invalid_request",
        message,
        "Run elsecall tune --help; tune only prints proposals.",
    )
}
fn summary(mut report: Value) -> Value {
    if let Some(object) = report.as_object_mut() {
        object.remove("outcomes");
        object.remove("timelines");
        object.remove("fidelity");
    }
    report
}
fn evaluate(
    trace: &Trace,
    settings: &Settings,
    seed: u64,
    day: Option<Timestamp>,
    load_factor: crate::replay_load::LoadFactor,
) -> Result<Value> {
    let mut result = simulation::simulate(
        trace.clone(),
        Options {
            load_factor,
            settings: space::arguments(settings),
            seed,
            day,
            fidelity: false,
        },
    )
    .map(summary)?;
    result["changed_settings"] = json!(space::change_count(trace, settings)?);
    Ok(result)
}
fn ranked(candidates: &[(Settings, Value)]) -> Vec<usize> {
    search::order(
        &candidates
            .iter()
            .map(|(s, r)| (s.clone(), search::quality(r)))
            .collect::<Vec<_>>(),
    )
}
fn sensitivity(space: &Space, candidates: &[(Settings, Value)]) -> Value {
    let baseline = &candidates[0].1["metrics"]["objective"]["overhead"]["mean"];
    Value::Object(space.iter().map(|(key,values)| {
        let probes:Vec<_>=candidates.iter().enumerate().filter(|(_, (s,_))|s.len()==1 && s.contains_key(key)).map(|(i,(s,r))|json!({"candidate":i,"value":s[key],"metrics":r["metrics"],"envelopes":r["sensitivity"],"delta_mean_overhead":r["metrics"]["objective"]["overhead"]["mean"].as_f64().zip(baseline.as_f64()).map(|(a,b)|a-b)})).collect();
        (key.clone(),json!({"method":"one factor at recorded baseline; descriptive, not causal","values":values,"evaluated_values":probes.len(),"coverage":if probes.len()==values.len(){"complete"}else{"budget_limited"},"probes":probes}))
    }).collect())
}
fn wins(candidate: &Value, baseline: &Value) -> Option<bool> {
    let a = search::quality(candidate);
    let b = search::quality(baseline);
    if a.tier >= 3 || b.tier >= 3 {
        return None;
    }
    if a.tier == 2 {
        return Some(false);
    }
    let candidate_settings: Settings = candidate["settings"]
        .as_array()
        .expect("simulation settings")
        .iter()
        .map(|v| {
            let (k, v) = v
                .as_str()
                .expect("setting text")
                .rsplit_once('=')
                .expect("validated setting");
            (k.to_owned(), v.to_owned())
        })
        .collect();
    let points = vec![(Settings::new(), b), (candidate_settings, a)];
    Some(search::order(&points)[0] == 1)
}
pub fn tune(trace: Trace, held_out: Option<Trace>, options: TuneOptions) -> Result<Value> {
    if !(1..=1024).contains(&options.budget) || !(1..=20).contains(&options.top) {
        return Err(invalid("Budget must be 1–1024; top must be 1–20."));
    }
    simulation::validate_trace(&trace)?;
    if let Some(t) = &held_out {
        simulation::validate_trace(t)?;
    }
    if held_out.is_none() && options.held_out_day.is_some() {
        return Err(invalid("A held-out day requires held-out input."));
    }
    if let Some(t) = &held_out {
        let window = |trace: &Trace, day: Option<Timestamp>| {
            day.map(|d| (d, d.after(std::time::Duration::from_secs(86400))))
                .unwrap_or((trace.since, trace.until))
        };
        let (a, b) = window(&trace, options.day);
        let (c, d) = window(t, options.held_out_day);
        if a < d && c < b {
            return Err(invalid(
                "Tuning and held-out evaluation windows must not overlap; select distinct --day and --held-out-day values.",
            ));
        }
    }
    let space = options.space.unwrap_or_else(|| space::defaults(&trace));
    space::check(&trace, &space)?;
    let started = Instant::now();
    let baseline = evaluate(
        &trace,
        &Settings::new(),
        options.seed,
        options.day,
        options.load_factor,
    )?;
    let mut candidates = vec![(Settings::new(), baseline)];
    let mut seen = BTreeSet::from([Settings::new()]);
    let mut rejected = Vec::new();
    let mut random = search::Random::new(options.seed);
    let mut center = Settings::new();
    let mut sweeps = 0;
    let mut attempts = 0;
    // The first sweep supplies baseline sensitivities. Later sweeps refine the
    // incumbent; a stationary sweep starts from a seeded random grid point.
    while candidates.len() < options.budget && attempts < options.budget * 64 {
        let previous = center.clone();
        let mut evaluated = 0;
        for point in search::neighbors(&space, &center, &mut random) {
            if candidates.len() == options.budget {
                break;
            }
            if !seen.insert(point.clone()) {
                continue;
            }
            attempts += 1;
            match space::validate(&trace, &point) {
                Ok(()) => {
                    let report = evaluate(
                        &trace,
                        &point,
                        options.seed,
                        options.day,
                        options.load_factor,
                    )?;
                    candidates.push((point, report));
                    evaluated += 1;
                }
                Err(e) => rejected.push(json!({"settings":point,"code":e.code,"reason":e.message})),
            }
        }
        sweeps += 1;
        center = candidates[ranked(&candidates)[0]].0.clone();
        if candidates.len() == options.budget {
            break;
        }
        if center == previous || evaluated == 0 {
            let mut found = None;
            for _ in 0..64 {
                attempts += 1;
                let point = search::restart(&space, &mut random);
                if seen.insert(point.clone()) {
                    found = Some(point);
                    break;
                }
            }
            let Some(point) = found else {
                break;
            };
            match space::validate(&trace, &point) {
                Ok(()) => {
                    let report = evaluate(
                        &trace,
                        &point,
                        options.seed,
                        options.day,
                        options.load_factor,
                    )?;
                    center = point.clone();
                    candidates.push((point, report));
                }
                Err(e) => rejected.push(json!({"settings":point,"code":e.code,"reason":e.message})),
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let order = ranked(&candidates);
    let best = if search::quality(&candidates[order[0]].1).tier == 3 {
        0
    } else {
        order[0]
    };
    let (settings, report) = &candidates[best];
    // Fidelity is never read from an editable certificate or candidate overrides.
    let fidelity = simulation::simulate(
        trace.clone(),
        Options {
            fidelity: true,
            day: options.day,
            seed: options.seed,
            ..Default::default()
        },
    )?;
    let mut trust_reasons = Vec::new();
    let fidelity_pass =
        trace.rehearsal.is_none() && fidelity["fidelity"]["trusted_for_tuning"] == true;
    if !fidelity_pass {
        trust_reasons.push("Run a complete rehearsal to obtain a current simulator certificate. Complete recorded-day plus held-out fidelity remains an optional stronger route; inspect fidelity_evidence for blockers.");
    }
    let held = if let Some(t) = held_out {
        let baseline = evaluate(
            &t,
            &Settings::new(),
            options.seed,
            options.held_out_day,
            options.load_factor,
        )?;
        space::validate(&t, settings)?;
        let candidate = evaluate(
            &t,
            settings,
            options.seed,
            options.held_out_day,
            options.load_factor,
        )?;
        let gate = simulation::simulate(
            t,
            Options {
                fidelity: true,
                day: options.held_out_day,
                seed: options.seed,
                ..Default::default()
            },
        )?;
        Some(
            json!({"baseline":baseline,"candidate":candidate,"wins":wins(&candidate,&baseline),"fidelity_evidence":gate["fidelity"]}),
        )
    } else {
        None
    };
    if held.is_none() {
        trust_reasons.push("Validate the proposal with rehearse --compare. Independent held-out simulation remains available to check overfitting.");
    }
    let tuning_win = wins(report, &candidates[0].1);
    let only_tuning = held
        .as_ref()
        .and_then(|h| h["wins"].as_bool())
        .map(|win| tuning_win == Some(true) && !win);
    let trusted = fidelity_pass
        && held
            .as_ref()
            .is_some_and(|h| h["fidelity_evidence"]["trusted_for_tuning"] == true);
    let recorded_day_certificate=trusted.then(|| {
        let issued_at=Timestamp::now();
        json!({"basis":"recorded_day_and_held_out","trace_digest":fidelity["trace_digest"],"held_out_trace_digest":held.as_ref().map(|h|&h["baseline"]["trace_digest"]),"build":fidelity["code_build"],"issued_at":issued_at,"expires_at":issued_at.after(crate::tuning_trust::MAX_AGE),"fidelity_digest":crate::build_identity::Digest::of(&serde_json::to_vec(&fidelity["fidelity"]).expect("fidelity serializes")),"held_out_fidelity_digest":held.as_ref().map(|h|crate::build_identity::Digest::of(&serde_json::to_vec(&h["fidelity_evidence"]).expect("fidelity serializes")))})
    });
    let sensitivity = sensitivity(&space, &candidates);
    let alternatives = order
        .iter()
        .copied()
        .filter(|i| *i != best)
        .take(options.top)
        .map(|i| json!({"candidate":i,"settings":candidates[i].0,"evaluation":candidates[i].1}))
        .collect::<Vec<_>>();
    let ledger=candidates.iter().enumerate().map(|(i,(s,r))|json!({"candidate":i,"settings":s,"metrics":r["metrics"],"envelopes":r["sensitivity"],"classification":match search::quality(r).tier{0=>"feasible",1=>"validation_needed",2=>"infeasible",_=>"invalid_or_unscorable"}})).collect::<Vec<_>>();
    Ok(
        json!({"tuning_version":2,"load_factor":options.load_factor,"ranking_basis":"point; envelopes report risk only","method":"seeded shuffled coordinate sweeps with random restarts; fixed finite grid; cached unique candidates; point and lower/upper envelopes per evaluation","seed":options.seed,"space":space,"budget":options.budget,"evaluated_candidates":candidates.len(),"sweeps":sweeps,"rejected_candidates":rejected,
        "baseline":candidates[0].1,"best":{"candidate":best,"settings":settings,"evaluation":report},"alternatives":alternatives,"candidates":ledger,"parameter_sensitivity":sensitivity,
        "recorded_day_certificate":recorded_day_certificate,"trusted_for_tuning":trusted,"proposal_status":if trusted{"awaiting_ab_rehearsal"}else{"untrusted"},"accepted_win":false,"trust_missing":trust_reasons,"fidelity_evidence":fidelity["fidelity"],
        "objective_comparison":crate::tuning_objective::compare(&candidates[0].1["metrics"]["scheduling_objective"],&report["metrics"]["scheduling_objective"],None),"tuning_win":tuning_win,"wins_only_on_tuning_trace":only_tuning,"held_out":held,"proposed_change":proposal::render(&trace,settings)?,
        "performance":{"search_elapsed_seconds":elapsed,"candidates_per_minute":candidates.len() as f64*60.0/elapsed,"scope":"Unique tuning evaluations including baseline and all three envelopes; excludes fidelity and held-out evaluation. Timing varies; settings/metrics are deterministic."},
        "limits":["Proposals only: no installed settings or consumer files are read or changed.","The primary objective is simulated per-request scheduling overhead; measured acceptance requires A/A noise and rehearsal.","Exploratory validation-needed proposals are never accepted tuning wins.","Grid search with a finite budget does not guarantee a global optimum; unevaluated sensitivities stay explicit."]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn report(mean: f64, strongest: f64, status: &str) -> Value {
        let metric = json!({"status":status,"objective":{"overhead":{"mean":mean}},"strongest":{"fraction":strongest},"violation_vector":{"reserve_excess_core_seconds":0,"busy_rate_excess":0,"slowdown_excess":0,"extra_other_refusals":0}});
        json!({"settings":["placement.keep_local_margin=0.2"],"changed_settings":1,"metrics":metric,"sensitivity":{"lower":metric,"upper":metric}})
    }
    #[test]
    fn held_out_uses_overhead_before_strongest_preference_and_flags_infeasibility() {
        assert_eq!(
            wins(
                &report(99.5, 0.0, "validation_needed"),
                &report(100.0, 1.0, "validation_needed")
            ),
            Some(true)
        );
        assert_eq!(
            wins(
                &report(100.5, 1.0, "validation_needed"),
                &report(100.0, 0.0, "validation_needed")
            ),
            Some(false)
        );
        assert_eq!(
            wins(
                &report(10.0, 1.0, "infeasible"),
                &report(100.0, 0.0, "validation_needed")
            ),
            Some(false)
        );
        assert_eq!(
            wins(
                &report(10.0, 1.0, "invalid"),
                &report(100.0, 0.0, "validation_needed")
            ),
            None
        );
    }
}
