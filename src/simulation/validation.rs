//! Validate the on-disk trace at the simulator boundary. Missing historical
//! evidence is allowed; the fidelity gate identifies it without inventing data.
use crate::{
    failure::{Failure, Result},
    reporting::facts::{PlacementKind, is_code},
    scheduling::{
        capacity::Capacity,
        request::{Kind, MAXIMUM_DEADLINE, MAXIMUM_HOLD_LIMIT, Placement, Urgency},
        run_id::RunId,
        speed::PLAUSIBLE,
    },
    workload::trace::{Trace, TraceOutcome, VERSION},
};
use std::collections::{BTreeMap, BTreeSet};

const DAY: u64 = 86_400_000;
const MAXIMUM_SPAN: u64 = 30 * DAY;
const MAXIMUM_TIMESTAMP: u64 = 253_402_300_799_000;
const MAXIMUM_CORES: f64 = 1_000_000.0;
const MAXIMUM_REQUESTS: usize = 50_000;
const MAXIMUM_WORK: usize = 100_000_000;

fn bad(what: &str) -> Failure {
    Failure::new(
        "invalid_trace",
        format!("Invalid workload trace: {what}."),
        "Export a supported trace; missing historical evidence is reported by fidelity separately.",
    )
}
fn text(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.chars().count() <= maximum && !value.chars().any(char::is_control)
}
fn name(value: &str) -> bool {
    value.len() <= 255
        && !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}
fn cores(value: f64) -> bool {
    value.is_finite() && (0.0..=MAXIMUM_CORES).contains(&value)
}
fn share(value: Option<f64>) -> bool {
    value.is_none_or(|v| v.is_finite() && (0.0..=1.0).contains(&v))
}
fn capacity(value: Capacity) -> bool {
    value.weighted_cores.millis() > 0
        && value.capacity == value.weighted_cores.saturating_sub(value.reserved)
}
fn id(value: &str) -> bool {
    RunId::parse(value).is_some()
}
fn finite_json(value: &serde_json::Value, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    match value {
        serde_json::Value::Number(n) => n
            .as_f64()
            .is_some_and(|v| v.is_finite() && (0.0..=1e15).contains(&v)),
        serde_json::Value::Array(a) => {
            a.len() <= 4096 && a.iter().all(|v| finite_json(v, depth + 1))
        }
        serde_json::Value::Object(o) => {
            o.len() <= 128 && o.values().all(|v| finite_json(v, depth + 1))
        }
        serde_json::Value::String(s) => s.len() <= 1024 && !s.chars().any(char::is_control),
        _ => true,
    }
}

pub fn trace(t: &Trace) -> Result<()> {
    // Check the major before considering any optional model or historical data.
    if t.trace_version != VERSION {
        return Err(Failure::new(
            "unsupported_trace_version",
            "Unsupported workload trace version.",
            "Use the Elsecall version that supports this trace's major version.",
        ));
    }
    let start = t.since.millis();
    let end = t.until.millis();
    if end > MAXIMUM_TIMESTAMP || end <= start || end - start > MAXIMUM_SPAN {
        return Err(bad("window must be positive and at most 30 days"));
    }
    if t.members.is_empty()
        || t.members.len() > 64
        || t.requests.len() > MAXIMUM_REQUESTS
        || t.leftovers.len() > MAXIMUM_REQUESTS
        || t.demand.len() > MAXIMUM_REQUESTS
        || t.contention.len() > MAXIMUM_REQUESTS
    {
        return Err(bad("fleet or workload collection exceeds supported bounds"));
    }
    let members: BTreeSet<_> = t.members.iter().map(|m| m.member.as_str()).collect();
    if members.len() != t.members.len()
        || members.iter().any(|m| !name(m))
        || t.master.as_deref().is_some_and(|m| !members.contains(m))
    {
        return Err(bad(
            "member roster must be unique and contain its named master",
        ));
    }
    if let Some(r) = &t.rehearsal
        && (!crate::rehearsal::protocol::valid_session(&r.session)
            || !(1.0..=1000.0).contains(&r.compress)
            || r.calibration.len() > 64
            || r.calibration
                .iter()
                .any(|(m, c)| !members.contains(m.as_str()) || !c.valid())
            || r.ideal_ms.len() > 10_000
            || r.ideal_ms.iter().any(|(id, ms)| {
                !r.demand.contains_key(id)
                    || ms.is_some_and(|v| !v.is_finite() || v <= 0.0 || v > 3_600_000_000.0)
            })
            || r.uncontended_ms.len() > 10_000
            || r.uncontended_ms.iter().any(|(id, rates)| {
                !r.demand.contains_key(id)
                    || rates.is_empty()
                    || rates.len() > 64
                    || rates.iter().any(|(m, v)| {
                        !r.demand[id].contains_key(m)
                            || !v.is_finite()
                            || *v <= 0.0
                            || *v > 3_600_000_000.0
                    })
            })
            || r.work_iterations.len() > 10_000
            || r.work_iterations
                .iter()
                .any(|(id, w)| !r.demand.contains_key(id) || *w > 1_000_000_000_000_000)
            || r.planned > 10_000
            || r.load_complete.len() > 64
            || r.load_complete
                .keys()
                .any(|m| !members.contains(m.as_str()))
            || r.charge_floor_ms.len() > 10_000
            || r.charge_floor_ms
                .iter()
                .any(|(id, ms)| *ms > 86_400_000 || !r.demand.contains_key(id))
            || r.duration_ms.keys().any(|id| !r.demand.contains_key(id))
            || r.duration_ms.len() > 10_000
            || r.duration_ms
                .values()
                .any(|ms| !(1..=3_600_000).contains(ms))
            || r.demand.len() > 10_000
            || r.demand.iter().any(|(id, demands)| {
                !t.requests.iter().any(|q| {
                    q.fact.run_id == *id
                        && q.fact.synthetic
                        && crate::rehearsal::belongs(&r.session, q.fact.label.as_deref())
                }) || demands.is_empty()
                    || demands.len() > 64
                    || demands.iter().any(|(m, c)| {
                        !members.contains(m.as_str())
                            || !c.is_finite()
                            || !(0.0..=256.0).contains(c)
                    })
            }))
    {
        return Err(bad("invalid controlled rehearsal provenance"));
    }
    let mut total_rows = 0usize;
    for m in &t.members {
        if m.load.len() > 43_201
            || m.scheduler_settings.len() > 1024
            || m.errors.len() > 64
            || m.errors.iter().any(|e| !is_code(e))
        {
            return Err(bad(
                "member history exceeds supported bounds or has invalid error codes",
            ));
        }
        total_rows = total_rows
            .checked_add(m.load.len())
            .ok_or_else(|| bad("load row count overflow"))?;
        if m.load.windows(2).any(|pair| pair[0].at >= pair[1].at)
            || m.scheduler_settings
                .windows(2)
                .any(|pair| pair[0].at >= pair[1].at)
        {
            return Err(bad(
                "member load and settings must have strictly increasing timestamps",
            ));
        }
        for b in &m.load {
            if b.at.millis() < start / 60_000 * 60_000
                || b.at.millis() > start.saturating_add(MAXIMUM_SPAN)
                || !b.at.millis().is_multiple_of(60_000)
                || b.seconds > u64::from(u32::MAX)
                || b.weighted_cores.millis() == 0
                || b.capacity > b.weighted_cores
                || !cores(b.in_use_mean)
                || b.in_use_mean > b.weighted_cores.as_f64()
                || b.in_use_max > b.weighted_cores
                || !share(b.cpu_mean)
                || !share(b.cpu_max)
                || !share(b.outside_mean)
                || b.speed_min.is_some_and(|v| !PLAUSIBLE.contains(&v))
                || b.speed_max.is_some_and(|v| !PLAUSIBLE.contains(&v))
                || b.speed_min.zip(b.speed_max).is_some_and(|(a, b)| a > b)
                || b.cpu_mean
                    .zip(b.cpu_max)
                    .is_some_and(|(a, b)| a > b + 1e-12)
                || b.queued_max > MAXIMUM_REQUESTS
                || b.interactive_queued_max.is_some_and(|v| v > b.queued_max)
                || [
                    b.busy_seconds,
                    b.saturated_seconds,
                    b.saturated_busy_seconds,
                    b.memory_pressure_seconds,
                ]
                .into_iter()
                .any(|v| v > b.seconds)
            {
                return Err(bad("invalid minute load measurement"));
            }
        }
        for s in &m.scheduler_settings {
            if s.at.millis() > start.saturating_add(MAXIMUM_SPAN)
                || !capacity(s.capacity)
                || s.parameters.validate().is_err()
            {
                return Err(bad("invalid historical scheduler settings"));
            }
        }
    }
    if total_rows
        .checked_mul(t.requests.len())
        .is_none_or(|work| work > MAXIMUM_WORK)
    {
        return Err(bad(
            "load/request product exceeds deterministic simulator work bound",
        ));
    }
    let mut ids = BTreeSet::new();
    let mut parents = BTreeMap::new();
    for r in &t.requests {
        let f = &r.fact;
        if !f.believable()
            || r.execution_reference.as_ref().is_some_and(|r| match r {
                crate::workload::trace::ExecutionReference::Unknown => false,
                crate::workload::trace::ExecutionReference::Known {
                    milliseconds,
                    member_speed,
                    ..
                } => {
                    !milliseconds.is_finite()
                        || *milliseconds < 0.0
                        || *milliseconds > MAXIMUM_SPAN as f64
                        || member_speed.is_some_and(|s| !PLAUSIBLE.contains(&s))
                }
            })
            || f.size.millis() == 0
            || !ids.insert(f.run_id.as_str())
            || !members.contains(f.calling_member.as_str())
            || f.member.as_deref().is_some_and(|m| !members.contains(m))
            || f.requested_at.millis() >= end
            || f.requested_at.millis() < start.saturating_sub(MAXIMUM_SPAN)
            || [f.waited_ms, f.run_ms, f.sync_ms]
                .into_iter()
                .flatten()
                .any(|ms| ms > MAXIMUM_SPAN)
            || f.nested != r.nested_in.is_some()
            || !f.nested && r.shares_capacity_of.is_some()
            || r.nested_in
                .as_deref()
                .is_some_and(|v| !id(v) || v == f.run_id)
            || r.shares_capacity_of
                .as_deref()
                .is_some_and(|v| !id(v) || v == f.run_id)
            || r.workspace.as_ref().is_some_and(|w| {
                [&w.workspace, &w.repository]
                    .iter()
                    .any(|v| v.len() != 64 || !v.bytes().all(|b| b.is_ascii_hexdigit()))
            })
            || r.pre_run_attempts_complete
                && r.pre_run_attempts.as_ref().is_none_or(|attempts| {
                    attempts.iter().any(|attempt| attempt.started_at.is_none())
                })
            || r.pre_run_attempts.as_ref().is_some_and(|attempts| {
                attempts.len() > crate::scheduling::pre_run_sync::History::MAXIMUM_ATTEMPTS
                    || attempts.iter().any(|attempt| {
                        !attempt.believable()
                            || attempt.started_at.is_some_and(|at| at < f.requested_at)
                    })
            })
            || matches!(&r.outcome,Some(TraceOutcome::Failed{code}) if !is_code(code))
        {
            return Err(bad("invalid, duplicate or unknown-member request"));
        }
        let placement = match &r.placement_request {
            Placement::Pinned => PlacementKind::Pinned,
            Placement::Placeable => PlacementKind::Placeable,
            Placement::Member(m) => {
                if !name(m) || m.len() > crate::scheduling::request::MAXIMUM_MEMBER {
                    return Err(bad("invalid required member name"));
                }
                PlacementKind::Required
            }
        };
        if f.placement != placement
            || f.kind == Kind::Lease && r.placement_request != Placement::Pinned
        {
            return Err(bad("request placement is inconsistent"));
        }
        if let Urgency::Interactive { hold_limit_ms } = r.urgency {
            if f.deferrable
                || hold_limit_ms.is_some_and(|ms| ms > MAXIMUM_HOLD_LIMIT.as_millis() as u64)
            {
                return Err(bad("interactive urgency or hold limit is inconsistent"));
            }
        } else if let Urgency::Deferrable { deadline, .. } = r.urgency
            && (!f.deferrable
                || r.placement_request != Placement::Pinned
                || deadline.since(f.requested_at) > MAXIMUM_DEADLINE)
        {
            return Err(bad("deferrable urgency is inconsistent"));
        }
        if r.hold_limit.is_some_and(|h| {
            h.seconds > MAXIMUM_HOLD_LIMIT
                || h.expected_run_seconds
                    .is_some_and(|d| d.as_millis() > u128::from(MAXIMUM_SPAN))
        }) {
            return Err(bad("resolved hold limit is out of range"));
        }
        if let Some(resources) = &f.resources {
            if resources.ended_at.is_some_and(|at| at < f.requested_at) {
                return Err(bad("request ended before arrival"));
            }
            if [resources.started_at, resources.ended_at]
                .into_iter()
                .flatten()
                .any(|at| at.millis() > end.saturating_add(MAXIMUM_SPAN))
            {
                return Err(bad("request resource timestamp is out of range"));
            }
            if resources
                .started_at
                .zip(resources.ended_at)
                .is_some_and(|(a, b)| b.since(a).as_millis() > u128::from(MAXIMUM_SPAN))
            {
                return Err(bad("request execution exceeds supported duration"));
            }
        }
        if let Some(p) = &f.predicted
            && [p.wait_ms, p.run_ms, p.sync_ms]
                .into_iter()
                .flatten()
                .any(|ms| ms > MAXIMUM_SPAN)
        {
            return Err(bad("request prediction exceeds supported duration"));
        }
        if let Some(d) = &r.decision
            && (d.decided_at < f.requested_at
                || d.decided_at.millis() > end.saturating_add(MAXIMUM_SPAN)
                || d.candidates.len() > 256
                || d.unreachable.len() > 256
                || d.arrival.as_ref().is_some_and(|a| a.candidates.len() > 256)
                || d.parameters.as_ref().is_some_and(|p| p.validate().is_err())
                || !finite_json(
                    &serde_json::to_value(d).map_err(|_| bad("decision is not serializable"))?,
                    0,
                ))
        {
            return Err(bad("invalid recorded decision"));
        }
        if let Some(parent) = r.nested_in.as_deref() {
            parents.insert(f.run_id.as_str(), parent);
        }
        // Missing ancestors are retained as missing evidence. A present ancestor
        // must have arrived first; this also prevents self references and cycles.
    }
    let by_id: BTreeMap<_, _> = t
        .requests
        .iter()
        .map(|r| (r.fact.run_id.as_str(), r))
        .collect();
    for (child, parent) in &parents {
        if let Some(p) = by_id.get(parent)
            && p.fact.requested_at > by_id[child].fact.requested_at
        {
            return Err(bad("nested request predates its recorded parent"));
        }
    }
    let mut done = BTreeSet::new();
    for child in parents.keys() {
        let mut chain = BTreeSet::new();
        let mut at = *child;
        while !done.contains(at) {
            if !chain.insert(at) {
                return Err(bad("nested request cycle"));
            }
            let Some(next) = parents.get(at) else {
                break;
            };
            at = next;
        }
        done.extend(chain);
    }
    let mut leftover_ids = BTreeSet::new();
    for l in &t.leftovers {
        if !id(&l.run_id)
            || !members.contains(l.member.as_str())
            || !leftover_ids.insert((l.member.as_str(), l.run_id.as_str()))
            || l.since.millis() > start.saturating_add(MAXIMUM_SPAN)
            || l.since.millis() < start.saturating_sub(MAXIMUM_SPAN)
            || l.cap_ms > DAY
            || l.duration_ms.is_some_and(|ms| ms > MAXIMUM_SPAN)
            || l.cpu_ms
                .is_some_and(|ms| ms > crate::scheduling::record::MAXIMUM_CPU_MS)
            || matches!(&l.ending,Some(crate::scheduling::leftovers::Ending::NextRun{run_id}) if !id(run_id))
        {
            return Err(bad("invalid leftover interval"));
        }
    }
    let mut demand_ids = BTreeSet::new();
    for d in &t.demand {
        if !members.contains(d.member.as_str())
            || !text(&d.family, crate::rehearsal::label_limit(&d.family))
            || !text(&d.status, 100)
            || !demand_ids.insert((d.member.as_str(), d.family.as_str()))
            || [
                d.declared_cores,
                d.process_tree_cores,
                d.effective_cores,
                d.outside_baseline_cores,
                d.residual_rmse_cores,
            ]
            .into_iter()
            .flatten()
            .any(|v| !cores(v))
            || d.interval_cores.is_some_and(|[a, b]| {
                !cores(a) || !cores(b) || a > b || d.effective_cores.is_some_and(|v| v < a || v > b)
            })
        {
            return Err(bad("invalid or duplicate demand model"));
        }
    }
    let mut curve_ids = BTreeSet::new();
    for c in &t.contention {
        if !members.contains(c.member.as_str())
            || !text(&c.family, crate::rehearsal::label_limit(&c.family))
            || !text(&c.status, 100)
            || !curve_ids.insert((c.member.as_str(), c.family.as_str()))
            || c.points.len() > 128
        {
            return Err(bad("invalid or duplicate contention model"));
        }
        let mut upper = 0.0;
        for p in &c.points {
            let [low, high] = p.other_declared_cores;
            let [a, b] = p.duration_ratio_range;
            if !cores(low)
                || !high.is_finite()
                || high <= low
                || low < upper
                || !a.is_finite()
                || !b.is_finite()
                || !p.duration_ratio_median.is_finite()
                || a <= 0.0
                || a > p.duration_ratio_median
                || p.duration_ratio_median > b
                || b > 1_000_000.0
            {
                return Err(bad("invalid contention bin or duration ratio"));
            }
            upper = high;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reporting::fixtures::{self, T0},
        scheduling::timestamp::Timestamp,
        workload::{
            contention::{Curve, Point},
            trace::{Member, Request},
        },
    };
    fn fixture() -> Trace {
        let mut record = fixtures::pinned(1, "work", "mini", 2.0);
        record.decision = None;
        Trace {
            rehearsal: None,
            placement_replay: None,
            trace_version: VERSION,
            incomplete: false,
            since: Timestamp::from_millis(T0),
            until: Timestamp::from_millis(T0 + 60_000),
            master: Some("mini".into()),
            members: vec![Member {
                member: "mini".into(),
                load: vec![fixtures::bucket(0, 0.1, 0.0, 0, 0, 1.0)],
                errors: vec![],
                scheduler_settings: vec![],
            }],
            requests: vec![Request::of(&record).unwrap()],
            leftovers: vec![],
            demand: vec![],
            contention: vec![],
            limits: vec![],
        }
    }
    #[test]
    fn legacy_missing_evidence_is_valid_but_unknown_version_is_rejected_first() {
        let mut t = fixture();
        assert!(trace(&t).is_ok());
        t.trace_version = VERSION + 1;
        t.members.clear();
        assert_eq!(trace(&t).unwrap_err().code, "unsupported_trace_version");
    }
    #[test]
    fn duplicate_ids_unknown_members_and_unsorted_load_are_rejected() {
        let mut t = fixture();
        t.requests.push(t.requests[0].clone());
        assert!(trace(&t).is_err());
        t = fixture();
        t.requests[0].fact.calling_member = "missing".into();
        assert!(trace(&t).is_err());
        t = fixture();
        let duplicate = t.members[0].load[0].clone();
        t.members[0].load.push(duplicate);
        assert!(trace(&t).is_err());
    }
    #[test]
    fn malformed_physics_and_zero_capacity_cannot_reach_simulator_arithmetic() {
        let mut t = fixture();
        t.members[0].load[0].weighted_cores = crate::scheduling::capacity::Cores::ZERO;
        assert!(trace(&t).is_err());
        t = fixture();
        t.members[0].load[0].cpu_mean = Some(f64::NAN);
        assert!(trace(&t).is_err());
        t = fixture();
        t.contention.push(Curve {
            member: "mini".into(),
            family: "work".into(),
            matched_runs: 3,
            unmatched_runs: 0,
            status: "observational_curve".into(),
            points: vec![Point {
                other_declared_cores: [0.0, f64::MAX],
                runs: 3,
                duration_ratio_median: 0.0,
                duration_ratio_range: [0.0, 1.0],
            }],
        });
        assert!(trace(&t).is_err());
        t.contention[0].points[0].duration_ratio_median = 1.0;
        t.contention[0].points[0].duration_ratio_range = [1.0, 1.1];
        assert!(trace(&t).is_ok());
    }
    #[test]
    fn frozen_execution_references_validate_numbers_and_preserve_explicit_unknown() {
        use crate::workload::trace::ExecutionReference;
        let mut t = fixture();
        t.requests[0].execution_reference = Some(ExecutionReference::Unknown);
        assert!(trace(&t).is_ok());
        for (milliseconds, member_speed) in [(-1.0, Some(1.0)), (1000.0, Some(f64::INFINITY))] {
            t.requests[0].execution_reference = Some(ExecutionReference::Known {
                milliseconds,
                member_speed,
                identified: true,
            });
            assert!(trace(&t).is_err());
        }
    }
    #[test]
    fn missing_nested_ancestor_is_evidence_gap_but_cycles_are_invalid() {
        let mut t = fixture();
        t.requests[0].fact.nested = true;
        t.requests[0].nested_in = Some("r-000000000002".into());
        assert!(trace(&t).is_ok());
        let mut other = t.requests[0].clone();
        other.fact.run_id = "r-000000000002".into();
        other.nested_in = Some(t.requests[0].fact.run_id.clone());
        t.requests.push(other);
        assert!(trace(&t).is_err());
    }
    #[test]
    fn duration_and_urgency_fields_must_agree_with_the_projection() {
        let mut t = fixture();
        t.requests[0].fact.run_ms = Some(u64::MAX);
        assert!(trace(&t).is_err());
        t = fixture();
        t.requests[0].fact.deferrable = true;
        assert!(trace(&t).is_err());
        t = fixture();
        t.requests[0].fact.placement = PlacementKind::Required;
        assert!(trace(&t).is_err());
        t = fixture();
        t.until = Timestamp::from_millis(T0 + MAXIMUM_SPAN + 1);
        assert!(trace(&t).is_err());
    }
    #[test]
    fn load_mean_accepts_only_producer_roundoff_above_maximum() {
        let mut t = fixture();
        t.members[0].load[0].cpu_mean = Some(0.05000000000000001);
        t.members[0].load[0].cpu_max = Some(0.05);
        assert!(trace(&t).is_ok());
        t.members[0].load[0].cpu_mean = Some(0.0500001);
        assert!(trace(&t).is_err());
    }
}
