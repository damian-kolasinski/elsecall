//! Harness objective and constraints, evaluated on arrival-window roots only.
use super::engine::{Outcome, Trial};
use crate::{
    scheduling::{
        placed::Warmth,
        request::{Kind, Placement},
        timestamp::Timestamp,
    },
    workload::{
        demand::median,
        trace::{Request, Trace},
    },
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
pub fn distribution(values: &[f64]) -> Value {
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let p = |q: f64| {
        (!v.is_empty()).then(|| v[((v.len() as f64 * q).ceil() as usize).saturating_sub(1)])
    };
    json!({"count":v.len(),"mean":(!v.is_empty()).then(||v.iter().sum::<f64>()/v.len() as f64),"median":median(v.clone()),"p90":p(0.9),"p95":p(0.95),"maximum":v.last()})
}
fn scheduling(member: Option<&str>, failure: Option<&str>) -> bool {
    member.is_some()
        || matches!(
            failure,
            Some("fleet_busy" | "request_too_large" | "member_unavailable" | "config_sync_failed")
        )
}
fn externally_fixed(failure: Option<&str>) -> bool {
    failure.is_some_and(|c| {
        !matches!(
            c,
            "fleet_busy"
                | "request_too_large"
                | "member_unavailable"
                | "config_sync_failed"
                | "deadline_skipped"
        )
    })
}
#[derive(Clone)]
struct RankedMember {
    member: String,
    warm_samples: usize,
    warm_median_ms: Option<f64>,
    median_core_speed: Option<f64>,
}
impl RankedMember {
    fn evidence(&self) -> &'static str {
        if self.warm_median_ms.is_some() {
            "matched_label_warm_median"
        } else if self.median_core_speed.is_some() {
            "median_recorded_core_speed"
        } else {
            "member_name_only_no_performance_evidence"
        }
    }
    fn tier(&self) -> u8 {
        if self.warm_median_ms.is_some() {
            0
        } else if self.median_core_speed.is_some() {
            1
        } else {
            2
        }
    }
}

/// Fixed entirely from trace evidence, before inspecting any trial's candidate
/// settings or eligible subset. Exact labels within a family keep separate ranks.
fn ranking(trace: &Trace, request: &Request) -> Vec<RankedMember> {
    let mut ranked: Vec<_> = trace
        .members
        .iter()
        .map(|member| {
            let warm: Vec<_> = trace
                .requests
                .iter()
                .filter(|r| {
                    request.fact.label.is_some()
                        && r.fact.label == request.fact.label
                        && r.fact.kind == Kind::Run
                        && r.fact.completed
                        && !r.fact.nested
                        && r.fact.member.as_ref() == Some(&member.member)
                        && r.fact.warmth == Some(Warmth::Warm)
                })
                .filter_map(|r| r.fact.run_ms.map(|ms| ms as f64))
                .collect();
            RankedMember {
                member: member.member.clone(),
                warm_samples: warm.len(),
                warm_median_ms: (warm.len() >= 3).then(|| median(warm).unwrap()),
                median_core_speed: median(
                    member
                        .load
                        .iter()
                        .filter_map(|b| b.speed_max.or(b.speed_min))
                        .collect(),
                ),
            }
        })
        .collect();
    ranked.sort_by(|a, b| {
        a.tier()
            .cmp(&b.tier())
            .then_with(|| match (a.warm_median_ms, b.warm_median_ms) {
                (Some(a), Some(b)) => a.total_cmp(&b),
                _ => match (a.median_core_speed, b.median_core_speed) {
                    (Some(a), Some(b)) => b.total_cmp(&a),
                    _ => std::cmp::Ordering::Equal,
                },
            })
            .then(a.member.cmp(&b.member))
    });
    ranked
}

#[derive(Default)]
struct StrongestGroup {
    ranking: Vec<RankedMember>,
    numerator: usize,
    denominator: usize,
    eligible_unknown: Vec<String>,
    performance_unknown: Vec<String>,
}

fn strongest_usage(trace: &Trace, outcomes: &[(&Request, &Outcome)]) -> Value {
    let mut groups = BTreeMap::<(Option<String>, Option<String>), StrongestGroup>::new();
    let mut of = 0usize;
    let mut numerator = 0usize;
    let mut denominator = 0usize;
    let mut unknown_eligibility = Vec::new();
    let mut unknown_performance = Vec::new();
    for (request, outcome) in outcomes {
        if !matches!(request.placement_request, Placement::Placeable)
            || request.fact.calling_member != trace.master.as_deref().unwrap_or("")
            || request.workspace.is_none()
        {
            continue;
        }
        of += 1;
        let key = (request.fact.family.clone(), request.fact.label.clone());
        let group = groups.entry(key).or_insert_with(|| StrongestGroup {
            ranking: ranking(trace, request),
            ..Default::default()
        });
        if outcome.eligible_members.is_empty() {
            group.eligible_unknown.push(outcome.run_id.clone());
            unknown_eligibility.push(outcome.run_id.clone());
            continue;
        }
        let eligible: BTreeSet<_> = outcome
            .eligible_members
            .iter()
            .map(String::as_str)
            .collect();
        let Some(strongest) = group
            .ranking
            .iter()
            .find(|r| eligible.contains(r.member.as_str()))
        else {
            group.eligible_unknown.push(outcome.run_id.clone());
            unknown_eligibility.push(outcome.run_id.clone());
            continue;
        };
        if strongest.tier() == 2 {
            group.performance_unknown.push(outcome.run_id.clone());
            unknown_performance.push(outcome.run_id.clone());
            continue;
        }
        let assigned = outcome.member.as_ref() == Some(&strongest.member);
        group.numerator += usize::from(assigned);
        group.denominator += 1;
        numerator += usize::from(assigned);
        denominator += 1;
    }
    let groups: Vec<_> = groups.into_iter().map(|((family, label), group)| {
        let ranking: Vec<_> = group.ranking.iter().enumerate().map(|(index, r)| json!({
            "rank":index+1,"member":r.member,"evidence":r.evidence(),"warm_label_samples":r.warm_samples,
            "warm_label_median_ms":r.warm_median_ms,"median_recorded_core_speed":r.median_core_speed,
        })).collect();
        json!({"family":family,"label":label,"ranking":ranking,"numerator":group.numerator,"denominator":group.denominator,
            "unknown_eligibility_run_ids":group.eligible_unknown,"unknown_performance_run_ids":group.performance_unknown})
    }).collect();
    let complete = unknown_eligibility.is_empty() && unknown_performance.is_empty();
    json!({"numerator":numerator,"denominator":denominator,"fraction":(denominator>0).then(||numerator as f64/denominator as f64),
        "freely_placeable_requests":of,"coverage":if complete {"complete"} else {"incomplete"},
        "eligibility_coverage":if unknown_eligibility.is_empty(){"complete"}else{"incomplete"},
        "performance_coverage":if unknown_performance.is_empty(){"complete"}else{"incomplete"},
        "unknown_eligibility_count":unknown_eligibility.len(),"unknown_eligibility_run_ids":unknown_eligibility,
        "unknown_performance_count":unknown_performance.len(),"unknown_performance_run_ids":unknown_performance,
        "groups":groups,"ranking_frozen_from":"trace warm exact-label medians (at least 3 completed root runs), then median minute core speed, then member name"})
}

/// External endings are fixed inputs, not outcomes that a candidate may trade
/// against latency. Check them even when fidelity mode was not requested.
fn external_invariants(
    trace: &Trace,
    trial: &Trial,
    since: Timestamp,
    until: Timestamp,
) -> (Vec<Value>, Vec<String>) {
    let outcomes: BTreeMap<_, _> = trial
        .outcomes
        .iter()
        .map(|o| (o.run_id.as_str(), o))
        .collect();
    let mut invalid = Vec::new();
    let mut unknown = Vec::new();
    for request in trace
        .requests
        .iter()
        .filter(|r| r.fact.requested_at >= since && r.fact.requested_at < until)
    {
        let recorded = request
            .fact
            .failure
            .as_deref()
            .filter(|code| externally_fixed(Some(code)));
        let outcome = outcomes.get(request.fact.run_id.as_str()).copied();
        let simulated = outcome
            .and_then(|o| o.failure.as_deref())
            .filter(|code| externally_fixed(Some(code)));
        if recorded.is_none() && simulated.is_none() {
            continue;
        }
        let observed_end = super::model::terminal(request).map(Timestamp::from_millis);
        let identity_matches =
            recorded == simulated && outcome.is_some_and(|o| o.member == request.fact.member);
        let ending_matches = match (observed_end, outcome.and_then(|o| o.terminal_at)) {
            (Some(a), Some(b)) => a == b,
            (None, _) => {
                unknown.push(request.fact.run_id.clone());
                true
            }
            (Some(_), None) => false,
        };
        if !identity_matches || !ending_matches {
            invalid.push(json!({"run_id":request.fact.run_id,"recorded":{"failure":recorded,"member":request.fact.member,"terminal_at":observed_end},
                "simulated":outcome.map(|o|json!({"failure":o.failure,"member":o.member,"terminal_at":o.terminal_at})),
                "identity_matches":identity_matches,"terminal_time_matches":ending_matches}));
        }
    }
    (invalid, unknown)
}

pub fn summarize(trace: &Trace, trial: &Trial, since: Timestamp, until: Timestamp) -> Value {
    let records: BTreeMap<_, _> = trace
        .requests
        .iter()
        .filter(|r| {
            !r.fact.nested
                && !r.fact.deferrable
                && r.fact.requested_at >= since
                && r.fact.requested_at < until
        })
        .map(|r| (r.fact.run_id.as_str(), r))
        .collect();
    let eligible: Vec<(&Request, &Outcome)> = trial
        .outcomes
        .iter()
        .filter_map(|o| records.get(o.run_id.as_str()).map(|r| (*r, o)))
        .collect();
    let score_population: Vec<_> = eligible
        .iter()
        .filter(|(r, _)| !externally_fixed(r.fact.failure.as_deref()))
        .collect();
    let charges: Vec<_> = score_population
        .iter()
        .filter(|(_, o)| o.member.is_some() || o.failure.as_deref() == Some("fleet_busy"))
        .filter_map(|(_, o)| o.charged_ms.map(|ms| ms / 1000.0))
        .collect();
    let unknown: Vec<_> = score_population
        .iter()
        .filter(|(_, o)| {
            o.terminal_at.is_none()
                || ((o.member.is_some() || o.failure.as_deref() == Some("fleet_busy"))
                    && (o.reference_ms.is_none() || o.charged_ms.is_none()))
        })
        .map(|(_, o)| o.run_id.as_str())
        .collect();
    let admitted: Vec<_> = score_population
        .iter()
        .filter(|(_, o)| o.member.is_some())
        .filter_map(|(r, o)| {
            o.terminal_at
                .map(|at| at.since(r.fact.requested_at).as_secs_f64())
        })
        .collect();
    let terminal = eligible
        .iter()
        .filter(|(_, o)| scheduling(o.member.as_deref(), o.failure.as_deref()))
        .count();
    let busy = eligible
        .iter()
        .filter(|(_, o)| o.failure.as_deref() == Some("fleet_busy"))
        .count();
    let rate = (terminal > 0).then(|| busy as f64 / terminal as f64);
    let root_runs: Vec<_> = trial
        .outcomes
        .iter()
        .filter_map(|o| {
            trace
                .requests
                .iter()
                .find(|r| {
                    if r.fact.run_id != o.run_id || r.fact.nested || r.fact.requested_at >= until {
                        return false;
                    }
                    let started = o
                        .waited_ms
                        .map(|wait| {
                            r.fact
                                .requested_at
                                .millis()
                                .saturating_add(wait)
                                .saturating_add(o.sync_ms)
                        })
                        .or_else(|| r.fact.resources.as_ref()?.started_at.map(Timestamp::millis))
                        .unwrap_or(r.fact.requested_at.millis());
                    // Carry-in is outside the objective population, but its
                    // execution still consumes reserve and must obey slowdown.
                    started < until.millis() && o.terminal_at.is_none_or(|at| at > since)
                })
                .map(|r| (r, o))
        })
        .collect();
    let slowdowns: Vec<_> = root_runs
        .iter()
        .filter(|(r, o)| r.fact.kind == Kind::Run && o.member.is_some())
        .filter_map(|(_, o)| o.slowdown)
        .collect();
    let uncertain: Vec<_> = root_runs
        .iter()
        .filter(|(r, o)| r.fact.kind == Kind::Run && o.member.is_some() && o.uncertain)
        .map(|(_, o)| o.run_id.as_str())
        .collect();
    let max_slowdown = slowdowns.iter().copied().reduce(f64::max);
    let mut baseline_refusals = BTreeMap::<&str, usize>::new();
    let mut refusals = BTreeMap::<&str, usize>::new();
    let mut fixed = BTreeMap::<&str, usize>::new();
    let mut extra_too_large = Vec::new();
    for (r, o) in &eligible {
        if let Some(code) = r
            .fact
            .failure
            .as_deref()
            .filter(|c| *c != "fleet_busy" && !externally_fixed(Some(c)))
        {
            *baseline_refusals.entry(code).or_default() += 1;
        }
        if let Some(code) = o.failure.as_deref() {
            if externally_fixed(Some(code)) {
                *fixed.entry(code).or_default() += 1;
            } else if code != "fleet_busy" {
                *refusals.entry(code).or_default() += 1;
            }
        }
        if o.failure.as_deref() == Some("request_too_large")
            && r.fact.failure.as_deref() != Some("request_too_large")
        {
            extra_too_large.push(o.run_id.as_str());
        }
    }
    let extra: usize = refusals
        .iter()
        .map(|(code, n)| n.saturating_sub(*baseline_refusals.get(code).unwrap_or(&0)))
        .sum();
    let strongest = strongest_usage(trace, &eligible);
    let (invalid_external, unknown_external) = external_invariants(trace, trial, since, until);
    let scorable =
        !charges.is_empty() && unknown.is_empty() && unknown_external.is_empty() && terminal > 0;
    let violated = trial.reserve.violating_member_seconds > 0.0
        || trial.reserve.declared_excess_core_seconds > 0.0
        || rate.is_some_and(|r| r > 0.01)
        || max_slowdown.is_some_and(|s| s > 1.25)
        || extra > 0
        || !extra_too_large.is_empty();
    let status = if !invalid_external.is_empty() {
        "invalid"
    } else if violated {
        "infeasible"
    } else if !scorable {
        "unscorable"
    } else {
        "validation_needed"
    };
    let charged = distribution(&charges);
    let mean = if scorable && invalid_external.is_empty() {
        charged["mean"].clone()
    } else {
        Value::Null
    };
    let score = if !invalid_external.is_empty() {
        Value::Null
    } else if violated {
        json!("infinity")
    } else if scorable {
        mean.clone()
    } else {
        Value::Null
    };
    let mut per_member = BTreeMap::new();
    for m in &trace.members {
        let rows: Vec<_> = trial
            .timelines
            .iter()
            .filter(|s| s.member == m.member)
            .collect();
        let seconds: f64 = rows.iter().map(|r| r.seconds).sum();
        let observed: f64 = rows.iter().map(|r| r.observed_seconds).sum();
        let unknown: f64 = rows.iter().map(|r| r.unknown_seconds).sum();
        let core_seconds: f64 = rows.iter().map(|r| r.cpu_core_seconds).sum();
        let weighted: f64 = rows
            .iter()
            .map(|r| r.observed_seconds * r.weighted_cores)
            .sum();
        per_member.insert(m.member.clone(),json!({"started_interactive_roots":eligible.iter().filter(|(_,o)|o.member.as_deref()==Some(m.member.as_str())).count(),"timeline_seconds":seconds,"observed_seconds":observed,"unknown_seconds":unknown,"observation_coverage":(seconds>0.0).then(||observed/seconds),"cpu_core_seconds":(observed>0.0).then_some(core_seconds),"cpu_mean":(weighted>0.0).then(||core_seconds/weighted),"grant_core_seconds":rows.iter().map(|r|r.declared_core_seconds).sum::<f64>(),"leftover_core_seconds":rows.iter().map(|r|r.leftover_core_seconds).sum::<f64>(),"queue_peak":rows.iter().map(|r|r.queued_max).max(),"interactive_queue_peak":rows.iter().map(|r|r.interactive_queued_max).max(),"gate_blocked_seconds":rows.iter().map(|r|r.blocked_seconds).sum::<f64>()}));
    }
    let mut uncertain_constraints =
        vec!["reserve: outside CPU is inferred rather than independently attributed".to_owned()];
    if trial.reserve.unknown_member_seconds > 0.0 {
        uncertain_constraints.push(format!(
            "reserve: outside CPU missing for {} member-seconds",
            trial.reserve.unknown_member_seconds
        ));
    }
    let mut result = json!({"status":status,"score":score,"objective":{"mean_charged_seconds":mean,"charged":charged,"actual_admitted_seconds":distribution(&admitted),"runs":eligible.iter().filter(|(r,_)|super::model::original_kind(trace,r)==Kind::Run).count(),"leases":eligible.iter().filter(|(r,_)|super::model::original_kind(trace,r)==Kind::Lease).count(),"unknown_reference_run_ids":unknown},
        "fleet_busy":{"numerator":busy,"denominator":terminal,"rate":rate,"limit":0.01},"other_refusals":refusals,"baseline_other_refusals":baseline_refusals,"extra_other_refusals":extra,"new_too_large_run_ids":extra_too_large,"cancelled_and_infrastructure":fixed,"external_event_invariants":{"status":if !invalid_external.is_empty(){"invalid"}else if !unknown_external.is_empty(){"missing_evidence"}else{"unchanged"},"outliers":invalid_external,"unknown_terminal_run_ids":unknown_external},
        "reserve":trial.reserve,"reserve_coverage":{"status":if trial.reserve.unknown_member_seconds>0.0 {"unknown"}else{"observed_inferred_outside"},"unknown_member_seconds":trial.reserve.unknown_member_seconds},"uncertain_constraints":uncertain_constraints,"slowdown":{"distribution":distribution(&slowdowns),"limit":1.25,"uncertain_run_ids":uncertain},
        "violation_vector":{"reserve_excess_core_seconds":(trial.reserve.unknown_member_seconds==0.0).then_some(trial.reserve.excess_core_seconds),"busy_rate_excess":rate.map(|r|(r-0.01).max(0.0)),"slowdown_excess":max_slowdown.map(|s|(s-1.25).max(0.0)),"extra_other_refusals":extra},
        "strongest":strongest,"members":per_member});
    let mut objective = super::objective::summarize(trace, trial, since, until);
    if !invalid_external.is_empty() || !unknown_external.is_empty() {
        objective["scorable"] = json!(false);
        objective["stopping_target_met"] = Value::Null;
        objective["overhead"]["mean"] = Value::Null;
        objective["overhead"]["p90"] = Value::Null;
    }
    result["scheduling_objective"] = objective.clone();
    result["objective"]["overhead"] = objective["overhead"].clone();
    let limits = objective["hard_limits"].as_object().expect("limits");
    let violated = limits.values().any(|v| v == false);
    result["status"] = json!(if !invalid_external.is_empty() {
        "invalid"
    } else if violated {
        "infeasible"
    } else if objective["scorable"] != true {
        "unscorable"
    } else {
        "validation_needed"
    });
    result["score"] = if !invalid_external.is_empty() {
        Value::Null
    } else if violated {
        json!("infinity")
    } else {
        objective["overhead"]["mean"].clone()
    };
    result["violation_vector"]["admission_reserve_excess_core_seconds"] =
        objective["admission_reserve"]["excess_core_seconds"].clone();
    result["violation_vector"]["p90_slowdown_excess"] = json!(
        objective["contention_slowdown"]["distribution"]["p90"]
            .as_f64()
            .map(|s| (s - 1.25).max(0.0))
    );
    result["violation_vector"]["failed_run_excess"] = json!(
        objective["failures"]["count"]
            .as_u64()
            .zip(objective["failures"]["baseline_count"].as_u64())
            .map(|(n, b)| n.saturating_sub(b))
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reporting::facts::PlacementKind,
        scheduling::record::WorkspaceIdentity,
        simulation::{engine::Reserve, model::Envelope},
    };

    fn fixture() -> (Trace, Trial) {
        let mut trace = crate::simulation::engine::tests::trace();
        trace.requests.truncate(1);
        let request = &mut trace.requests[0];
        request.placement_request = Placement::Placeable;
        request.fact.placement = PlacementKind::Placeable;
        request.workspace = Some(WorkspaceIdentity {
            workspace: "a".repeat(64),
            repository: "b".repeat(64),
        });
        let outcome = Outcome {
            run_id: request.fact.run_id.clone(),
            member: Some("a".into()),
            failure: None,
            waited_ms: Some(0),
            sync_ms: 0,
            leftovers_wait_ms: 0,
            run_ms: Some(1_000),
            terminal_at: Some(trace.since.after(std::time::Duration::from_secs(1))),
            state: "completed",
            ideal_ms: Some(1000.0),
            reference_ms: Some(1_000.0),
            charged_ms: Some(1_000.0),
            slowdown: Some(1.0),
            uncertain: false,
            eligible_members: vec!["a".into(), "b".into()],
        };
        (
            trace,
            Trial {
                envelope: Envelope::Point,
                outcomes: vec![outcome],
                timelines: vec![],
                reserve: Reserve::default(),
                missing_evidence: vec![],
            },
        )
    }
    fn report(trace: &Trace, trial: &Trial) -> Value {
        summarize(trace, trial, trace.since, trace.until)
    }

    #[test]
    fn strongest_is_selected_from_real_eligible_members_after_freezing_ranking() {
        let (trace, mut trial) = fixture();
        let initial = report(&trace, &trial);
        assert_eq!(initial["strongest"]["numerator"], 0);
        assert_eq!(initial["strongest"]["denominator"], 1);
        assert_eq!(
            initial["strongest"]["groups"][0]["ranking"][0]["member"],
            "b"
        );
        trial.outcomes[0].eligible_members = vec!["a".into()];
        let restricted = report(&trace, &trial);
        assert_eq!(restricted["strongest"]["numerator"], 1);
        assert_eq!(
            restricted["strongest"]["groups"][0]["ranking"],
            initial["strongest"]["groups"][0]["ranking"]
        );
        trial.outcomes[0].member = None;
        assert_eq!(report(&trace, &trial)["strongest"]["numerator"], 0);
        assert_eq!(report(&trace, &trial)["strongest"]["denominator"], 1);
    }

    #[test]
    fn missing_eligibility_or_performance_is_published_and_excluded() {
        let (mut trace, mut trial) = fixture();
        trial.outcomes[0].eligible_members.clear();
        let missing = report(&trace, &trial);
        assert_eq!(missing["strongest"]["denominator"], 0);
        assert_eq!(missing["strongest"]["unknown_eligibility_count"], 1);
        assert_eq!(missing["strongest"]["eligibility_coverage"], "incomplete");
        trial.outcomes[0].eligible_members = vec!["a".into()];
        for member in &mut trace.members {
            for bucket in &mut member.load {
                bucket.speed_min = None;
                bucket.speed_max = None;
            }
        }
        let unknown = report(&trace, &trial);
        assert_eq!(unknown["strongest"]["denominator"], 0);
        assert_eq!(unknown["strongest"]["unknown_performance_count"], 1);
    }

    #[test]
    fn warm_label_evidence_precedes_microbenchmark_and_labels_keep_separate_ranks() {
        let (mut trace, trial) = fixture();
        for n in 10..13 {
            let mut history = trace.requests[0].clone();
            history.fact.run_id = format!("r-{n:012x}");
            history.fact.warmth = Some(Warmth::Warm);
            history.fact.completed = true;
            trace.requests.push(history);
        }
        let result = report(&trace, &trial);
        assert_eq!(result["strongest"]["numerator"], 1);
        assert_eq!(
            result["strongest"]["groups"][0]["ranking"][0]["member"],
            "a"
        );
        assert_eq!(
            result["strongest"]["groups"][0]["ranking"][0]["evidence"],
            "matched_label_warm_median"
        );
        let mut another = trace.requests[0].clone();
        another.fact.label = Some("different exact label".into());
        let ranks = ranking(&trace, &another);
        assert_eq!(ranks[0].member, "b");
    }

    #[test]
    fn pinned_required_and_nonmaster_requests_never_enter_strongest_denominator() {
        let (mut trace, trial) = fixture();
        for placement in [Placement::Pinned, Placement::Member("a".into())] {
            trace.requests[0].placement_request = placement;
            assert_eq!(
                report(&trace, &trial)["strongest"]["freely_placeable_requests"],
                0
            );
        }
        trace.requests[0].placement_request = Placement::Placeable;
        trace.requests[0].fact.calling_member = "b".into();
        assert_eq!(report(&trace, &trial)["strongest"]["denominator"], 0);
    }

    #[test]
    fn actual_admitted_time_survives_missing_reference_and_unknown_outside_is_never_feasible() {
        let (trace, mut trial) = fixture();
        assert_eq!(report(&trace, &trial)["status"], "validation_needed");
        trial.outcomes[0].reference_ms = None;
        trial.outcomes[0].ideal_ms = None;
        trial.outcomes[0].charged_ms = None;
        let result = report(&trace, &trial);
        assert_eq!(result["status"], "unscorable");
        assert_eq!(result["objective"]["actual_admitted_seconds"]["count"], 1);
        assert_eq!(result["objective"]["actual_admitted_seconds"]["mean"], 1.0);
        assert!(result["objective"]["mean_charged_seconds"].is_null());
    }

    #[test]
    fn unresolved_requests_do_not_turn_the_known_subset_into_an_objective() {
        let (mut trace, mut trial) = fixture();
        let mut second = trace.requests[0].clone();
        second.fact.run_id = "r-000000000002".into();
        trace.requests.push(second);
        let mut queued = trial.outcomes[0].clone();
        queued.run_id = "r-000000000002".into();
        queued.member = None;
        queued.state = "queued";
        queued.terminal_at = None;
        queued.charged_ms = None;
        queued.run_ms = None;
        trial.outcomes.push(queued);
        let result = report(&trace, &trial);
        assert_eq!(result["status"], "unscorable");
        assert!(result["objective"]["mean_charged_seconds"].is_null());
        assert_eq!(result["objective"]["charged"]["count"], 1);
        assert_eq!(
            result["objective"]["unknown_reference_run_ids"],
            serde_json::json!(["r-000000000002"])
        );
    }

    #[test]
    fn deferrable_slowdown_is_a_hard_constraint_even_without_interactive_score() {
        let (mut trace, mut trial) = fixture();
        trace.requests[0].fact.deferrable = true;
        trial.outcomes[0].slowdown = Some(2.0);
        trial.outcomes[0].run_ms = Some(2000);
        let result = report(&trace, &trial);
        assert_eq!(result["status"], "infeasible");
        assert_eq!(result["score"], "infinity");
        assert_eq!(result["slowdown"]["distribution"]["maximum"], 2.0);
        assert_eq!(result["strongest"]["denominator"], 0);
    }

    #[test]
    fn external_identity_and_absolute_ending_are_fixed_even_without_fidelity() {
        let (mut trace, mut trial) = fixture();
        trace.requests[0].fact.failure = Some("cancelled".into());
        trial.outcomes[0].failure = Some("cancelled".into());
        // Add an ordinary request so there is an otherwise valid objective.
        let mut regular = trace.requests[0].clone();
        regular.fact.run_id = "r-000000000002".into();
        regular.fact.failure = None;
        trace.requests.push(regular);
        let ordinary = Outcome {
            run_id: "r-000000000002".into(),
            failure: None,
            member: Some("a".into()),
            waited_ms: Some(0),
            sync_ms: 0,
            leftovers_wait_ms: 0,
            run_ms: Some(1_000),
            terminal_at: trial.outcomes[0].terminal_at,
            state: "completed",
            ideal_ms: Some(1000.0),
            reference_ms: Some(1_000.0),
            charged_ms: Some(1_000.0),
            slowdown: Some(1.0),
            uncertain: false,
            eligible_members: vec!["a".into(), "b".into()],
        };
        trial.outcomes.push(ordinary);
        assert_eq!(report(&trace, &trial)["status"], "validation_needed");
        trial.outcomes[0].failure = None;
        let lost = report(&trace, &trial);
        assert_eq!(lost["status"], "invalid");
        assert_eq!(lost["score"], Value::Null);
        trial.outcomes[0].failure = Some("cancelled".into());
        trial.outcomes[0].terminal_at =
            Some(trace.since.after(std::time::Duration::from_millis(1_001)));
        assert_eq!(report(&trace, &trial)["status"], "invalid");
        trial.outcomes[0].terminal_at = trace.requests[0].fact.resources.as_ref().unwrap().ended_at;
        trial.outcomes[0].member = Some("b".into());
        assert_eq!(report(&trace, &trial)["status"], "invalid");
        trial.outcomes[0].member = Some("a".into());
        trace.requests[0].fact.resources.as_mut().unwrap().ended_at = None;
        trace.requests[0].decision = None;
        let unknown = report(&trace, &trial);
        assert_eq!(unknown["status"], "unscorable");
        assert_eq!(
            unknown["external_event_invariants"]["unknown_terminal_run_ids"][0],
            "r-000000000001"
        );
    }

    #[test]
    fn carry_in_slowdown_is_constrained_without_entering_arrival_objective() {
        let (mut trace, mut trial) = fixture();
        trace.requests[0].fact.requested_at = Timestamp::from_millis(trace.since.millis() - 1_000);
        trial.outcomes[0].slowdown = Some(2.0);
        trial.outcomes[0].run_ms = Some(2000);
        let result = report(&trace, &trial);
        assert_eq!(result["objective"]["runs"], 0);
        assert_eq!(result["objective"]["charged"]["count"], 0);
        assert_eq!(result["slowdown"]["distribution"]["maximum"], 2.0);
        assert_eq!(result["status"], "infeasible");
        trial.outcomes[0].terminal_at = Some(trace.since);
        let boundary = report(&trace, &trial);
        assert_eq!(boundary["slowdown"]["distribution"]["count"], 0);
    }
}
