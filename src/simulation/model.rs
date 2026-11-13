//! Execution physics, separate from admission/placement's own estimates.
use crate::{
    scheduling::{placed::Warmth, timestamp::Timestamp},
    workload::{
        demand::{family, interval, median, overlap},
        trace::{Request, Trace},
    },
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Envelope {
    Lower,
    Point,
    Upper,
}

/// An unsupported curve is usable for exploration, never certification.
pub fn contention(
    trace: &Trace,
    member: &str,
    name: Option<&str>,
    other: f64,
    envelope: Envelope,
) -> (f64, bool) {
    // Fixed-work synthetic runs use the executor's physical sharing model.
    // No additional observational contention curve is applied.
    if trace.rehearsal.is_some() {
        return (1.0, true);
    }
    let Some(curve) = trace
        .contention
        .iter()
        .find(|c| c.member == member && Some(c.family.as_str()) == name)
    else {
        return (1.0, false);
    };
    let Some(point) = curve
        .points
        .iter()
        .find(|p| other >= p.other_declared_cores[0] && other < p.other_declared_cores[1])
    else {
        return (1.0, false);
    };
    let ratio = match envelope {
        Envelope::Lower => point.duration_ratio_range[0],
        Envelope::Point => point.duration_ratio_median,
        Envelope::Upper => point.duration_ratio_range[1],
    };
    (
        ratio.max(1.0),
        curve.status == "observational_curve" && point.runs >= 3,
    )
}

pub fn demand(trace: &Trace, request: &Request, member: &str, envelope: Envelope) -> (f64, bool) {
    if let Some(cores) = trace
        .rehearsal
        .as_ref()
        .and_then(|r| r.demand(request, member))
    {
        return (cores, true);
    }
    let model = trace
        .demand
        .iter()
        .find(|d| d.member == member && Some(d.family.as_str()) == family(&request.fact));
    match model.and_then(|d| Some((d.effective_cores?, d.interval_cores?))) {
        Some((point, band)) => (
            match envelope {
                Envelope::Lower => band[0],
                Envelope::Point => point,
                Envelope::Upper => band[1],
            },
            true,
        ),
        None => (request.fact.size.as_f64(), false),
    }
}

pub fn speed(trace: &Trace, member: &str, now: u64) -> Option<f64> {
    bucket(trace, member, now).and_then(|b| b.speed_max.or(b.speed_min))
}
pub fn bucket<'a>(
    trace: &'a Trace,
    member: &str,
    now: u64,
) -> Option<&'a crate::load_log::buckets::Bucket> {
    let loads = &trace.members.iter().find(|m| m.member == member)?.load;
    let i = loads.partition_point(|b| b.at.millis() <= now);
    i.checked_sub(1)
        .and_then(|i| loads.get(i))
        .filter(|b| now < b.at.millis() + 60_000)
}

/// Fixed exogenous residual, computed once from the observed execution timeline.
/// It is an inferred background, not independent CPU attribution.
pub fn outside(trace: &Trace, member: &str, now: u64, envelope: Envelope) -> Option<f64> {
    let b = bucket(trace, member, now)?;
    let at = b.at.millis();
    // Rehearsal buckets contain only their clipped window exposure. Ordinary
    // trace buckets still describe the full recorded minute.
    let (from, to) = if trace.rehearsal.is_some() {
        (
            at.max(trace.since.millis()),
            (at + 60_000).min(trace.until.millis()),
        )
    } else {
        (at, at + 60_000)
    };
    let span = (to - from).max(1) as f64;
    let mut identified = true;
    let active: f64 = trace
        .requests
        .iter()
        .filter(|r| r.fact.member.as_deref() == Some(member))
        .filter_map(|r| {
            let (start, end) = interval(&r.fact)?;
            let (mut cores, known) = demand(trace, r, member, Envelope::Point);
            // The worker's realized CPU cost can differ from its target duty.
            // Remove its measured process-tree exposure before inferring outside
            // CPU. OS CPU milliseconds use logical cores, H1 uses weighted ones.
            if let Some(calibration) = trace
                .rehearsal
                .as_ref()
                .and_then(|recording| recording.calibration.get(member))
                && let Some(cpu_ms) = r.fact.resources.as_ref().and_then(|v| v.cpu_ms)
                && end > start
            {
                cores = cpu_ms as f64 / (end - start) as f64 * b.weighted_cores.as_f64()
                    / f64::from(calibration.logical_cores);
            }
            if overlap(start, end, from, to) > 0.0 {
                identified &= known;
            }
            Some(overlap(start, end, from, to) / span * cores)
        })
        .sum();
    // Leftovers no longer hold grants. They are included both in total CPU and
    // in the grant-derived outside proxy, so remove their observed exposure in
    // either branch before the virtual leftover timeline is added back.
    let lingering: f64 = trace
        .leftovers
        .iter()
        .filter(|l| l.member == member)
        .filter_map(|l| leftover_interval(trace, l))
        .map(|(start, end, cores, _)| overlap(start, end, from, to) / span * cores)
        .sum();
    let baseline = (if identified {
        b.cpu_mean? * b.weighted_cores.as_f64() - active
    } else {
        b.outside_mean? * b.weighted_cores.as_f64()
    } - lingering)
        .max(0.0);
    let error = trace
        .demand
        .iter()
        .filter(|d| d.member == member)
        .filter_map(|d| d.residual_rmse_cores)
        .fold(0.0, f64::max);
    Some(match envelope {
        Envelope::Lower => (baseline - error).max(0.0),
        Envelope::Point => baseline,
        Envelope::Upper => baseline + error,
    })
}

/// The observed execution interval, including nested commands. Demand callers
/// exclude nested work separately because its outer grant already accounts for it.
fn execution_interval(trace: &Trace, request: &Request) -> Option<(u64, u64)> {
    let resources = request.fact.resources.as_ref()?;
    let start = resources.started_at?.millis() + super::lingering::observed_wait(trace, request);
    let end = resources.ended_at?.millis();
    (end > start).then_some((start, end))
}

/// Mirrors the virtual executor's recorded leftover input: process-tree CPU over
/// its retained duration, or an explicitly uncertain declared-size fallback.
fn leftover_interval(
    trace: &Trace,
    leftover: &crate::workload::trace::Leftover,
) -> Option<(u64, u64, f64, bool)> {
    let duration = leftover
        .duration_ms
        .unwrap_or(leftover.cap_ms)
        .min(leftover.cap_ms);
    if duration == 0 {
        return None;
    }
    let cores = leftover
        .cpu_ms
        .map(|cpu| cpu as f64 / duration as f64)
        .or_else(|| {
            trace
                .requests
                .iter()
                .find(|r| r.fact.run_id == leftover.run_id)
                .map(|r| r.fact.size.as_f64())
        })?;
    Some((
        leftover.since.millis(),
        leftover.since.millis().saturating_add(duration),
        cores,
        leftover.cpu_ms.is_some() && leftover.duration_ms.is_some(),
    ))
}

/// Freeze input work before any trial by integrating the *same* point physics
/// applied by the executor over the observed execution timeline. Normalizing by
/// an average bin alone would apply physical overload twice and would miss runs
/// crossing a contention-bin boundary. Every sensitivity trial reuses this point
/// reference; its envelope changes execution, never the amount of input work.
fn completed_reference(trace: &Trace, request: &Request) -> Option<(f64, bool)> {
    let ms = request
        .fact
        .run_ms?
        .saturating_sub(super::lingering::observed_wait(trace, request));
    let member = request.fact.member.as_deref()?;
    if ms == 0 {
        return Some((0.0, true));
    }
    let Some((start, end)) = execution_interval(trace, request) else {
        return Some((ms as f64, false));
    };
    let mut boundaries = BTreeSet::from([start, end]);
    let mut minute = (start / 60_000 + 1) * 60_000;
    while minute < end {
        boundaries.insert(minute);
        minute += 60_000;
    }
    let roots: Vec<_> = trace
        .requests
        .iter()
        .filter(|r| !r.fact.nested && r.fact.member.as_deref() == Some(member))
        .filter_map(|r| execution_interval(trace, r).map(|(s, e)| (r, s, e)))
        .filter(|(_, s, e)| *s < end && *e > start)
        .collect();
    let leftovers: Vec<_> = trace
        .leftovers
        .iter()
        .filter(|l| l.member == member)
        .filter_map(|l| leftover_interval(trace, l))
        .filter(|(s, e, _, _)| *s < end && *e > start)
        .collect();
    for (_, s, e) in &roots {
        boundaries.insert((*s).clamp(start, end));
        boundaries.insert((*e).clamp(start, end));
    }
    for (s, e, _, _) in &leftovers {
        boundaries.insert((*s).clamp(start, end));
        boundaries.insert((*e).clamp(start, end));
    }
    let boundaries: Vec<_> = boundaries.into_iter().collect();
    let mut work = 0.0;
    let mut known = true;
    for pair in boundaries.windows(2) {
        let [at, until] = [pair[0], pair[1]];
        let active: Vec<_> = roots
            .iter()
            .filter(|(_, s, e)| *s <= at && *e > at)
            .collect();
        let other = active
            .iter()
            .filter(|(r, _, _)| r.fact.run_id != request.fact.run_id)
            .map(|(r, _, _)| r.fact.size.as_f64())
            .sum();
        let (curve, supported) =
            contention(trace, member, family(&request.fact), other, Envelope::Point);
        known &= supported;
        let active_demand: f64 = active
            .iter()
            .map(|(r, _, _)| {
                let (cores, identified) = demand(trace, r, member, Envelope::Point);
                known &= identified;
                cores
            })
            .sum();
        let lingering: f64 = leftovers
            .iter()
            .filter(|(s, e, _, _)| *s <= at && *e > at)
            .map(|(_, _, cores, identified)| {
                known &= identified;
                cores
            })
            .sum();
        let physical = bucket(trace, member, at)
            .zip(outside(trace, member, at, Envelope::Point))
            .map(|(b, external)| {
                ((active_demand + lingering) / (b.weighted_cores.as_f64() - external).max(0.001))
                    .max(1.0)
            });
        known &= physical.is_some();
        let slowdown = curve.max(physical.unwrap_or(1.0));
        work += (until - at) as f64 / slowdown;
    }
    // In valid records the command interval and run_ms agree. Retain the
    // reported duration for old projections with timing differences, explicitly
    // without treating that discrepancy as identified evidence.
    known &= end - start == ms;
    Some((work * ms as f64 / (end - start) as f64, known))
}

pub fn reference(trace: &Trace, request: &Request) -> Option<(f64, bool)> {
    if let Some(ms) = trace
        .rehearsal
        .as_ref()
        .and_then(|r| r.uncontended_ms.get(&request.fact.run_id))
        .and_then(|rates| {
            rates.get(
                request
                    .fact
                    .member
                    .as_deref()
                    .unwrap_or(&request.fact.calling_member),
            )
        })
    {
        return Some((*ms, true));
    }
    if let Some(ms) = trace
        .rehearsal
        .as_ref()
        .and_then(|r| r.duration_ms.get(&request.fact.run_id))
    {
        return Some((*ms as f64, true));
    }
    if let Some(reference) = &request.execution_reference {
        return match reference {
            crate::workload::trace::ExecutionReference::Unknown => None,
            crate::workload::trace::ExecutionReference::Known {
                milliseconds,
                identified,
                ..
            } => Some((*milliseconds, *identified)),
        };
    }
    if request.fact.kind == crate::scheduling::request::Kind::Lease {
        return request.fact.run_ms.map(|ms| (ms as f64, true));
    }
    if request.fact.completed && request.fact.run_ms.is_some() && request.fact.member.is_some() {
        return completed_reference(trace, request);
    }
    let runs = trace
        .requests
        .iter()
        .filter(|r| {
            r.fact.label == request.fact.label
                && r.fact.kind == request.fact.kind
                && r.fact.warmth == request.fact.warmth
        })
        .filter_map(|r| completed_reference(trace, r).map(|(ms, _)| ms))
        .collect();
    median(runs).map(|ms| (ms, false))
}

/// Preserve input work and its source-member speed before trimming history.
pub fn freeze_reference(
    trace: &Trace,
    request: &Request,
) -> crate::workload::trace::ExecutionReference {
    match reference(trace, request) {
        Some((milliseconds, identified)) => crate::workload::trace::ExecutionReference::Known {
            milliseconds,
            identified,
            member_speed: reference_speed(trace, request),
        },
        None => crate::workload::trace::ExecutionReference::Unknown,
    }
}
pub fn reference_speed(trace: &Trace, request: &Request) -> Option<f64> {
    if let Some(reference) = &request.execution_reference {
        return match reference {
            crate::workload::trace::ExecutionReference::Known { member_speed, .. } => *member_speed,
            crate::workload::trace::ExecutionReference::Unknown => None,
        };
    }
    let member = request
        .fact
        .member
        .as_deref()
        .unwrap_or(match &request.placement_request {
            crate::scheduling::request::Placement::Member(target) => target.as_str(),
            _ => request.fact.calling_member.as_str(),
        });
    let at = request
        .fact
        .resources
        .as_ref()
        .and_then(|r| r.started_at)
        .unwrap_or(request.fact.requested_at)
        .millis();
    speed(trace, member, at)
}

/// Structural placement constraints are independent of occupancy and proposals.
pub fn structural_members(trace: &Trace, request: &Request) -> Vec<String> {
    use crate::scheduling::request::Placement;
    trace
        .members
        .iter()
        .filter(|m| match &request.placement_request {
            Placement::Pinned => m.member == request.fact.calling_member,
            Placement::Member(name) => &m.member == name,
            Placement::Placeable => {
                m.member == request.fact.calling_member
                    || (trace.master.as_ref() == Some(&request.fact.calling_member)
                        && request.workspace.is_some())
            }
        })
        .map(|m| m.member.clone())
        .collect()
}

/// Frozen admission capability and explicit transport/workspace availability.
/// Occupancy, saturation and hold/sync estimates do not constrain a free ideal.
pub fn eligible_members(trace: &Trace, request: &Request) -> Vec<String> {
    structural_members(trace, request)
        .into_iter()
        .filter(|name| {
            let member = trace
                .members
                .iter()
                .find(|m| m.member == *name)
                .expect("roster member");
            let candidate = request
                .decision
                .as_ref()
                .and_then(|d| d.candidates.iter().find(|c| c.member == *name));
            let capacity = member
                .scheduler_settings
                .iter()
                .rev()
                .find(|s| s.at <= request.fact.requested_at)
                .map(|s| s.capacity)
                .or_else(|| candidate.map(|c| c.capacity));
            capacity.is_none_or(|c| request.fact.size <= c.capacity)
                && !request
                    .decision
                    .as_ref()
                    .is_some_and(|d| d.unreachable.iter().any(|m| m.member == *name))
                && !candidate.and_then(|c| c.inputs.as_ref()).is_some_and(|i| {
                    matches!(i.copy, crate::scheduling::placement::Copy::Unavailable)
                })
        })
        .collect()
}

/// Uncontended runtime on the fastest structurally eligible member, frozen
/// from input evidence. Occupancy/wait and candidate settings never change it.
pub fn ideal(trace: &Trace, request: &Request) -> Option<f64> {
    ideal_from_reference(trace, request, reference(trace, request))
}
pub fn ideal_from_reference(
    trace: &Trace,
    request: &Request,
    reference: Option<(f64, bool)>,
) -> Option<f64> {
    use crate::scheduling::request::Kind;
    if let Some(recording) = &trace.rehearsal {
        return recording
            .ideal_ms
            .get(&request.fact.run_id)
            .copied()
            .flatten();
    }
    let (reference, _) = reference?;
    if request.fact.kind == Kind::Lease {
        return (reference > 0.0).then_some(reference);
    }
    let original_speed = reference_speed(trace, request)?;
    let mut best: Option<f64> = None;
    for member in eligible_members(trace, request) {
        // Eligibility is frozen from input constraints, never from the selected
        // candidate's grant sizes or queue. Missing speed is missing evidence.
        let selected = speed(trace, &member, request.fact.requested_at.millis())?;
        if selected <= 0.0 {
            return None;
        }
        let candidate = request
            .decision
            .as_ref()
            .and_then(|d| d.candidates.iter().find(|c| c.member == member));
        let warmth = if member == request.fact.calling_member {
            None
        } else if let Some(crate::scheduling::placement::Inputs {
            copy: crate::scheduling::placement::Copy::Remote { warmth, .. },
            ..
        }) = candidate.and_then(|c| c.inputs.as_ref())
        {
            Some(*warmth)
        } else if request.fact.member.as_deref() == Some(&member) {
            request.fact.warmth
        } else {
            None
        };
        let own_demand = demand(trace, request, &member, Envelope::Point).0;
        let physical_cores = bucket(trace, &member, request.fact.requested_at.millis())?
            .weighted_cores
            .as_f64();
        let mut ms = reference * original_speed / selected * (own_demand / physical_cores).max(1.0);
        if cold(warmth) != cold(request.fact.warmth) {
            let factor = trace
                .members
                .iter()
                .find(|m| m.member == member)
                .and_then(|m| {
                    m.scheduler_settings
                        .iter()
                        .rev()
                        .find(|s| s.at <= request.fact.requested_at)
                })
                .map(|s| s.parameters.placement.cold_run_factor)
                .unwrap_or_else(|| {
                    crate::scheduling::parameters::Parameters::default()
                        .placement
                        .cold_run_factor
                });
            ms *= if cold(warmth) { factor } else { 1.0 / factor };
        }
        best = Some(best.map_or(ms, |b| b.min(ms)));
    }
    best.filter(|v| v.is_finite() && *v > 0.0)
}

pub fn original_kind(trace: &Trace, request: &Request) -> crate::scheduling::request::Kind {
    trace
        .rehearsal
        .as_ref()
        .map_or(request.fact.kind, |r| r.original_kind(request))
}

pub fn cold(warmth: Option<Warmth>) -> bool {
    matches!(warmth, Some(Warmth::Cold | Warmth::ColdWithHistory))
}
pub fn terminal(r: &Request) -> Option<u64> {
    r.terminal_at().map(Timestamp::millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ideal_respects_frozen_capacity_placement_availability_and_cold_runtime() {
        use crate::scheduling::{capacity::Cores, record::WorkspaceIdentity, request::Placement};
        let mut t = crate::simulation::engine::tests::trace();
        t.requests.truncate(1);
        let r = &mut t.requests[0];
        r.placement_request = Placement::Placeable;
        r.workspace = Some(WorkspaceIdentity {
            workspace: "a".repeat(64),
            repository: "b".repeat(64),
        });
        let r = t.requests[0].clone();
        assert_eq!(
            ideal_from_reference(&t, &r, Some((1000.0, true))),
            Some(500.0)
        );
        t.members[1].scheduler_settings[0].capacity.capacity = Cores::from_millis(500);
        assert_eq!(
            ideal_from_reference(&t, &r, Some((1000.0, true))),
            Some(1000.0)
        );
        t.members[1].scheduler_settings[0].capacity.capacity = Cores::ONE;
        let mut required = r.clone();
        required.placement_request = Placement::Member("a".into());
        assert_eq!(
            ideal_from_reference(&t, &required, Some((1000.0, true))),
            Some(1000.0)
        );
        // Explicitly unreachable members cannot supply the ideal. Occupancy or
        // estimate.eligible=false alone would not establish unavailability.
        let mut r = r;
        r.decision = Some(crate::scheduling::record::Decision {
            decided_at: r.fact.requested_at,
            verdict: crate::scheduling::record::Verdict::Started,
            reason: "fixture".into(),
            candidates: vec![],
            unreachable: vec![crate::scheduling::placement::Unreachable {
                peer_io: None,
                member: "b".into(),
                why: "unavailable".into(),
            }],
            parameters: None,
            scoring: None,
            arrival: None,
        });
        assert_eq!(
            ideal_from_reference(&t, &r, Some((1000.0, true))),
            Some(1000.0)
        );
        // A twice-as-fast cold member with factor three is slower than the warm caller.
        r.decision.as_mut().unwrap().unreachable.clear();
        r.fact.warmth = Some(Warmth::Warm);
        r.decision
            .as_mut()
            .unwrap()
            .candidates
            .push(crate::scheduling::record::Candidate {
                member: "b".into(),
                chosen: false,
                why: String::new(),
                capacity: t.members[1].scheduler_settings[0].capacity,
                in_use: Cores::ZERO,
                free: Cores::ONE,
                queued: Default::default(),
                load: None,
                cpu_utilization_window: None,
                gate: crate::scheduling::load::Gate::Open,
                expected_run_seconds: None,
                expected_completion: None,
                estimate: None,
                inputs: Some(crate::scheduling::placement::Inputs {
                    caller: false,
                    speed: Some(2.0),
                    utilization: None,
                    holding: vec![],
                    ahead: vec![],
                    copy: crate::scheduling::placement::Copy::Remote {
                        warmth: Warmth::Cold,
                        busy: false,
                    },
                    learned: Default::default(),
                    admission: None,
                }),
            });
        assert_eq!(
            ideal_from_reference(&t, &r, Some((1000.0, true))),
            Some(1000.0)
        );
        r.decision.as_mut().unwrap().candidates[0]
            .inputs
            .as_mut()
            .unwrap()
            .copy = crate::scheduling::placement::Copy::Unavailable;
        assert_eq!(eligible_members(&t, &r), vec!["a".to_string()]);
    }
    #[test]
    fn ideal_alone_still_obeys_the_members_physical_cpu_limit() {
        let mut t = crate::simulation::engine::tests::trace();
        t.requests.truncate(1);
        t.demand[0].effective_cores = Some(3.0);
        t.demand[0].interval_cores = Some([3.0, 3.0]);
        assert_eq!(
            ideal_from_reference(&t, &t.requests[0], Some((1000.0, true))),
            Some(1500.0)
        );
    }
    #[test]
    fn controlled_background_uses_the_same_clipped_exposure_as_recorded_cpu() {
        use std::collections::BTreeMap;
        let mut trace = crate::simulation::engine::tests::trace();
        trace.requests.truncate(1);
        let at = trace.members[0].load[0].at;
        trace.since = at.after(std::time::Duration::from_secs(10));
        trace.until = at.after(std::time::Duration::from_secs(12));
        let r = &mut trace.requests[0];
        let id = r.fact.run_id.clone();
        r.fact.member = Some("a".into());
        r.fact.requested_at = trace.since;
        r.fact.run_ms = Some(2000);
        r.fact.resources.as_mut().unwrap().started_at = Some(trace.since);
        r.fact.resources.as_mut().unwrap().ended_at = Some(trace.until);
        trace.members[0].load[0].cpu_mean = Some(0.75);
        trace.members[0].load[0].weighted_cores =
            crate::scheduling::capacity::Cores::from_millis(2000);
        trace.rehearsal = Some(crate::rehearsal::recording::Recording {
            calibration: BTreeMap::new(),
            ideal_ms: Default::default(),
            uncontended_ms: BTreeMap::new(),
            work_iterations: BTreeMap::new(),
            session: "a".repeat(24),
            source_digest: crate::build_identity::Digest::of(b"source"),
            build: crate::build_identity::this_build().unwrap(),
            load_factor: Default::default(),
            compress: 6.0,
            seed: 0,
            complete: false,
            planned: 1,
            requests: serde_json::Value::Null,
            aggregated_nested: vec![],
            load_complete: BTreeMap::new(),
            charge_floor_ms: BTreeMap::new(),
            duration_ms: BTreeMap::from([(id.clone(), 2000)]),
            demand: BTreeMap::from([(id, BTreeMap::from([("a".into(), 0.5)]))]),
        });
        assert_eq!(
            outside(&trace, "a", trace.since.millis(), Envelope::Point),
            Some(1.0)
        );
    }
    #[test]
    fn realized_rehearsal_cpu_cost_is_not_attributed_to_outside_load() {
        let mut trace: Trace =
            serde_json::from_str(include_str!("../../tests/fixtures/run5-rehearsal.json")).unwrap();
        let member = "mac-mini";
        // Every run has final process-tree CPU. Compute their weighted minute
        // exposure, then add a known half-core external baseline.
        let b = trace.members[0].load[2].clone();
        let from = b.at.millis();
        let to = from + 60_000;
        let observed: f64 = trace
            .requests
            .iter()
            .filter_map(|r| {
                let resources = r.fact.resources.as_ref()?;
                let start = resources.started_at?.millis();
                let end = resources.ended_at?.millis();
                Some(
                    overlap(start, end, from, to) / 60_000.0 * resources.cpu_ms? as f64
                        / (end - start) as f64
                        * 12.0
                        / 14.0,
                )
            })
            .sum();
        trace.members[0].load[2].cpu_mean = Some((observed + 0.5) / 12.0);
        assert!((outside(&trace, member, from, Envelope::Point).unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn sensitivity_selects_demand_and_contention_without_changing_declared_size() {
        let mut t = crate::simulation::engine::tests::trace();
        let r = t.requests[0].clone();
        assert_eq!(demand(&t, &r, "a", Envelope::Lower), (0.5, true));
        assert_eq!(demand(&t, &r, "a", Envelope::Upper), (1.5, true));
        assert_eq!(r.fact.size.as_f64(), 1.0);
        t.contention[0].points[0].duration_ratio_range = [1.0, 1.2];
        assert_eq!(
            contention(&t, "a", Some("work"), 0.0, Envelope::Upper),
            (1.2, true)
        );
        assert_eq!(
            contention(&t, "a", Some("work"), 5.0, Envelope::Upper),
            (1.0, false)
        );
    }
    fn single_run() -> Trace {
        let mut trace = crate::simulation::engine::tests::trace();
        trace.requests.truncate(1);
        trace
    }

    #[test]
    fn reference_removes_physical_overload_before_reapplying_it() {
        let mut trace = single_run();
        trace.demand[0].effective_cores = Some(1.5);
        trace.demand[0].interval_cores = Some([1.5, 1.5]);
        // One second of a 1.5-core process in a minute, plus a fixed one-core
        // background on the two-core member.
        trace.members[0].load[0].cpu_mean = Some((1.0 + 1.5 / 60.0) / 2.0);
        assert!(
            (outside(&trace, "a", trace.since.millis(), Envelope::Point).unwrap() - 1.0).abs()
                < 1e-12
        );
        let (work, known) = reference(&trace, &trace.requests[0]).unwrap();
        assert!(known);
        assert!((work - 1000.0 / 1.5).abs() < 1e-9);
        assert!((work * 1.5 - 1000.0).abs() < 1e-9);
    }

    #[test]
    fn reference_integrates_each_change_in_competing_work() {
        let mut trace = single_run();
        let start = trace.since;
        trace.requests[0].fact.run_ms = Some(2_000);
        trace.requests[0].fact.resources.as_mut().unwrap().ended_at =
            Some(start.after(std::time::Duration::from_millis(2_000)));
        let mut competing = trace.requests[0].clone();
        competing.fact.run_id = "r-000000000002".into();
        competing.fact.run_ms = Some(1_000);
        competing.fact.resources.as_mut().unwrap().ended_at =
            Some(start.after(std::time::Duration::from_millis(1_000)));
        trace.requests.push(competing);
        trace.demand[0].effective_cores = Some(0.5);
        trace.demand[0].interval_cores = Some([0.5, 0.5]);
        trace.contention[0].points[1].duration_ratio_median = 2.0;
        trace.contention[0].points[1].duration_ratio_range = [2.0, 2.0];
        trace.members[0].load[0].cpu_mean = Some(0.5 * 3.0 / 60.0 / 2.0);
        let (work, known) = reference(&trace, &trace.requests[0]).unwrap();
        assert!(known);
        assert_eq!(work, 1_500.0);
    }

    #[test]
    fn observed_leftover_cpu_is_removed_from_background_exactly_once() {
        let mut trace = single_run();
        let at = trace.since.millis();
        trace.leftovers.push(crate::workload::trace::Leftover {
            run_id: "r-000000000099".into(),
            member: "a".into(),
            since: trace.since,
            cap_ms: 60_000,
            duration_ms: Some(60_000),
            cpu_ms: Some(60_000),
            ending: None,
        });
        // One full core of leftovers plus one request core for one second.
        trace.members[0].load[0].cpu_mean = Some((1.0 + 1.0 / 60.0) / 2.0);
        let external = outside(&trace, "a", at, Envelope::Point).unwrap();
        assert!(external.abs() < 1e-12);
        let leftover = leftover_interval(&trace, &trace.leftovers[0]).unwrap();
        assert_eq!(leftover.2, 1.0);
        // The virtual timeline adds the movable leftover once, while the
        // exogenous background no longer contains its recorded exposure.
        assert!((external + leftover.2 - 1.0).abs() < 1e-12);
        trace.demand.clear();
        trace.members[0].load[0].outside_mean = Some(0.5);
        assert_eq!(outside(&trace, "a", at, Envelope::Point), Some(0.0));
    }

    #[test]
    fn normalization_includes_leftover_contention() {
        let mut trace = single_run();
        trace.demand[0].effective_cores = Some(1.5);
        trace.demand[0].interval_cores = Some([1.5, 1.5]);
        trace.leftovers.push(crate::workload::trace::Leftover {
            run_id: "r-000000000099".into(),
            member: "a".into(),
            since: trace.since,
            cap_ms: 60_000,
            duration_ms: Some(60_000),
            cpu_ms: Some(60_000),
            ending: None,
        });
        trace.members[0].load[0].cpu_mean = Some((1.0 + 1.5 / 60.0) / 2.0);
        let (work, known) = reference(&trace, &trace.requests[0]).unwrap();
        assert!(known);
        assert_eq!(work, 800.0); // (1.5 request + 1 leftover) / 2 cores = 1.25
    }
}
