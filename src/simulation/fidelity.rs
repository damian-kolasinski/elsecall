//! Recorded-day validation. Tolerances are fixed by the harness contract, never
//! inferred from residuals or relaxed to make an incomplete recording pass.
use crate::{
    scheduling::{
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

const DAY: u64 = 86_400_000;
const MINUTE: u64 = 60_000;

#[derive(Clone, Debug)]
pub struct ReplayOutcome {
    pub run_id: String,
    pub member: Option<String>,
    pub failure: Option<String>,
    pub wait_ms: Option<u64>,
    pub sync_ms: Option<u64>,
    pub run_ms: Option<u64>,
    pub terminal_at_ms: Option<u64>,
    pub charged_ms: Option<f64>,
    /// The trial's frozen uncontended execution reference, also used to charge
    /// the recorded fleet_busy outcome. This is not its contended recorded run.
    pub reference_ms: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct ReplayMinute {
    pub member: String,
    pub at_ms: u64,
    pub queued_max: usize,
    pub interactive_queued_max: usize,
    /// Total machine CPU share, including fixed independent outside load.
    pub cpu_mean: Option<f64>,
}

/// Choose the busiest finished UTC day by root interactive arrivals. Coverage is
/// checked afterwards: selecting a quieter day to avoid missing data is forbidden.
pub fn busiest_day(trace: &Trace) -> Option<(Timestamp, Timestamp)> {
    let mut counts = BTreeMap::<u64, usize>::new();
    for r in &trace.requests {
        let at = r.fact.requested_at.millis();
        let day = at / DAY * DAY;
        if !r.fact.nested
            && !r.fact.deferrable
            && day >= trace.since.millis()
            && day
                .checked_add(DAY)
                .is_some_and(|end| end <= trace.until.millis())
        {
            *counts.entry(day).or_default() += 1;
        }
    }
    let (day, _) = counts
        .into_iter()
        .max_by(|(a, an), (b, bn)| an.cmp(bn).then(b.cmp(a)))?;
    Some((
        Timestamp::from_millis(day),
        Timestamp::from_millis(day + DAY),
    ))
}

fn percentile(values: &[f64], q: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Some(sorted[((sorted.len() as f64 * q).ceil() as usize).saturating_sub(1)])
}
fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}
fn terminal(member: Option<&str>, failure: Option<&str>) -> bool {
    member.is_some()
        || matches!(
            failure,
            Some("fleet_busy" | "request_too_large" | "member_unavailable" | "config_sync_failed")
        )
}
fn externally_fixed(failure: Option<&str>) -> bool {
    failure.is_some_and(|code| {
        !matches!(
            code,
            "fleet_busy"
                | "request_too_large"
                | "member_unavailable"
                | "config_sync_failed"
                | "deadline_skipped"
        )
    })
}
fn observed_terminal_at(r: &Request) -> Option<u64> {
    r.fact
        .resources
        .as_ref()
        .and_then(|v| v.ended_at)
        .map(Timestamp::millis)
        .or_else(|| r.decision.as_ref().map(|d| d.decided_at.millis()))
}

fn relative(actual: Option<f64>, simulated: Option<f64>, tolerance: f64) -> Option<bool> {
    actual
        .zip(simulated)
        .map(|(a, s)| (a - s).abs() <= a.abs() * tolerance)
}
pub(super) fn charge(r: &Request, replay: Option<&ReplayOutcome>) -> Option<f64> {
    if r.fact.failure.as_deref() == Some("fleet_busy") {
        // waited_ms stops at admission and is absent for an unadmitted
        // refusal. A sync can also exhaust the hold after admission, so the
        // recorded terminal time is the comparable time until refusal.
        let refused_at = r
            .fact
            .resources
            .as_ref()
            .and_then(|resources| resources.ended_at)
            .or_else(|| {
                r.decision
                    .as_ref()
                    .filter(|decision| {
                        decision.verdict == crate::scheduling::record::Verdict::FleetBusy
                    })
                    .map(|decision| decision.decided_at)
            });
        let elapsed_ms = refused_at
            .map(|at| at.since(r.fact.requested_at).as_millis() as u64)
            .or(r.fact.waited_ms)?;
        Some(
            (elapsed_ms as f64).max(r.hold_limit?.seconds.as_secs_f64() * 1000.0)
                + replay?.reference_ms?,
        )
    } else if r.fact.member.is_some() {
        Some(r.fact.waited_ms? as f64 + r.fact.sync_ms.unwrap_or(0) as f64 + r.fact.run_ms? as f64)
    } else {
        None
    }
}

pub fn compare(
    trace: &Trace,
    since: Timestamp,
    until: Timestamp,
    outcomes: &[ReplayOutcome],
    minutes: &[ReplayMinute],
    model_blockers: &[String],
) -> Value {
    let start = since.millis();
    let end = until.millis();
    let mut blockers: BTreeSet<String> = model_blockers.iter().cloned().collect();
    if trace
        .placement_replay
        .as_ref()
        .is_some_and(|p| p.mode == crate::replay_placement::Mode::Current)
    {
        blockers.insert(
            "current placement is a counterfactual and cannot certify historical fidelity".into(),
        );
    }
    let rehearsal = trace.rehearsal.as_ref();
    if rehearsal.is_some_and(|r| !r.complete_for(trace)) {
        blockers.insert("rehearsal is incomplete, stopped, or missing terminal records".into());
    }
    if rehearsal.is_none()
        && (!start.is_multiple_of(DAY)
            || end.checked_sub(start) != Some(DAY)
            || start < trace.since.millis()
            || end > trace.until.millis())
    {
        blockers.insert("evaluation window is not a complete recorded UTC day".into());
    }
    if trace.incomplete {
        blockers.insert("trace.incomplete: fleet collection is incomplete".into());
    }
    // H1 member-filtered exports predate the explicit incomplete flag. Their
    // provenance still prevents a subset from certifying fleet-wide fidelity.
    if trace.limits.iter().any(|limit| {
        limit == "member-filtered arrivals: not a full-fleet replay; models and load retain fleet context"
    }) {
        blockers.insert("trace.limits: member-filtered arrivals omit fleet workload".into());
    }
    if trace.members.len() > 1 && trace.master.is_none() {
        blockers.insert("master identity missing for a multi-member fleet".into());
    }
    let population: Vec<_> = trace
        .requests
        .iter()
        .filter(|r| r.fact.requested_at >= since && r.fact.requested_at < until && !r.fact.nested)
        .collect();
    let expected: BTreeSet<_> = population.iter().map(|r| r.fact.run_id.as_str()).collect();
    let actual: BTreeMap<_, _> = outcomes.iter().map(|r| (r.run_id.as_str(), r)).collect();
    if actual.len() != outcomes.len() {
        blockers.insert("duplicate simulated run IDs".into());
    }
    // Causal history also teaches copy warmth and estimator state.
    for r in trace
        .requests
        .iter()
        .filter(|r| r.synthetic_workspace && r.fact.requested_at < until)
    {
        blockers.insert(format!(
            "requests[{}].synthetic rehearsal workspace is not historical evidence",
            r.fact.run_id
        ));
    }
    for r in &population {
        if r.fact.unfinished {
            blockers.insert(format!(
                "requests[{}].terminal outcome missing",
                r.fact.run_id
            ));
        }
        if r.fact.kind == Kind::Run
            && r.workspace.is_none()
            && (!r.workspace_recorded
                || r.fact
                    .member
                    .as_ref()
                    .is_some_and(|member| member != &r.fact.calling_member))
        {
            blockers.insert(format!("requests[{}].workspace identity unavailable (older recording or outside a working tree)",r.fact.run_id));
        }
        if r.fact.member.is_some() && r.fact.run_ms.is_none() {
            blockers.insert(format!("requests[{}].run_ms missing", r.fact.run_id));
        }
        if !actual.contains_key(r.fact.run_id.as_str()) {
            blockers.insert(format!(
                "requests[{}].simulated outcome missing",
                r.fact.run_id
            ));
        }
    }
    let invented: Vec<_> = actual
        .keys()
        .filter(|id| !expected.contains(**id))
        .copied()
        .collect();
    for m in &trace.members {
        for e in &m.errors {
            blockers.insert(format!("members[{}].collection: {}", m.member, e));
        }
        if !m.scheduler_settings.iter().any(|s| s.at <= since) {
            blockers.insert(format!(
                "members[{}].scheduler_settings at day start missing",
                m.member
            ));
        }
        let rows: BTreeMap<_, _> = m
            .load
            .iter()
            .filter(|b| b.at.millis() >= start / MINUTE * MINUTE && b.at < until)
            .map(|b| (b.at.millis(), b))
            .collect();
        let missing: Vec<_> = (start / MINUTE * MINUTE..end)
            .step_by(MINUTE as usize)
            .filter(|at| !rows.contains_key(at))
            .collect();
        if !missing.is_empty() {
            blockers.insert(format!(
                "members[{}].load: {} missing minutes (first {}ms)",
                m.member,
                missing.len(),
                missing[0]
            ));
        }
        for b in rows.values() {
            let expected_seconds = if rehearsal.is_some() {
                (end.min(b.at.millis() + MINUTE)
                    .saturating_sub(start.max(b.at.millis()))
                    / 1000)
                    .max(1)
            } else {
                60
            };
            if b.seconds * 10 < expected_seconds * 9
                || b.cpu_mean.is_none()
                || b.outside_mean.is_none()
                || b.speed_min.is_none()
                || b.speed_max.is_none()
            {
                blockers.insert(format!(
                    "members[{}].load[{}]: incomplete observations, outside load or speed",
                    m.member,
                    b.at.millis()
                ));
            }
        }
    }
    let mut placement_of = 0usize;
    let mut placement_equal = 0usize;
    let mut fixed_of = 0usize;
    let mut fixed_equal = 0usize;
    let mut refusals = Vec::new();
    let mut placement_outliers = Vec::new();
    let mut wait_errors = Vec::new();
    let mut recorded_waits = Vec::new();
    let mut wait_outliers = Vec::new();
    let mut sync_errors = Vec::new();
    let mut recorded_charges = Vec::new();
    let mut replay_charges = Vec::new();
    let mut missing_charges = Vec::new();
    let mut recorded_terminal = 0usize;
    let mut replay_terminal = 0usize;
    let mut recorded_busy = 0usize;
    let mut replay_busy = 0usize;
    let mut external_of = 0usize;
    let mut external_exact = 0usize;
    let mut external_outliers = Vec::new();
    let mut groups = BTreeMap::<(String, String), (Vec<f64>, Vec<f64>)>::new();
    let mut missing_outcomes = Vec::new();
    let mut missing_waits = Vec::new();
    for r in &population {
        let f = &r.fact;
        let s = actual.get(f.run_id.as_str()).copied();
        let charge_eligible = !f.deferrable
            && !externally_fixed(f.failure.as_deref())
            && (f.member.is_some() || f.failure.as_deref() == Some("fleet_busy"));
        if externally_fixed(f.failure.as_deref())
            || s.is_some_and(|o| externally_fixed(o.failure.as_deref()))
        {
            let expected_fixed = externally_fixed(f.failure.as_deref());
            external_of += usize::from(expected_fixed);
            let observed_end = observed_terminal_at(r);
            let simulated_end = s.and_then(|o| o.terminal_at_ms);
            let same = expected_fixed
                && s.is_some_and(|o| o.failure == f.failure && o.member == f.member)
                && observed_end.zip(simulated_end).is_some_and(|(a, b)| a == b);
            external_exact += usize::from(same);
            if expected_fixed && observed_end.is_none() {
                blockers.insert(format!(
                    "requests[{}].fixed failure terminal timestamp missing",
                    f.run_id
                ));
            }
            if !same {
                external_outliers.push(json!({"run_id":f.run_id,"recorded_failure":f.failure,"simulated_failure":s.and_then(|o|o.failure.as_deref()),"recorded_member":f.member,"simulated_member":s.and_then(|o|o.member.as_deref()),"recorded_terminal_at_ms":observed_end,"simulated_terminal_at_ms":simulated_end}));
            }
        }
        let family = f
            .family
            .clone()
            .or_else(|| f.label.clone())
            .unwrap_or_else(|| "<unlabelled>".into());
        // Recorded populations are fixed even when replay loses an outcome.
        // Shrinking these denominators could otherwise turn lost work into a pass.
        if f.member.is_some() {
            placement_of += 1;
            if !matches!(r.placement_request, Placement::Placeable) {
                fixed_of += 1;
            }
        }
        if let Some(wait) = f.waited_ms {
            recorded_waits.push(wait as f64);
        }
        if f.kind == Kind::Run
            && let Some((member, duration)) = f.member.as_ref().zip(f.run_ms)
        {
            groups
                .entry((member.clone(), family.clone()))
                .or_default()
                .0
                .push(duration as f64);
        }
        if !f.deferrable {
            recorded_terminal += usize::from(terminal(f.member.as_deref(), f.failure.as_deref()));
            recorded_busy += usize::from(f.failure.as_deref() == Some("fleet_busy"));
            if charge_eligible {
                match charge(r, s) {
                    Some(value) => recorded_charges.push(value),
                    None => missing_charges.push(f.run_id.clone()),
                }
            }
        }
        let Some(s) = s else {
            missing_outcomes.push(f.run_id.clone());
            refusals.push(json!({"run_id":f.run_id,"recorded":f.failure,"simulated":null,"simulated_outcome_missing":true}));
            if f.member.is_some() {
                placement_outliers.push(json!({"run_id":f.run_id,"recorded":f.member,"simulated":null,"simulated_outcome_missing":true}));
            }
            if f.waited_ms.is_some() || f.member.is_some() {
                missing_waits.push(f.run_id.clone());
            }
            if charge_eligible && !missing_charges.contains(&f.run_id) {
                missing_charges.push(f.run_id.clone());
            }
            continue;
        };
        if f.member.is_some() {
            let same = f.member == s.member;
            placement_equal += usize::from(same);
            if !matches!(r.placement_request, Placement::Placeable) {
                fixed_equal += usize::from(same);
            }
            if !same {
                placement_outliers
                    .push(json!({"run_id":f.run_id,"recorded":f.member,"simulated":s.member}));
            }
        }
        if f.failure != s.failure {
            refusals.push(json!({"run_id":f.run_id,"recorded":f.failure,"simulated":s.failure}));
        }
        if !f.deferrable {
            replay_terminal += usize::from(terminal(s.member.as_deref(), s.failure.as_deref()));
            replay_busy += usize::from(s.failure.as_deref() == Some("fleet_busy"));
            if charge_eligible {
                match s.charged_ms {
                    Some(value) => replay_charges.push(value),
                    None if !missing_charges.contains(&f.run_id) => {
                        missing_charges.push(f.run_id.clone())
                    }
                    None => (),
                }
            }
        }
        if let Some((a, b)) = f.waited_ms.zip(s.wait_ms) {
            let error = a.abs_diff(b) as f64;
            wait_errors.push(error);
            let tolerance = 5000.0f64.max(a as f64 * 0.1);
            if error > tolerance {
                wait_outliers.push(json!({"run_id":f.run_id,"recorded_ms":a,"simulated_ms":b,"absolute_error_ms":error,"limit_ms":tolerance}));
            }
        } else if f.member.is_some() {
            missing_waits.push(f.run_id.clone());
            blockers.insert(format!("requests[{}].wait comparison missing", f.run_id));
        }
        if let Some((a, b)) = f.sync_ms.zip(s.sync_ms) {
            sync_errors.push(json!({"run_id":f.run_id,"recorded_ms":a,"simulated_ms":b,"absolute_error_ms":a.abs_diff(b)}));
        }
        if f.kind == Kind::Run
            && let Some((member, duration)) = s.member.as_ref().zip(s.run_ms)
        {
            groups
                .entry((member.clone(), family))
                .or_default()
                .1
                .push(duration as f64);
        }
    }
    for id in &missing_charges {
        blockers.insert(format!("requests[{id}].charged reference/duration missing"));
    }
    let median_error = median(wait_errors.clone());
    let p95_error = percentile(&wait_errors, 0.95);
    let p95_limit = 2000.0f64.max(percentile(&recorded_waits, 0.95).unwrap_or(0.0) * 0.05);
    let waits_pass = median_error.is_some_and(|x| x <= 1000.0)
        && p95_error.is_some_and(|x| x <= p95_limit)
        && wait_outliers.is_empty()
        && missing_waits.is_empty();
    let mut duration_groups = Vec::new();
    let mut durations_pass = true;
    for ((member, family), (a, b)) in groups {
        let median_a = median(a.clone());
        let median_b = median(b.clone());
        let p95_a = percentile(&a, 0.95);
        let p95_b = percentile(&b, 0.95);
        let passed = relative(median_a, median_b, 0.1) == Some(true)
            && relative(p95_a, p95_b, 0.1) == Some(true);
        if a.len() >= 5 {
            durations_pass &= passed;
        }
        duration_groups.push(json!({"member":member,"family":family,"count":a.len(),"simulated_count":b.len(),"status":if a.len()<5 {"insufficient_group_size"} else if passed {"pass"} else {"fail"},"recorded_median_ms":median_a,"simulated_median_ms":median_b,"recorded_p95_ms":p95_a,"simulated_p95_ms":p95_b,"relative_tolerance":0.1}));
    }
    let mut queue_reports = Vec::new();
    let mut queues_pass = true;
    let mut utilisation = Vec::new();
    let simulated: BTreeMap<_, _> = minutes
        .iter()
        .map(|m| ((m.member.as_str(), m.at_ms), m))
        .collect();
    for m in &trace.members {
        let rows: Vec<_> = m
            .load
            .iter()
            .filter(|b| b.at.millis() >= start / MINUTE * MINUTE && b.at < until)
            .collect();
        for interactive in [false, true] {
            let mut equal = 0;
            let mut within_tolerance = 0;
            let mut of = 0;
            let mut missing = 0;
            let mut max_error = 0;
            let mut peak_a = 0;
            let mut peak_b = 0;
            let mut invented_idle = Vec::new();
            let mut outliers = Vec::new();
            let mut timeline = Vec::new();
            for b in &rows {
                let Some(a) = (if interactive {
                    b.interactive_queued_max
                } else {
                    Some(b.queued_max)
                }) else {
                    continue;
                };
                of += 1;
                peak_a = peak_a.max(a);
                let Some(s) = simulated.get(&(m.member.as_str(), b.at.millis())) else {
                    missing += 1;
                    continue;
                };
                let value = if interactive {
                    s.interactive_queued_max
                } else {
                    s.queued_max
                };
                peak_b = peak_b.max(value);
                let error = a.abs_diff(value);
                equal += usize::from(error == 0);
                within_tolerance += usize::from(error <= 1);
                max_error = max_error.max(error);
                let point = json!({"at":b.at,"recorded":a,"simulated":value});
                if error > 1 {
                    outliers.push(point.clone());
                }
                // The strict zero-invention rule applies only to a fully
                // observed idle minute. Busy minutes retain the contract's
                // one-request tolerance for minute sampling.
                if a == 0
                    && b.seconds == 60
                    && b.busy_seconds == 0
                    && b.in_use_max == crate::scheduling::capacity::Cores::ZERO
                    && value > 0
                {
                    invented_idle.push(point.clone());
                }
                timeline.push(point);
            }
            let passed = of > 0
                && missing == 0
                && within_tolerance == of
                && max_error <= 1
                && peak_a.abs_diff(peak_b) <= 1
                && invented_idle.is_empty();
            if of > 0 || !interactive {
                queues_pass &= passed;
            }
            queue_reports.push(json!({"member":m.member,"interactive":interactive,"status":if interactive&&of==0{"unavailable"}else if passed{"pass"}else{"fail"},"exact_count":equal,"within_tolerance_count":within_tolerance,"minimum_matching_count":of,"per_minute_tolerance":1,"of":of,"missing_simulated_minutes":missing,"maximum_error":max_error,"recorded_peak":peak_a,"simulated_peak":peak_b,"outliers":outliers,"invented_idle_queues":invented_idle,"timeline":timeline}));
        }
        let mut a = 0.0;
        let mut b = 0.0;
        let mut seconds = 0u64;
        let mut timeline = Vec::new();
        for row in &rows {
            if let Some((av, bv)) = row.cpu_mean.zip(
                simulated
                    .get(&(m.member.as_str(), row.at.millis()))
                    .and_then(|m| m.cpu_mean),
            ) {
                a += av * row.seconds as f64;
                b += bv * row.seconds as f64;
                seconds += row.seconds;
                timeline.push(json!({"at":row.at,"recorded":av,"simulated":bv,"absolute_error":(av-bv).abs()}));
            }
        }
        utilisation.push(json!({"member":m.member,"status":"unspecified_tolerance","matched_seconds":seconds,"recorded_cpu_mean":(seconds>0).then(||a/seconds as f64),"simulated_cpu_mean":(seconds>0).then(||b/seconds as f64),"timeline":timeline}));
    }
    let actual_mean = mean(&recorded_charges);
    let simulated_mean = mean(&replay_charges);
    let objective_pass =
        missing_charges.is_empty() && relative(actual_mean, simulated_mean, 0.05) == Some(true);
    let placements_pass =
        placement_of > 0 && placement_equal * 100 >= placement_of * 99 && fixed_equal == fixed_of;
    let refusals_pass =
        refusals.is_empty() && recorded_terminal == replay_terminal && invented.is_empty();
    let external_pass = external_outliers.is_empty() && external_of == external_exact;
    let passed = external_pass
        && placements_pass
        && refusals_pass
        && waits_pass
        && queues_pass
        && objective_pass
        && durations_pass;
    json!({"status":if !blockers.is_empty(){"blocked_by_missing_evidence"}else if passed{"pass"}else{"fail"},"trusted_for_tuning":blockers.is_empty()&&passed,"since":since,"until":until,"blockers":blockers,
        "placements":{"status":if placements_pass{"pass"}else{"fail"},"exact_count":placement_equal,"of":placement_of,"minimum_fraction":0.99,"pinned_required_exact_count":fixed_equal,"pinned_required_of":fixed_of,"outliers":placement_outliers},
        "terminal_verdicts":{"status":if refusals_pass{"pass"}else{"fail"},"recorded_count":recorded_terminal,"simulated_count":replay_terminal,"outliers":refusals,"invented_run_ids":invented,"missing_simulated_run_ids":missing_outcomes},
        "cancelled_and_infrastructure":{"status":if external_pass{"pass"}else{"fail"},"recorded_count":external_of,"exact_count":external_exact,"outliers":external_outliers},
        "fleet_busy":{"status":if recorded_busy==replay_busy&&recorded_terminal==replay_terminal{"pass"}else{"fail"},"recorded_count":recorded_busy,"recorded_of":recorded_terminal,"simulated_count":replay_busy,"simulated_of":replay_terminal},
        "waits":{"status":if waits_pass{"pass"}else{"fail"},"count":wait_errors.len(),"recorded_count":recorded_waits.len(),"missing_simulated_run_ids":missing_waits,"median_absolute_error_ms":median_error,"median_limit_ms":1000,"p95_absolute_error_ms":p95_error,"p95_limit_ms":p95_limit,"maximum_absolute_error_ms":percentile(&wait_errors,1.0),"outliers":wait_outliers},
        "sync":{"status":"reported_separately","requests":sync_errors},"queues":queue_reports,
        "charged_mean":{"status":if objective_pass{"pass"}else{"fail"},"count":recorded_charges.len(),"simulated_count":replay_charges.len(),"recorded_ms":actual_mean,"simulated_ms":simulated_mean,"relative_tolerance":0.05,"unscorable_run_ids":missing_charges},
        "run_durations":{"status":if durations_pass{"pass"}else{"fail"},"groups":duration_groups},"utilisation":utilisation,
        "questions":["The contract gives no numeric per-member utilisation tolerance; values are reported without inventing a pass threshold."]})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reporting::fixtures,
        scheduling::{record::WorkspaceIdentity, records::SchedulerSettings},
        workload::trace::{Member, VERSION},
    };

    fn fixture(count: usize) -> (Trace, Vec<ReplayOutcome>, Vec<ReplayMinute>) {
        let start = DAY * 20_000;
        let mut requests = Vec::new();
        let mut outcomes = Vec::new();
        for i in 0..count {
            let mut record = fixtures::pinned(i as u64 + 1, "test", "mini", 10.0);
            record.requested_at = Timestamp::from_millis(start + i as u64 * 1000);
            record.started_at = Some(Timestamp::from_millis(start + i as u64 * 1000 + 20_000));
            record.ended_at = Some(Timestamp::from_millis(start + i as u64 * 1000 + 30_000));
            record.decision.as_mut().unwrap().decided_at = record.started_at.unwrap();
            record.placement = Placement::Placeable;
            record.waited_ms = Some(20_000);
            record.run_ms = Some(10_000);
            record.workspace = Some(WorkspaceIdentity {
                workspace: "a".repeat(64),
                repository: "b".repeat(64),
            });
            let r = Request::of(&record).unwrap();
            outcomes.push(ReplayOutcome {
                run_id: r.fact.run_id.clone(),
                member: r.fact.member.clone(),
                failure: r.fact.failure.clone(),
                wait_ms: Some(20_000),
                sync_ms: r.fact.sync_ms,
                run_ms: Some(10_000),
                terminal_at_ms: record.ended_at.map(Timestamp::millis),
                charged_ms: Some(30_000.0),
                reference_ms: Some(10_000.0),
            });
            requests.push(r);
        }
        let mut load = Vec::new();
        let mut minutes = Vec::new();
        for i in 0..1440 {
            let mut b = fixtures::bucket(0, 0.2, 0.0, 0, 0, 1.0);
            b.at = Timestamp::from_millis(start + i * MINUTE);
            minutes.push(ReplayMinute {
                member: "mini".into(),
                at_ms: b.at.millis(),
                queued_max: 0,
                interactive_queued_max: 0,
                cpu_mean: Some(0.2),
            });
            load.push(b);
        }
        let cap = fixtures::candidate("mini", true, None).capacity;
        (
            Trace {
                rehearsal: None,
                placement_replay: None,
                trace_version: VERSION,
                incomplete: false,
                since: Timestamp::from_millis(start),
                until: Timestamp::from_millis(start + DAY),
                master: Some("mini".into()),
                members: vec![Member {
                    member: "mini".into(),
                    load,
                    errors: vec![],
                    scheduler_settings: vec![SchedulerSettings {
                        at: Timestamp::from_millis(start),
                        capacity: cap,
                        parameters: Default::default(),
                    }],
                }],
                requests,
                leftovers: vec![],
                demand: vec![],
                contention: vec![],
                limits: vec![],
            },
            outcomes,
            minutes,
        )
    }
    fn check(t: &Trace, o: &[ReplayOutcome], m: &[ReplayMinute]) -> Value {
        compare(t, t.since, t.until, o, m, &[])
    }

    #[test]
    fn synthetic_workload_identities_cannot_certify_real_history() {
        let (mut t, o, m) = fixture(1);
        t.requests[0].synthetic_workspace = true;
        let v = check(&t, &o, &m);
        assert_eq!(v["status"], "blocked_by_missing_evidence");
        assert!(v["blockers"].as_array().unwrap().iter().any(|b| {
            b.as_str()
                .unwrap()
                .contains("synthetic rehearsal workspace")
        }));
        t.requests[0].synthetic_workspace = false;
        let mut prior = t.requests[0].clone();
        prior.fact.run_id = "r-000000000002".into();
        prior.fact.requested_at = Timestamp::from_millis(t.since.millis() - 1);
        prior.synthetic_workspace = true;
        t.requests.push(prior);
        assert_eq!(check(&t, &o, &m)["status"], "blocked_by_missing_evidence");
    }
    #[test]
    fn recorded_terminal_results_after_the_arrival_window_are_compared() {
        let (mut t, mut o, m) = fixture(1);
        let arrival = t.until.millis() - 10_000;
        t.requests[0].fact.requested_at = Timestamp::from_millis(arrival);
        let resources = t.requests[0].fact.resources.as_mut().unwrap();
        resources.started_at = Some(Timestamp::from_millis(arrival + 20_000));
        resources.ended_at = Some(Timestamp::from_millis(arrival + 30_000));
        o[0].terminal_at_ms = Some(arrival + 30_000);
        let v = check(&t, &o, &m);
        assert_eq!(v["status"], "pass", "{v}");
        o[0].run_ms = Some(50_000);
        o[0].charged_ms = Some(70_000.0);
        assert_eq!(check(&t, &o, &m)["status"], "fail");
    }
    #[test]
    fn exact_recorded_day_passes_with_every_gate_and_publishes_counts() {
        let (t, o, m) = fixture(100);
        let v = check(&t, &o, &m);
        assert_eq!(v["status"], "pass");
        assert_eq!(v["placements"]["exact_count"], 100);
        assert_eq!(v["waits"]["count"], 100);
        assert_eq!(v["charged_mean"]["recorded_ms"], 30_000.0);
        assert_eq!(v["queues"][0]["exact_count"], 1440);
        assert_eq!(v["utilisation"][0]["status"], "unspecified_tolerance");
    }
    #[test]
    fn placement_threshold_is_exact_and_required_requests_are_strict() {
        let (mut t, mut o, m) = fixture(100);
        o[0].member = Some("devbox".into());
        assert_eq!(check(&t, &o, &m)["placements"]["status"], "pass");
        t.requests[0].placement_request = Placement::Member("mini".into());
        let v = check(&t, &o, &m);
        assert_eq!(v["placements"]["status"], "fail");
        assert_eq!(v["placements"]["pinned_required_of"], 1);
        t.requests[0].placement_request = Placement::Placeable;
        o[1].member = Some("devbox".into());
        assert_eq!(check(&t, &o, &m)["placements"]["status"], "fail");
    }
    #[test]
    fn wait_outlier_is_not_rounded_away_and_population_percentiles_stay_fixed() {
        let (t, mut o, m) = fixture(100);
        o[0].wait_ms = Some(25_001);
        let v = check(&t, &o, &m);
        assert_eq!(v["waits"]["status"], "fail");
        assert_eq!(v["waits"]["outliers"][0]["absolute_error_ms"], 5001.0);
        assert_eq!(v["waits"]["median_absolute_error_ms"], 0.0);
        assert_eq!(v["waits"]["p95_limit_ms"], 2000.0);
    }
    #[test]
    fn even_populations_use_the_mean_of_middle_values_for_the_median_gate() {
        let (t, mut outcomes, minutes) = fixture(2);
        outcomes[0].wait_ms = Some(20_900);
        outcomes[1].wait_ms = Some(21_900);
        let report = check(&t, &outcomes, &minutes);
        assert_eq!(report["waits"]["median_absolute_error_ms"], 1400.0);
        assert_eq!(report["waits"]["p95_absolute_error_ms"], 1900.0);
        assert_eq!(report["waits"]["status"], "fail");

        let (mut t, mut outcomes, minutes) = fixture(6);
        for (request, duration) in t
            .requests
            .iter_mut()
            .zip([1000, 1000, 1000, 2000, 4000, 5000])
        {
            request.fact.run_ms = Some(duration);
        }
        for (outcome, duration) in outcomes
            .iter_mut()
            .zip([1000, 1000, 1000, 3000, 4000, 5000])
        {
            outcome.run_ms = Some(duration);
        }
        let report = check(&t, &outcomes, &minutes);
        let group = &report["run_durations"]["groups"][0];
        assert_eq!(group["recorded_median_ms"], 1500.0);
        assert_eq!(group["simulated_median_ms"], 2000.0);
        assert_eq!(group["recorded_p95_ms"], group["simulated_p95_ms"]);
        assert_eq!(group["status"], "fail");
    }

    #[test]
    fn idle_queue_invention_fails_even_within_one_request_tolerance() {
        let (t, o, mut m) = fixture(5);
        m[0].queued_max = 1;
        let v = check(&t, &o, &m);
        assert_eq!(v["queues"][0]["status"], "fail");
        assert_eq!(v["queues"][0]["maximum_error"], 1);
        assert_eq!(
            v["queues"][0]["invented_idle_queues"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn busy_minutes_keep_the_one_request_sampling_tolerance() {
        let (mut trace, outcomes, mut minutes) = fixture(5);
        trace.members[0].load[0].busy_seconds = 60;
        trace.members[0].load[0].in_use_max = crate::scheduling::capacity::Cores::ONE;
        minutes[0].queued_max = 1;
        let report = check(&trace, &outcomes, &minutes);
        assert_eq!(report["queues"][0]["status"], "pass");
        assert_eq!(report["queues"][0]["maximum_error"], 1);
        assert_eq!(report["queues"][0]["invented_idle_queues"], json!([]));
        assert_eq!(report["status"], "pass");
    }

    #[test]
    fn short_queue_regressions_keep_sampling_tolerance_and_gross_error_bounds() {
        let regressions: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/queue-fidelity-regressions.json"
        ))
        .unwrap();
        for (name, expected) in [("run8", "pass"), ("run5", "fail")] {
            let (mut trace, outcomes, mut minutes) = fixture(5);
            let timeline = regressions[name]["timeline"].as_array().unwrap();
            trace.members[0].load.truncate(timeline.len());
            minutes.truncate(timeline.len());
            for ((load, minute), point) in trace.members[0]
                .load
                .iter_mut()
                .zip(&mut minutes)
                .zip(timeline)
            {
                load.queued_max = point["recorded"].as_u64().unwrap() as usize;
                load.interactive_queued_max = Some(load.queued_max);
                load.busy_seconds = 60;
                load.in_use_max = crate::scheduling::capacity::Cores::ONE;
                minute.queued_max = point["simulated"].as_u64().unwrap() as usize;
                minute.interactive_queued_max = minute.queued_max;
            }
            let report = check(&trace, &outcomes, &minutes);
            for queue in report["queues"].as_array().unwrap() {
                assert_eq!(queue["status"], expected, "{name}: {queue}");
                assert_eq!(queue["minimum_matching_count"], timeline.len());
                if name == "run8" {
                    assert_eq!(queue["exact_count"], 4);
                    assert_eq!(queue["within_tolerance_count"], 6);
                    assert_eq!(queue["maximum_error"], 1);
                    assert_eq!(queue["recorded_peak"], 6);
                    assert_eq!(queue["simulated_peak"], 6);
                } else {
                    assert_eq!(queue["recorded_peak"], 16);
                    assert_eq!(queue["simulated_peak"], 4);
                }
            }
            // Missing minutes and errors of two requests still fail on a short sample.
            if name == "run8" {
                minutes.pop();
                assert_eq!(
                    check(&trace, &outcomes, &minutes)["queues"][0]["status"],
                    "fail"
                );
                minutes[0].queued_max = 3;
                assert_eq!(
                    check(&trace, &outcomes, &minutes)["queues"][0]["maximum_error"],
                    2
                );
            }
        }
    }

    #[test]
    fn missing_historical_evidence_blocks_even_when_computed_metrics_pass() {
        let (mut t, o, m) = fixture(5);
        t.members[0].scheduler_settings.clear();
        t.members[0].load.remove(5);
        t.requests[0].workspace = None;
        t.requests[0].workspace_recorded = false;
        let v = check(&t, &o, &m);
        assert_eq!(v["status"], "blocked_by_missing_evidence");
        assert_eq!(v["trusted_for_tuning"], false);
        let blockers = v["blockers"].as_array().unwrap();
        assert_eq!(blockers.len(), 3);
        assert_eq!(v["placements"]["status"], "pass");
    }
    #[test]
    fn legacy_member_filtered_provenance_blocks_fleet_fidelity_even_when_metrics_match() {
        let (mut trace, outcomes, minutes) = fixture(5);
        trace.incomplete = false;
        trace.limits.push("member-filtered arrivals: not a full-fleet replay; models and load retain fleet context".into());
        let report = check(&trace, &outcomes, &minutes);
        assert_eq!(report["placements"]["status"], "pass");
        assert_eq!(report["waits"]["status"], "pass");
        assert_eq!(report["status"], "blocked_by_missing_evidence");
        assert_eq!(report["trusted_for_tuning"], false);
        assert!(
            report["blockers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason.as_str().unwrap().contains("member-filtered"))
        );
    }
    #[test]
    fn busiest_finished_day_is_selected_before_coverage_checks() {
        let (mut t, _, _) = fixture(3);
        let original = t.since;
        t.until = Timestamp::from_millis(t.until.millis() + DAY + 2000);
        let mut next = t.requests[0].clone();
        next.fact.requested_at = Timestamp::from_millis(original.millis() + DAY + 1000);
        t.requests.push(next);
        assert_eq!(
            busiest_day(&t),
            Some((original, Timestamp::from_millis(original.millis() + DAY)))
        );
    }
    #[test]
    fn busy_charge_keeps_recorded_hold_floor_and_refusal_codes_are_exact() {
        use crate::scheduling::record::{HoldBasis, HoldLimitRecord};
        let (mut t, mut o, m) = fixture(5);
        let r = &mut t.requests[0];
        r.fact.member = None;
        r.fact.failure = Some("fleet_busy".into());
        r.hold_limit = Some(HoldLimitRecord {
            seconds: std::time::Duration::from_secs(60),
            basis: HoldBasis::Requested,
            expected_run_seconds: None,
        });
        o[0].member = None;
        o[0].failure = Some("fleet_busy".into());
        o[0].run_ms = None;
        o[0].charged_ms = Some(70_000.0);
        let v = check(&t, &o, &m);
        assert_eq!(v["charged_mean"]["status"], "pass");
        assert_eq!(v["charged_mean"]["recorded_ms"], 38_000.0);
        assert_eq!(v["fleet_busy"]["recorded_count"], 1);
        o[0].failure = Some("request_too_large".into());
        let v = check(&t, &o, &m);
        assert_eq!(v["terminal_verdicts"]["status"], "fail");
        assert_eq!(v["fleet_busy"]["status"], "fail");
    }
    #[test]
    fn busy_charge_uses_refusal_terminal_time_without_an_admission_wait() {
        use crate::scheduling::record::{HoldBasis, HoldLimitRecord, Verdict};
        let (mut trace, mut outcomes, minutes) = fixture(5);
        let request = &mut trace.requests[0];
        request.fact.member = None;
        request.fact.failure = Some("fleet_busy".into());
        request.fact.waited_ms = None;
        request.fact.run_ms = None;
        request.hold_limit = Some(HoldLimitRecord {
            seconds: std::time::Duration::from_secs(60),
            basis: HoldBasis::Requested,
            expected_run_seconds: None,
        });
        let refused_at = request
            .fact
            .requested_at
            .after(std::time::Duration::from_secs(90));
        let resources = request.fact.resources.as_mut().unwrap();
        resources.started_at = None;
        resources.ended_at = Some(refused_at);
        outcomes[0].member = None;
        outcomes[0].failure = Some("fleet_busy".into());
        outcomes[0].wait_ms = Some(90_000);
        outcomes[0].run_ms = None;
        outcomes[0].terminal_at_ms = Some(refused_at.millis());
        outcomes[0].charged_ms = Some(100_000.0);
        assert_eq!(charge(request, Some(&outcomes[0])), Some(100_000.0));
        let report = check(&trace, &outcomes, &minutes);
        assert_eq!(report["charged_mean"]["recorded_ms"], 44_000.0);
        assert_eq!(report["charged_mean"]["status"], "pass");

        let request = &mut trace.requests[0];
        request.fact.resources.as_mut().unwrap().ended_at = None;
        let decision = request.decision.as_mut().unwrap();
        decision.verdict = Verdict::FleetBusy;
        decision.decided_at = refused_at;
        assert_eq!(charge(request, Some(&outcomes[0])), Some(100_000.0));
        request.decision.as_mut().unwrap().verdict = Verdict::Started;
        request.fact.waited_ms = Some(20_000);
        assert_eq!(charge(request, Some(&outcomes[0])), Some(70_000.0));
        outcomes[0].reference_ms = None;
        assert_eq!(charge(request, Some(&outcomes[0])), None);
    }
    #[test]
    fn sync_deadline_busy_charge_includes_time_after_admission() {
        use crate::scheduling::record::{HoldBasis, HoldLimitRecord};
        let (mut trace, mut outcomes, minutes) = fixture(5);
        let request = &mut trace.requests[0];
        request.fact.failure = Some("fleet_busy".into());
        request.fact.waited_ms = Some(20_000);
        request.fact.sync_ms = Some(65_000);
        request.fact.run_ms = Some(0);
        request.hold_limit = Some(HoldLimitRecord {
            seconds: std::time::Duration::from_secs(60),
            basis: HoldBasis::Requested,
            expected_run_seconds: None,
        });
        let refused_at = request
            .fact
            .requested_at
            .after(std::time::Duration::from_secs(85));
        request.fact.resources.as_mut().unwrap().ended_at = Some(refused_at);
        outcomes[0].failure = Some("fleet_busy".into());
        outcomes[0].sync_ms = Some(65_000);
        outcomes[0].run_ms = Some(0);
        outcomes[0].terminal_at_ms = Some(refused_at.millis());
        outcomes[0].charged_ms = Some(95_000.0);
        assert_eq!(charge(request, Some(&outcomes[0])), Some(95_000.0));
        let report = check(&trace, &outcomes, &minutes);
        assert_eq!(report["charged_mean"]["recorded_ms"], 43_000.0);
        assert_eq!(report["charged_mean"]["status"], "pass");
    }

    #[test]
    fn charged_objective_tolerance_is_inclusive_but_never_rounded() {
        let (t, mut o, m) = fixture(5);
        for r in &mut o {
            r.charged_ms = Some(31_500.0);
        }
        assert_eq!(check(&t, &o, &m)["charged_mean"]["status"], "pass");
        for r in &mut o {
            r.charged_ms = Some(31_500.01);
        }
        assert_eq!(check(&t, &o, &m)["charged_mean"]["status"], "fail");
    }
    #[test]
    fn missing_outcomes_cannot_shrink_recorded_denominators_or_pass_metrics() {
        let (mut t, mut o, m) = fixture(5);
        o.remove(0);
        let v = check(&t, &o, &m);
        assert_eq!(v["placements"]["of"], 5);
        assert_eq!(v["placements"]["exact_count"], 4);
        assert_eq!(v["placements"]["status"], "fail");
        assert_eq!(v["terminal_verdicts"]["recorded_count"], 5);
        assert_eq!(v["terminal_verdicts"]["simulated_count"], 4);
        assert_eq!(v["terminal_verdicts"]["status"], "fail");
        assert_eq!(v["waits"]["recorded_count"], 5);
        assert_eq!(v["waits"]["status"], "fail");
        assert_eq!(v["charged_mean"]["count"], 5);
        assert_eq!(v["charged_mean"]["simulated_count"], 4);
        assert_eq!(v["charged_mean"]["status"], "fail");
        t.requests[0].fact.member = None;
        t.requests[0].fact.failure = Some("fleet_busy".into());
        let v = check(&t, &o, &m);
        assert_eq!(v["fleet_busy"]["recorded_count"], 1);
        assert_eq!(v["fleet_busy"]["recorded_of"], 5);
        assert_eq!(v["fleet_busy"]["simulated_count"], 0);
        assert_eq!(v["fleet_busy"]["status"], "fail");
    }
    #[test]
    fn fixed_failures_are_excluded_from_score_but_require_exact_identity_and_terminal() {
        let (mut t, mut o, m) = fixture(5);
        t.requests[0].fact.failure = Some("cancelled".into());
        t.requests[0].fact.completed = false;
        t.requests[0].outcome = Some(crate::workload::trace::TraceOutcome::Cancelled);
        o[0].failure = Some("cancelled".into());
        o[0].charged_ms = None;
        let v = check(&t, &o, &m);
        assert_eq!(v["charged_mean"]["count"], 4);
        assert_eq!(v["charged_mean"]["status"], "pass");
        assert_eq!(v["cancelled_and_infrastructure"]["recorded_count"], 1);
        assert_eq!(v["cancelled_and_infrastructure"]["exact_count"], 1);
        assert_eq!(v["cancelled_and_infrastructure"]["status"], "pass");
        o[0].terminal_at_ms = o[0].terminal_at_ms.map(|at| at + 1);
        let v = check(&t, &o, &m);
        assert_eq!(v["cancelled_and_infrastructure"]["status"], "fail");
        assert_eq!(v["status"], "fail");
        o[0].failure = Some("connection_lost".into());
        assert_eq!(
            check(&t, &o, &m)["cancelled_and_infrastructure"]["status"],
            "fail"
        );
    }

    #[test]
    fn nonzero_command_exit_is_scored_as_admitted_work() {
        let (mut t, o, m) = fixture(5);
        t.requests[0].outcome =
            Some(crate::workload::trace::TraceOutcome::Exited { exit_code: 17 });
        let v = check(&t, &o, &m);
        assert_eq!(v["charged_mean"]["count"], 5);
        assert_eq!(v["charged_mean"]["status"], "pass");
        assert_eq!(v["cancelled_and_infrastructure"]["recorded_count"], 0);
    }
    #[test]
    fn known_outside_working_tree_is_valid_but_legacy_absence_blocks_fidelity() {
        let (mut t, o, m) = fixture(5);
        t.requests[0].workspace = None;
        t.requests[0].workspace_recorded = true;
        assert_eq!(check(&t, &o, &m)["status"], "pass");
        t.requests[0].workspace_recorded = false;
        let v = check(&t, &o, &m);
        assert_eq!(v["status"], "blocked_by_missing_evidence");
        assert!(
            v["blockers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b.as_str().unwrap().contains("workspace"))
        );
    }
}
