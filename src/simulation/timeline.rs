//! Queue and resource timelines, integrated across virtual event intervals.
use super::{
    adapters::World,
    execution::{Active, advance},
    member::Member,
};
use crate::{scheduling::timestamp::Timestamp, workload::trace::Trace};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

#[derive(Default, Clone, Debug, Serialize)]
pub struct Minute {
    pub member: String,
    pub at_ms: u64,
    /// Total simulated interval exposure, including periods without load evidence.
    pub seconds: f64,
    pub observed_seconds: f64,
    pub unknown_seconds: f64,
    pub queued_max: usize,
    pub interactive_queued_max: usize,
    pub cpu_core_seconds: f64,
    pub declared_core_seconds: f64,
    pub admitted_capacity_share_seconds: f64,
    pub capacity_observed_seconds: f64,
    pub leftover_core_seconds: f64,
    pub weighted_cores: f64,
    pub blocked_seconds: f64,
}
#[derive(Default, Debug, Serialize)]
pub struct Reserve {
    pub admission_violating_member_seconds: f64,
    pub admission_unknown_member_seconds: f64,
    pub violating_member_seconds: f64,
    pub excess_core_seconds: f64,
    pub maximum_excess_cores: f64,
    pub declared_excess_core_seconds: f64,
    pub exogenous_member_seconds: f64,
    pub unknown_member_seconds: f64,
    pub unknown_minutes: BTreeMap<String, BTreeSet<u64>>,
}
pub(super) fn record_queues(
    members: &BTreeMap<String, Member>,
    timelines: &mut BTreeMap<(String, u64), Minute>,
    now: u64,
    since: Timestamp,
    until: Timestamp,
) {
    if now < since.millis() || now >= until.millis() {
        return;
    }
    for (name, m) in members {
        let s = m.scheduler.member_state();
        let minute = timelines
            .entry((name.clone(), now / 60_000 * 60_000))
            .or_insert_with(|| Minute {
                member: name.clone(),
                at_ms: now / 60_000 * 60_000,
                weighted_cores: m.capacity.weighted_cores.as_f64(),
                ..Default::default()
            });
        minute.queued_max = minute
            .queued_max
            .max(s.queued.interactive + s.queued.deferrable);
        minute.interactive_queued_max = minute.interactive_queued_max.max(s.queued.interactive);
    }
}
#[allow(clippy::too_many_arguments)]
pub(super) fn integrate(
    trace: &Trace,
    members: &BTreeMap<String, Member>,
    world: &Arc<Mutex<World>>,
    active: &mut BTreeMap<String, Active>,
    timelines: &mut BTreeMap<(String, u64), Minute>,
    reserve: &mut Reserve,
    now: u64,
    next: u64,
    since: Timestamp,
    until: Timestamp,
) {
    advance(trace, world, active, now, next);
    let seconds = next
        .min(until.millis())
        .saturating_sub(now.max(since.millis())) as f64
        / 1000.0;
    // Cohort work may finish after the evaluation day. Day timelines remain
    // clipped, while every admitted execution interval still owes its reserve.
    let reserve_seconds = next.saturating_sub(now.max(since.millis())) as f64 / 1000.0;
    if seconds == 0.0 && reserve_seconds == 0.0 {
        return;
    }
    let w = world.lock().unwrap();
    for (name, m) in members {
        let state = m.scheduler.member_state();
        let outside = w.outside(name);
        let demand: f64 = w.family_resources(name).map(|o| o.demand).sum();
        let left: f64 = w
            .leftovers
            .values()
            .filter(|(member, _, _)| member == name)
            .map(|(_, d, _)| d)
            .sum();
        let usable = m.baseline_capacity.capacity.as_f64();
        if seconds > 0.0 {
            let minute = timelines
                .entry((name.clone(), now / 60_000 * 60_000))
                .or_insert_with(|| Minute {
                    member: name.clone(),
                    at_ms: now / 60_000 * 60_000,
                    weighted_cores: m.capacity.weighted_cores.as_f64(),
                    ..Default::default()
                });
            minute.seconds += seconds;
            match w.cpu(name) {
                Some(cpu) => {
                    minute.observed_seconds += seconds;
                    minute.cpu_core_seconds += cpu * m.capacity.weighted_cores.as_f64() * seconds;
                }
                None => minute.unknown_seconds += seconds,
            }
            minute.declared_core_seconds += state.in_use.as_f64() * seconds;
            if m.capacity.capacity.as_f64() > 0.0 {
                minute.admitted_capacity_share_seconds +=
                    state.in_use.as_f64() / m.capacity.capacity.as_f64() * seconds;
                minute.capacity_observed_seconds += seconds;
            }
            minute.leftover_core_seconds += left * seconds;
            if !state.gate.admits() {
                minute.blocked_seconds += seconds;
            }
        }
        if reserve_seconds == 0.0 {
            continue;
        }
        if let Some(outside) = outside {
            let excess = (outside + demand + left - usable).max(0.0);
            if outside > usable {
                reserve.exogenous_member_seconds += reserve_seconds;
                if demand + left > 0.0 {
                    reserve.violating_member_seconds += reserve_seconds;
                    reserve.excess_core_seconds += (demand + left) * reserve_seconds;
                    reserve.maximum_excess_cores = reserve.maximum_excess_cores.max(demand + left);
                }
            } else if excess > 0.0 {
                reserve.violating_member_seconds += reserve_seconds;
                reserve.excess_core_seconds += excess * reserve_seconds;
                reserve.maximum_excess_cores = reserve.maximum_excess_cores.max(excess);
            }
        } else {
            reserve.unknown_member_seconds += reserve_seconds;
            if reserve_seconds > 0.0 {
                reserve
                    .unknown_minutes
                    .entry(name.clone())
                    .or_default()
                    .insert(now / 60_000 * 60_000);
            }
        }
        let admitted_excess =
            (state.in_use.as_f64() - m.capacity.capacity.as_f64().min(usable)).max(0.0);
        if admitted_excess > 0.0 {
            reserve.admission_violating_member_seconds += reserve_seconds;
        }
        reserve.declared_excess_core_seconds += admitted_excess * reserve_seconds;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        scheduling::clock::{Clock, VirtualClock},
        simulation::{
            adapters::{Registry, World},
            engine::Trial,
            member::FleetAdapters,
            metrics,
            model::Envelope,
            settings::Settings,
        },
    };

    #[tokio::test(start_paused = true)]
    async fn reserve_follows_admitted_work_after_midnight_while_day_timelines_stop() {
        use crate::scheduling::{
            capacity::Cores,
            request::{AdmissionRequest, Kind, Placement, Urgency},
            run_id::RunId,
        };
        use crate::simulation::adapters::Occupancy;
        use std::time::Duration;
        let mut trace = crate::simulation::engine::tests::trace();
        let midnight = trace.since.after(Duration::from_secs(86400));
        trace.requests.clear();
        trace.members.truncate(1);
        trace.until = midnight.after(Duration::from_secs(60));
        let before = &mut trace.members[0].load[0];
        before.at = Timestamp::from_millis(midnight.millis() - 60_000);
        let mut after = before.clone();
        after.at = midnight;
        after.cpu_mean = Some(0.5);
        after.cpu_max = Some(0.5);
        after.outside_mean = Some(0.5);
        trace.members[0].load.push(after);
        let trace = Arc::new(trace);
        let start = midnight.millis() - 1000;
        let clock: Arc<dyn Clock> = Arc::new(VirtualClock::new(Timestamp::from_millis(start)));
        let world = Arc::new(Mutex::new(World::new(
            trace.clone(),
            clock.clone(),
            Envelope::Point,
        )));
        let adapters = FleetAdapters {
            world: world.clone(),
            registry: Arc::default(),
            clock,
            master: trace.master.clone(),
        };
        let candidates = Settings::parse(&trace, &[]).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let baseline = &trace.members[0].scheduler_settings[0];
        let member = Member::start(
            "a",
            baseline.capacity,
            baseline.parameters.clone(),
            &candidates,
            &adapters,
            temp.path(),
            0,
        )
        .unwrap();
        member.processes.insert(100, 101);
        member.processes.insert(101, 1);
        let id = RunId::parse("r-000000000099").unwrap();
        member.ids.0.lock().unwrap().push_back(id.clone());
        member
            .scheduler
            .request(
                100,
                AdmissionRequest {
                    workload: None,
                    kind: Kind::Run,
                    holder_pid: None,
                    size: Some(Cores::ONE),
                    label: None,
                    placement: Placement::Pinned,
                    urgency: Urgency::Interactive {
                        hold_limit_ms: Some(0),
                    },
                    workspace: None,
                    settings: None,
                },
            )
            .await
            .unwrap();
        world.lock().unwrap().occupancy.insert(
            id.to_string(),
            Occupancy {
                member: "a".into(),
                demand: 1.0,
                declared: 1.0,
                nested: false,
                family: id.to_string(),
                live: true,
                workspace: None,
            },
        );
        let members = BTreeMap::from([("a".to_owned(), member)]);
        let mut rows = BTreeMap::new();
        let mut reserve = Reserve::default();
        integrate(
            &trace,
            &members,
            &world,
            &mut BTreeMap::new(),
            &mut rows,
            &mut reserve,
            start,
            midnight.millis(),
            trace.since,
            midnight,
        );
        assert_eq!(reserve.excess_core_seconds, 0.0);
        tokio::time::advance(Duration::from_secs(1)).await;
        integrate(
            &trace,
            &members,
            &world,
            &mut BTreeMap::new(),
            &mut rows,
            &mut reserve,
            midnight.millis(),
            midnight.millis() + 1000,
            trace.since,
            midnight,
        );
        assert_eq!(
            reserve.excess_core_seconds, 1.0,
            "post-day outside load plus the admitted run must still respect reserve"
        );
        assert_eq!(reserve.violating_member_seconds, 1.0);
        assert_eq!(rows.values().map(|row| row.seconds).sum::<f64>(), 1.0);
        assert!(rows.keys().all(|(_, at)| *at < midnight.millis()));
        world
            .lock()
            .unwrap()
            .background
            .remove(&("a".into(), midnight.millis()));
        integrate(
            &trace,
            &members,
            &world,
            &mut BTreeMap::new(),
            &mut rows,
            &mut reserve,
            midnight.millis() + 1000,
            midnight.millis() + 2000,
            trace.since,
            midnight,
        );
        assert_eq!(reserve.unknown_member_seconds, 1.0);
        assert!(reserve.unknown_minutes["a"].contains(&midnight.millis()));
    }

    #[tokio::test(start_paused = true)]
    async fn missing_load_exposure_is_unknown_while_observed_idle_stays_zero() {
        let mut trace = crate::simulation::engine::tests::trace();
        trace.requests.clear();
        trace.members[0].load.clear();
        let trace = Arc::new(trace);
        let clock: Arc<dyn Clock> = Arc::new(VirtualClock::new(trace.since));
        let world = Arc::new(Mutex::new(World::new(
            trace.clone(),
            clock.clone(),
            Envelope::Point,
        )));
        let registry: Registry = Arc::default();
        let adapters = FleetAdapters {
            world: world.clone(),
            registry,
            clock,
            master: trace.master.clone(),
        };
        let candidates = Settings::parse(&trace, &[]).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let mut members = BTreeMap::new();
        for (index, member) in trace.members.iter().enumerate() {
            let baseline = &member.scheduler_settings[0];
            members.insert(
                member.member.clone(),
                Member::start(
                    &member.member,
                    baseline.capacity,
                    baseline.parameters.clone(),
                    &candidates,
                    &adapters,
                    temp.path(),
                    index,
                )
                .unwrap(),
            );
        }
        let mut timelines = BTreeMap::new();
        let mut reserve = Reserve::default();
        let start = trace.since.millis();
        integrate(
            &trace,
            &members,
            &world,
            &mut BTreeMap::new(),
            &mut timelines,
            &mut reserve,
            start,
            start + 1000,
            trace.since,
            trace.until,
        );
        let missing = &timelines[&("a".into(), start)];
        assert_eq!(missing.seconds, 1.0);
        assert_eq!(missing.observed_seconds, 0.0);
        assert_eq!(missing.unknown_seconds, 1.0);
        let observed = &timelines[&("b".into(), start)];
        assert_eq!(observed.observed_seconds, 1.0);
        assert_eq!(observed.unknown_seconds, 0.0);
        assert_eq!(reserve.unknown_member_seconds, 1.0);
        let trial = Trial {
            envelope: Envelope::Point,
            outcomes: Vec::new(),
            timelines: timelines.into_values().collect(),
            reserve,
            missing_evidence: Vec::new(),
        };
        let report = metrics::summarize(&trace, &trial, trace.since, trace.until);
        assert_eq!(report["members"]["a"]["observed_seconds"], 0.0);
        assert_eq!(report["members"]["a"]["unknown_seconds"], 1.0);
        assert!(report["members"]["a"]["cpu_mean"].is_null());
        assert!(report["members"]["a"]["cpu_core_seconds"].is_null());
        assert_eq!(report["members"]["b"]["cpu_mean"], 0.0);
        assert_eq!(report["reserve_coverage"]["status"], "unknown");
        assert!(report["violation_vector"]["reserve_excess_core_seconds"].is_null());
    }
}
