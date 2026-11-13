//! Select arrival density from complete, observed rehearsals of the same workload.
use crate::{
    failure::Result, replay_load::LoadFactor, scheduling::timestamp::Timestamp,
    workload::trace::Trace,
};
use serde::{Deserialize, Serialize};
mod fit;
use fit::Fit;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Shape {
    /// Master pressure remains the matching gate. Other members are diagnostics.
    pub admitted_capacity_share: f64,
    pub member_admitted_capacity_share: BTreeMap<String, Option<f64>>,
    pub queue_p50: f64,
    pub queue_p90: f64,
    pub queue_max: f64,
    pub cpu_mean: f64,
    pub observed_seconds: f64,
    pub wait_p90_ms: f64,
}
fn percentile(samples: &[(f64, f64)], fraction: f64) -> f64 {
    let mut samples = samples.to_vec();
    samples.sort_by(|a, b| a.0.total_cmp(&b.0));
    let threshold = samples.iter().map(|(_, w)| w).sum::<f64>() * fraction;
    let mut accumulated = 0.0;
    for (value, weight) in samples {
        accumulated += weight;
        if accumulated >= threshold {
            return value;
        }
    }
    unreachable!("nonempty, positive-weight samples")
}
#[derive(Clone, Debug, Serialize)]
pub struct Target {
    master: String,
    since: Timestamp,
    until: Timestamp,
    source_digest: crate::build_identity::Digest,
    shape: Shape,
}
impl Target {
    pub fn observed(trace: &Trace, since: Timestamp, until: Timestamp) -> Result<Self> {
        crate::simulation::validate_trace(trace)?;
        if since >= until
            || since < trace.since
            || until > trace.until
            || !since.millis().is_multiple_of(60_000)
            || !until.millis().is_multiple_of(60_000)
        {
            return Err(crate::tuning::invalid(
                "Busy-hour bounds must be minute-aligned, nonempty and inside the source history.",
            ));
        }
        let master = trace.master.as_ref().ok_or_else(|| {
            crate::tuning::invalid("Busy-hour calibration requires a recorded master.")
        })?;
        let member = trace
            .members
            .iter()
            .find(|m| &m.member == master)
            .expect("validated master");
        let rows: Vec<_> = member
            .load
            .iter()
            .filter(|r| {
                r.at >= since
                    && r.at < until
                    && r.seconds > 0
                    && r.cpu_mean.is_some()
                    && r.capacity.as_f64() > 0.0
            })
            .collect();
        let seconds = rows.iter().map(|r| r.seconds.min(60) as f64).sum::<f64>();
        if rows.is_empty() || seconds < until.since(since).as_secs_f64() * 0.9 {
            return Err(crate::tuning::invalid(
                "Busy-hour calibration requires at least 90% minute coverage, CPU and positive capacity on the master.",
            ));
        }
        let queues: Vec<_> = rows
            .iter()
            .map(|r| (r.queued_max as f64, r.seconds.min(60) as f64))
            .collect();
        let shape = Shape {
            member_admitted_capacity_share: member_shares(trace, since, until),
            admitted_capacity_share: rows
                .iter()
                .map(|r| r.in_use_mean / r.capacity.as_f64() * r.seconds.min(60) as f64)
                .sum::<f64>()
                / seconds,
            queue_p50: percentile(&queues, 0.5),
            queue_p90: percentile(&queues, 0.9),
            queue_max: rows.iter().map(|r| r.queued_max as f64).fold(0.0, f64::max),
            cpu_mean: rows
                .iter()
                .map(|r| r.cpu_mean.unwrap() * r.seconds.min(60) as f64)
                .sum::<f64>()
                / seconds,
            observed_seconds: seconds,
            wait_p90_ms: waits(trace, since, until, 1.0)?,
        };
        let source_digest = crate::build_identity::Digest::of(
            &serde_json::to_vec(&(&rows, &shape)).expect("reference serializes"),
        );
        Ok(Self {
            master: master.clone(),
            since,
            until,
            source_digest,
            shape,
        })
    }
    fn rehearsal(&self, trace: &Trace) -> Result<Shape> {
        let recording = trace.rehearsal.as_ref().expect("validated recording");
        if trace.master.as_deref() != Some(self.master.as_str()) {
            return Err(crate::tuning::invalid(
                "Observed rehearsal has a different master.",
            ));
        }
        let member = trace
            .members
            .iter()
            .find(|m| m.member == self.master)
            .ok_or_else(|| crate::tuning::invalid("Observed rehearsal has a different master."))?;
        let rows: Vec<_> = member
            .load
            .iter()
            .filter_map(|b| {
                let from = b.at.millis().max(trace.since.millis());
                let to = (b.at.millis() + 60_000).min(trace.until.millis());
                let seconds = to.saturating_sub(from) as f64 / 1000.0;
                let observed = seconds.min(b.seconds as f64);
                (observed > 0.0).then_some((b, observed))
            })
            .collect();
        let seconds: f64 = rows.iter().map(|(_, seconds)| seconds).sum();
        if trace.until.since(trace.since).as_secs_f64() > 3600.0 {
            return Err(crate::tuning::invalid(
                "Observed calibration exceeds the one-hour rehearsal limit.",
            ));
        }
        if seconds <= 0.0 || seconds < trace.until.since(trace.since).as_secs_f64() * 0.9 {
            return Err(crate::tuning::invalid(
                "Calibration needs at least 90% observed rehearsal load coverage.",
            ));
        }
        let mut events = std::collections::BTreeMap::<u64, i64>::new();
        for r in trace
            .requests
            .iter()
            .filter(|r| r.fact.calling_member == self.master)
        {
            let arrival = r.fact.requested_at;
            // The caller records admission from the original arrival even when
            // a peer hosts the command. Execution start also includes post-grant
            // workspace sync and must not extend the master's queue interval.
            let start = arrival.after(std::time::Duration::from_millis(
                r.fact.waited_ms.expect("validated admission wait"),
            ));
            if start > arrival {
                let offset = |t: Timestamp| {
                    ((t.millis() - trace.since.millis()) as f64 * recording.compress).round() as u64
                };
                *events.entry(offset(arrival)).or_default() += 1;
                *events.entry(offset(start)).or_default() -= 1;
            }
        }
        // Reconstruct the master's source-minute queue peaks from caller
        // arrival/admission events, including requests admitted on peers.
        // Wall-minute buckets of a compressed rehearsal describe different spans.
        let span =
            (trace.until.since(trace.since).as_millis() as f64 * recording.compress).ceil() as u64;
        let peaks = queue_peaks(events, span);
        Ok(Shape {
            member_admitted_capacity_share: member_shares(trace, trace.since, trace.until),
            admitted_capacity_share: rows
                .iter()
                .map(|(b, seconds)| b.in_use_mean / b.capacity.as_f64() * seconds)
                .sum::<f64>()
                / seconds,
            cpu_mean: rows
                .iter()
                .map(|(b, seconds)| b.cpu_mean.unwrap() * seconds)
                .sum::<f64>()
                / seconds,
            queue_p50: percentile(&peaks, 0.5),
            queue_p90: percentile(&peaks, 0.9),
            queue_max: peaks.iter().map(|(v, _)| *v).fold(0.0, f64::max),
            observed_seconds: seconds,
            wait_p90_ms: waits(trace, trace.since, trace.until, recording.compress)?,
        })
    }
}
fn queue_peaks(events: BTreeMap<u64, i64>, span: u64) -> Vec<(f64, f64)> {
    let mut queued = 0i64;
    let mut peaks = Vec::new();
    let mut events = events.into_iter().peekable();
    for from in (0..span).step_by(60_000) {
        let until = (from + 60_000).min(span);
        // Half-open bins: admissions exactly at `from` leave this bin's queue;
        // arrivals at the same instant enter it. Apply their net change first.
        while events.peek().is_some_and(|(at, _)| *at <= from) {
            queued += events.next().unwrap().1;
        }
        let mut peak = queued;
        while events.peek().is_some_and(|(at, _)| *at < until) {
            queued += events.next().unwrap().1;
            peak = peak.max(queued);
        }
        peaks.push((peak as f64, (until - from) as f64));
    }
    peaks
}
/// Exposure-weighted shares for every member, including the devbox. Missing or
/// less than 90% coverage stays unknown rather than appearing idle.
fn member_shares(
    trace: &Trace,
    since: Timestamp,
    until: Timestamp,
) -> BTreeMap<String, Option<f64>> {
    trace
        .members
        .iter()
        .map(|member| {
            let mut seconds = 0.0;
            let mut share_seconds = 0.0;
            for b in &member.load {
                let from = b.at.millis().max(since.millis());
                let to = (b.at.millis() + 60_000).min(until.millis());
                let observed = (to.saturating_sub(from) as f64 / 1000.0).min(b.seconds as f64);
                if b.capacity.as_f64() > 0.0 && b.cpu_mean.is_some() {
                    seconds += observed;
                    share_seconds += b.in_use_mean / b.capacity.as_f64() * observed;
                }
            }
            let share = (seconds > 0.0 && seconds >= until.since(since).as_secs_f64() * 0.9)
                .then(|| share_seconds / seconds);
            (member.member.clone(), share)
        })
        .collect()
}
fn waits(trace: &Trace, since: Timestamp, until: Timestamp, scale: f64) -> Result<f64> {
    let samples: Vec<_> = trace
        .requests
        .iter()
        .filter(|r| {
            !r.fact.nested
                && !r.fact.deferrable
                && r.fact.requested_at >= since
                && r.fact.requested_at < until
                && r.fact.member.is_some()
                && r.fact.failure.is_none()
        })
        .map(|r| r.fact.waited_ms.map(|ms| (ms as f64 * scale, 1.0)))
        .collect::<Option<_>>()
        .ok_or_else(|| {
            crate::tuning::invalid(
                "Calibration needs observed waits for every started interactive root.",
            )
        })?;
    if samples.is_empty() {
        return Err(crate::tuning::invalid(
            "Calibration needs observed interactive waits.",
        ));
    }
    Ok(percentile(&samples, 0.9))
}

/// Select only among supplied observed factors. Never infer a match from a
/// simulator, interpolate across trials or assume a monotone queue response.
pub fn calibrate(
    workload: &Trace,
    target: &Target,
    observations: &[Trace],
    compress: f64,
    seed: u64,
) -> Result<(Option<LoadFactor>, Value)> {
    calibrate_observations(
        &[workload_digest(workload)],
        target,
        observations,
        compress,
        seed,
    )
}

/// A preflight's selected export and the same source with explicit arrival bounds
/// are both valid rehearsal inputs. Keep their exact identities: selection adds
/// diagnostic limits and trims inactive settings/leftovers, while rehearse bounds
/// the source without that export decoration. Never rewrite recorded provenance.
pub(crate) fn calibrate_preflight(
    source: &Trace,
    selected: &Trace,
    target: &Target,
    observations: &[Trace],
    compress: f64,
    seed: u64,
) -> Result<(Option<LoadFactor>, Value)> {
    let bounded = crate::rehearsal::window::bound(source, selected.since, selected.until)?;
    calibrate_observations(
        &[workload_digest(selected), workload_digest(&bounded)],
        target,
        observations,
        compress,
        seed,
    )
}

fn workload_digest(workload: &Trace) -> crate::build_identity::Digest {
    crate::build_identity::Digest::of(&serde_json::to_vec(workload).expect("trace serializes"))
}

fn calibrate_observations(
    workload_digests: &[crate::build_identity::Digest],
    target: &Target,
    observations: &[Trace],
    compress: f64,
    seed: u64,
) -> Result<(Option<LoadFactor>, Value)> {
    let mut observations: Vec<_> = observations.iter().collect();
    observations.sort_by(|a, b| {
        f64::from(
            a.rehearsal
                .as_ref()
                .map(|r| r.load_factor)
                .unwrap_or_default(),
        )
        .total_cmp(&f64::from(
            b.rehearsal
                .as_ref()
                .map(|r| r.load_factor)
                .unwrap_or_default(),
        ))
    });
    let mut chosen: Option<(LoadFactor, Fit)> = None;
    let mut trials = Vec::new();
    let mut identity = None;
    for trace in observations {
        crate::simulation::validate_trace(trace)?;
        let recording = trace
            .rehearsal
            .as_ref()
            .filter(|r| r.complete_for(trace))
            .ok_or_else(|| {
                crate::tuning::invalid(
                    "Busy-hour calibration requires complete observed rehearsal traces.",
                )
            })?;
        if !workload_digests.contains(&recording.source_digest)
            || recording.compress != compress
            || recording.seed != seed
        {
            return Err(crate::tuning::invalid(
                "Calibration rehearsal workload, compression or seed differs from the selected experiment.",
            ));
        }
        if trace.members.iter().any(|m| {
            m.scheduler_settings
                .windows(2)
                .any(|w| w[0].parameters != w[1].parameters || w[0].capacity != w[1].capacity)
        }) {
            return Err(crate::tuning::invalid(
                "Calibration settings changed during a rehearsal.",
            ));
        }
        if trace.requests.iter().any(|r| {
            r.fact.requested_at < trace.since
                || r.fact.requested_at >= trace.until
                || !r.fact.completed
                || r.fact
                    .resources
                    .as_ref()
                    .and_then(|v| Some((v.started_at?, v.ended_at?)))
                    .is_none_or(|(start, end)| {
                        start < r.fact.requested_at || end < start || end > trace.until
                    })
        }) {
            return Err(crate::tuning::invalid(
                "Calibration needs complete in-window execution intervals without carry-in.",
            ));
        }
        if trace.requests.iter().any(|r| {
            r.fact.member.is_none()
                || r.fact.failure.is_some()
                || r.fact.waited_ms.is_none_or(|wait| {
                    wait > r
                        .fact
                        .resources
                        .as_ref()
                        .unwrap()
                        .started_at
                        .unwrap()
                        .since(r.fact.requested_at)
                        .as_millis() as u64
                })
        }) {
            return Err(crate::tuning::invalid(
                "Calibration needs observed arrival-to-admission waits before execution for every request.",
            ));
        }
        // The installed build and settings must be the same across all trials.
        let settings: Vec<_> = trace
            .members
            .iter()
            .map(|m| {
                (
                    &m.member,
                    m.scheduler_settings
                        .last()
                        .map(|s| (&s.parameters, s.capacity)),
                )
            })
            .collect();
        let key = serde_json::to_value((&recording.build, settings)).expect("identity serializes");
        if identity.as_ref().is_some_and(|v| v != &key) {
            return Err(crate::tuning::invalid(
                "Calibration rehearsals use different builds or settings.",
            ));
        }
        identity = Some(key);
        if trace
            .members
            .iter()
            .any(|m| m.load.iter().any(|b| b.capacity.as_f64() <= 0.0))
        {
            return Err(crate::tuning::invalid(
                "Calibration needs positive observed capacity.",
            ));
        }
        let shape = target.rehearsal(trace)?;
        let fit = Fit::between(&target.shape, &shape);
        trials.push(json!({"load_factor":recording.load_factor,"matched":fit.matched(),"distance":fit.distance,"deviations":fit.deviations,"shape":shape,"session":recording.session,"build":recording.build,"compress":recording.compress,"observed":true}));
        if chosen.as_ref().is_none_or(|(factor, current)| {
            fit::prefer(
                (recording.load_factor.into(), &fit),
                ((*factor).into(), current),
            )
        }) {
            chosen = Some((recording.load_factor, fit));
        }
    }
    let factor = chosen.as_ref().map(|(factor, _)| *factor);
    let selected_fit = chosen.as_ref().map(|(_, fit)| fit);
    let matched = selected_fit.is_some_and(|fit| fit.matched());
    let selection = match selected_fit {
        Some(fit) if fit.matched() => "matched",
        Some(_) => "nearest_match",
        None => "unavailable",
    };
    Ok((
        factor,
        json!({"target":target,"chosen_load_factor":factor,"matched":matched,"selection":selection,"distance":selected_fit.map(|fit|fit.distance),"deviations":selected_fit.map(|fit| &fit.deviations),"distance_basis":"Arithmetic mean over five metrics of absolute distance outside the inclusive band divided by full band width; zero inside. Equal weights; ties choose smallest observed F.","basis":"observed_rehearsals","criterion":"Smallest observed F fitting all bands; otherwise nearest observed F by mean normalised distance. Bands: master admitted share within 0.10, master-caller source-minute pending queue p50/p90 and peak within 1 (all execution members), and fleet-wide source-time interactive arrival-to-admission wait p90 within max(10s,25%). Other member shares are diagnostic. Same workload (including replay rules), master, build, settings, compression and seed required; no interpolation or simulated fallback.","queue_basis":{"reference":"sampled_master_local_admission","rehearsal":"master_caller_arrival_to_admission","limitation":"Rehearsal pending includes remote placement and pre-admission work. The reference load queue omits remote placing phases; matching is conservative and can reject identical behavior when the reference contains those phases."},"trials":trials}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observed_fixture() -> (Trace, Trace, Target) {
        let workload: Trace =
            serde_json::from_str(include_str!("../tests/fixtures/run5-rehearsal.json")).unwrap();
        let mut trace = workload.clone();
        trace.rehearsal.as_mut().unwrap().source_digest =
            crate::build_identity::Digest::of(&serde_json::to_vec(&workload).unwrap());
        let mut target = Target {
            master: trace.master.clone().unwrap(),
            since: trace.since,
            until: trace.until,
            source_digest: crate::build_identity::Digest::of(b"busy reference"),
            shape: Shape {
                admitted_capacity_share: 0.0,
                member_admitted_capacity_share: BTreeMap::new(),
                queue_p50: 0.0,
                queue_p90: 0.0,
                queue_max: 0.0,
                cpu_mean: 0.0,
                observed_seconds: 1.0,
                wait_p90_ms: 0.0,
            },
        };
        target.shape = target.rehearsal(&trace).unwrap();
        (workload, trace, target)
    }
    #[test]
    fn observed_selection_uses_smallest_matching_trial_and_preserves_wait_units() {
        let (workload, trace, target) = observed_fixture();
        // Golden pressure statistics from the prior local-pinned recording.
        assert_eq!(target.shape.queue_p50, 8.0);
        assert_eq!(target.shape.queue_p90, 15.0);
        assert_eq!(target.shape.queue_max, 16.0);
        assert_eq!(target.shape.wait_p90_ms, 1_274_514.0);
        let mut lower = trace.clone();
        lower.rehearsal.as_mut().unwrap().load_factor = "1.25".parse().unwrap();
        let observed_wait = trace
            .requests
            .iter()
            .filter_map(|r| r.fact.waited_ms)
            .max()
            .unwrap() as f64;
        assert!(target.shape.wait_p90_ms > observed_wait); // source-time waits, compression=6
        let (factor, report) = calibrate(&workload, &target, &[trace, lower], 6.0, 42).unwrap();
        assert_eq!(f64::from(factor.unwrap()), 1.25);
        assert_eq!(report["basis"], "observed_rehearsals");
        assert_eq!(report["trials"][0]["observed"], true);
    }
    #[test]
    fn rejects_unrelated_partial_or_differently_compressed_observations() {
        let (workload, mut trace, target) = observed_fixture();
        assert!(calibrate(&workload, &target, &[trace.clone()], 60.0, 42).is_err());
        assert!(calibrate(&workload, &target, &[trace.clone()], 6.0, 43).is_err());
        trace.rehearsal.as_mut().unwrap().complete = false;
        assert!(calibrate(&workload, &target, &[trace], 6.0, 42).is_err());
        let (workload, mut trace, target) = observed_fixture();
        trace.rehearsal.as_mut().unwrap().source_digest =
            crate::build_identity::Digest::of(b"other workload");
        assert!(calibrate(&workload, &target, &[trace], 6.0, 42).is_err());
    }

    #[test]
    fn observed_calibration_rejects_missing_coverage_and_settings_changes() {
        let (workload, mut trace, target) = observed_fixture();
        for bucket in &mut trace.members[0].load {
            bucket.seconds = 0;
            bucket.busy_seconds = 0;
            bucket.saturated_seconds = 0;
            bucket.saturated_busy_seconds = 0;
            bucket.memory_pressure_seconds = 0;
        }
        assert!(crate::simulation::validate_trace(&trace).is_ok());
        assert!(calibrate(&workload, &target, &[trace], 6.0, 42).is_err());
        let (workload, mut trace, target) = observed_fixture();
        let mut changed = trace.members[0].scheduler_settings[0].clone();
        changed.at = trace.since.after(std::time::Duration::from_secs(1));
        changed.capacity.capacity = crate::scheduling::capacity::Cores::ONE;
        changed.capacity.reserved = crate::scheduling::capacity::Cores::from_millis(11_000);
        trace.members[0].scheduler_settings.push(changed);
        assert!(crate::simulation::validate_trace(&trace).is_ok());
        assert!(calibrate(&workload, &target, &[trace], 6.0, 42).is_err());
    }

    #[test]
    fn placeable_remote_admissions_count_in_master_queue_without_sync_time() {
        use crate::{reporting::facts::PlacementKind, scheduling::request::Placement};
        let (workload, mut trace, mut target) = observed_fixture();
        for demands in trace.rehearsal.as_mut().unwrap().demand.values_mut() {
            demands.insert(trace.members[1].member.clone(), 2.0);
        }
        for request in &mut trace.requests {
            request.placement_request = Placement::Placeable;
            request.fact.placement = PlacementKind::Placeable;
            request.fact.member = Some(trace.members[1].member.clone());
            request.fact.requested_at = trace.since;
            request.fact.waited_ms = Some(20_000);
            request.fact.sync_ms = Some(30_000);
            let resources = request.fact.resources.as_mut().unwrap();
            resources.started_at = Some(trace.since.after(std::time::Duration::from_secs(50)));
            resources.ended_at = Some(trace.since.after(std::time::Duration::from_secs(60)));
        }
        crate::simulation::validate_trace(&trace).unwrap();
        let shape = target.rehearsal(&trace).unwrap();
        assert_eq!(shape.queue_max, 40.0);
        assert_eq!(shape.queue_p90, 0.0); // 20s * 6, not 50s * 6 of waiting
        assert_eq!(shape.wait_p90_ms, 120_000.0);
        assert!(shape.member_admitted_capacity_share[&trace.members[1].member].is_some());
        target.shape = shape;
        assert!(
            calibrate(&workload, &target, &[trace.clone()], 6.0, 42)
                .unwrap()
                .0
                .is_some()
        );
        // Removing remote waits must miss this target, even with matching load.
        for r in &mut trace.requests {
            r.fact.waited_ms = Some(0);
        }
        let (chosen, report) = calibrate(&workload, &target, &[trace], 6.0, 42).unwrap();
        assert!(chosen.is_some());
        assert_eq!(report["matched"], false);
        assert_eq!(report["selection"], "nearest_match");
        assert!(
            report["deviations"]["wait_p90_ms"]["deviation"]
                .as_f64()
                .unwrap()
                < 0.0
        );
    }

    #[test]
    fn reference_waits_include_remote_execution_and_other_callers() {
        let (_, mut trace, _) = observed_fixture();
        trace.requests.truncate(2);
        trace.requests[0].fact.waited_ms = Some(0);
        trace.requests[1].fact.member = Some(trace.members[1].member.clone());
        trace.requests[1].fact.calling_member = trace.members[1].member.clone();
        trace.requests[1].fact.waited_ms = Some(30_000);
        assert_eq!(
            waits(&trace, trace.since, trace.until, 1.0).unwrap(),
            30_000.0
        );
        trace.requests[1].fact.waited_ms = None;
        assert!(waits(&trace, trace.since, trace.until, 1.0).is_err());
    }

    #[test]
    fn rejects_missing_admission_waits_missing_population_and_interrupted_complete_population() {
        let (workload, trace, target) = observed_fixture();
        for variant in 0..5 {
            let mut trace = trace.clone();
            match variant {
                0 => trace.requests[0].fact.waited_ms = None,
                1 => trace.requests[0].fact.waited_ms = Some(86_400_000),
                2 => {
                    trace.requests.pop();
                }
                3 => trace.rehearsal.as_mut().unwrap().complete = false,
                _ => trace.master = Some(trace.members[1].member.clone()),
            }
            assert!(calibrate(&workload, &target, &[trace], 6.0, 42).is_err());
        }
    }

    #[test]
    fn peer_share_is_diagnostic_and_missing_coverage_stays_unknown() {
        let (workload, mut trace, target) = observed_fixture();
        for row in &mut trace.members[1].load {
            row.in_use_mean = row.capacity.as_f64();
        }
        let (_, report) = calibrate(&workload, &target, &[trace.clone()], 6.0, 42).unwrap();
        assert_eq!(report["matched"], true);
        assert_eq!(
            report["trials"][0]["shape"]["member_admitted_capacity_share"]
                [&trace.members[1].member],
            1.0
        );
        trace.members[1].load.clear();
        assert_eq!(
            member_shares(&trace, trace.since, trace.until)[&trace.members[1].member],
            None
        );
    }

    #[test]
    fn observed_calibration_rejects_carry_in_before_computing_offsets() {
        let (workload, mut trace, target) = observed_fixture();
        trace.requests[0].fact.requested_at =
            crate::scheduling::timestamp::Timestamp::from_millis(trace.since.millis() - 1);
        assert!(crate::simulation::validate_trace(&trace).is_ok());
        assert!(calibrate(&workload, &target, &[trace], 6.0, 42).is_err());
    }

    #[test]
    fn queue_bins_apply_boundary_admissions_and_simultaneous_arrivals_before_the_peak() {
        assert_eq!(
            queue_peaks(
                BTreeMap::from([(0, 2), (60_000, -2), (120_000, 1), (180_000, -1)]),
                210_000
            ),
            vec![
                (2.0, 60_000.0),
                (0.0, 60_000.0),
                (1.0, 60_000.0),
                (0.0, 30_000.0)
            ]
        );
    }

    #[test]
    fn matching_requires_both_share_and_queue_distribution() {
        let s = Shape {
            admitted_capacity_share: 0.85,
            member_admitted_capacity_share: BTreeMap::new(),
            queue_p50: 1.0,
            queue_p90: 4.0,
            queue_max: 7.0,
            cpu_mean: 0.65,
            observed_seconds: 3600.0,
            wait_p90_ms: 90_000.0,
        };
        assert!(Fit::between(&s, &s).matched());
        assert!(
            !Fit::between(
                &s,
                &Shape {
                    admitted_capacity_share: 0.70,
                    ..s.clone()
                }
            )
            .matched()
        );
        assert!(
            !Fit::between(
                &s,
                &Shape {
                    queue_p90: 2.0,
                    ..s.clone()
                }
            )
            .matched()
        );
        assert!(
            !Fit::between(
                &s,
                &Shape {
                    queue_p50: 3.0,
                    ..s.clone()
                }
            )
            .matched()
        );
        assert!(
            !Fit::between(
                &s,
                &Shape {
                    queue_max: 10.0,
                    ..s.clone()
                }
            )
            .matched()
        );
        assert!(
            !Fit::between(
                &s,
                &Shape {
                    wait_p90_ms: 150_000.0,
                    ..s.clone()
                }
            )
            .matched()
        );
    }
}
