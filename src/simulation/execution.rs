//! Simulated execution lifecycle around production grants: verdict delivery,
//! sync, contention progress, completion, learning reports and leftovers.
use super::{
    adapters::{self, CopyState, Occupancy, World},
    engine::invalid,
    member::Member,
    model,
};
use crate::{
    failure::Result,
    scheduling::{
        messages::{Grant, Release, RunEnded},
        placed::{PlacedReport, Reached, SyncSummary, Warmth},
        request::Kind,
        run_id::RunId,
        service::HostedEnd,
        timestamp::Timestamp,
    },
    workload::{
        demand::family,
        trace::{Request, Trace, TraceOutcome},
    },
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use tokio::task::JoinHandle;

#[derive(Clone, Debug, Serialize)]
pub struct Outcome {
    pub run_id: String,
    pub member: Option<String>,
    pub failure: Option<String>,
    pub waited_ms: Option<u64>,
    pub sync_ms: u64,
    pub leftovers_wait_ms: u64,
    pub run_ms: Option<u64>,
    pub terminal_at: Option<Timestamp>,
    pub state: &'static str,
    pub reference_ms: Option<f64>,
    pub ideal_ms: Option<f64>,
    pub charged_ms: Option<f64>,
    pub slowdown: Option<f64>,
    pub uncertain: bool,
    pub eligible_members: Vec<String>,
}
pub(super) struct Pending {
    pub index: usize,
    pub pid: u32,
    pub task: JoinHandle<Result<Grant>>,
}
pub(super) struct Active {
    pub index: usize,
    pub pid: u32,
    pub grant: Grant,
    pub remaining: f64,
    pub reference: f64,
    pub has_reference: bool,
    pub sync_until: u64,
    pub sync_ms: u64,
    pub leftovers_wait_ms: u64,
    pub run_ms: f64,
    pub slowdown: f64,
    pub uncertain: bool,
    pub eligible_members: Vec<String>,
    pub workspace: Option<String>,
    pub fixed_at: Option<u64>,
    pub command_started: bool,
    pub execution_failure: Option<String>,
    pub cpu_ms: Option<u64>,
}

// Drain real offer/expiry work and verdict delivery before time can move.
pub(super) async fn settle(
    members: &BTreeMap<String, Member>,
    pending: &BTreeMap<String, Pending>,
    trace: &Trace,
) {
    loop {
        tokio::task::yield_now().await;
        if members.values().any(|m| m.has_ready_scheduler_work()) {
            continue;
        }
        if pending.iter().any(|(id, p)| {
            !p.task.is_finished()
                && !members[&trace.requests[p.index].fact.calling_member]
                    .scheduler
                    .is_awaiting_verdict(&RunId::parse(id).unwrap())
        }) {
            continue;
        }
        break;
    }
}
pub(super) fn fixed_failure(r: &Request) -> Option<&str> {
    r.fact.failure.as_deref().filter(|code| {
        !matches!(
            *code,
            "fleet_busy"
                | "request_too_large"
                | "member_unavailable"
                | "config_sync_failed"
                | "deadline_skipped"
        )
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn collect(
    trace: &Trace,
    members: &BTreeMap<String, Member>,
    world: &Arc<Mutex<World>>,
    pending: &mut BTreeMap<String, Pending>,
    active: &mut BTreeMap<String, Active>,
    outcomes: &mut Vec<Outcome>,
    missing: &mut BTreeSet<String>,
    now: u64,
) -> Result<()> {
    let done: Vec<String> = pending
        .iter()
        .filter(|(_, p)| p.task.is_finished())
        .map(|(id, _)| id.clone())
        .collect();
    for id in done {
        let p = pending.remove(&id).unwrap();
        let r = &trace.requests[p.index];
        let result = p
            .task
            .await
            .map_err(|_| invalid("A simulated scheduling task failed."))?;
        let reference = world
            .lock()
            .unwrap()
            .references
            .get(&r.fact.run_id)
            .copied();
        match result {
            Err(e) => {
                let waited = now.saturating_sub(r.fact.requested_at.millis());
                let charge = if e.code == "fleet_busy" {
                    reference.map(|(ms, _)| {
                        waited.max(r.hold_limit.map_or(0, |h| h.seconds.as_millis() as u64)) as f64
                            + ms
                    })
                } else {
                    None
                };
                outcomes.push(Outcome {
                    run_id: id,
                    member: None,
                    failure: Some(e.code),
                    waited_ms: Some(waited),
                    sync_ms: 0,
                    leftovers_wait_ms: 0,
                    run_ms: None,
                    terminal_at: Some(Timestamp::from_millis(now)),
                    state: "ended",
                    reference_ms: reference.map(|v| v.0),
                    ideal_ms: None,
                    charged_ms: charge,
                    slowdown: None,
                    uncertain: reference.is_none_or(|v| !v.1),
                    eligible_members: Vec::new(),
                });
            }
            Ok(grant) => {
                if !grant.reused {
                    debug_assert_eq!(grant.run_id.to_string(), r.fact.run_id);
                }
                let eligible_members = members[&r.fact.calling_member]
                    .scheduler
                    .record(&grant.run_id)
                    .ok()
                    .and_then(|record| record.decision)
                    .map(|decision| {
                        decision
                            .candidates
                            .into_iter()
                            .filter(|c| c.estimate.as_ref().map_or(c.chosen, |e| e.eligible))
                            .map(|c| c.member)
                            .collect()
                    })
                    .unwrap_or_default();
                let m = &members[&grant.member];
                let (mut ms, known) = reference.unwrap_or((0.0, false));
                if reference.is_none() {
                    missing.insert(format!(
                        "requests[{id}].reference_execution_ms unavailable; trial unscorable"
                    ));
                }
                if r.fact.kind == Kind::Run {
                    let original = model::reference_speed(trace, r);
                    let selected = model::speed(trace, &grant.member, now);
                    if let Some((a, b)) = original.zip(selected) {
                        ms *= a / b;
                    } else {
                        missing.insert(format!("requests[{id}].reference speed missing"));
                    }
                }
                if let Some(runtime) = trace
                    .rehearsal
                    .as_ref()
                    .and_then(|recording| recording.uncontended_ms.get(&r.fact.run_id))
                    .and_then(|rates| rates.get(&grant.member))
                {
                    ms = *runtime;
                }
                let workspace = adapters::workspace(r).map(|s| s.root);
                let remote = grant.placed.is_some();
                let warmth = if remote {
                    workspace.as_ref().and_then(|s| {
                        world
                            .lock()
                            .unwrap()
                            .copies
                            .get(&(grant.member.clone(), s.clone()))
                            .map(|c| c.warmth)
                    })
                } else {
                    None
                };
                if trace.rehearsal.is_none()
                    && r.fact.kind == Kind::Run
                    && model::cold(warmth) != model::cold(r.fact.warmth)
                {
                    ms *= if model::cold(warmth) {
                        m.physics_parameters.placement.cold_run_factor
                    } else {
                        1.0 / m.physics_parameters.placement.cold_run_factor
                    };
                    missing.insert(format!("requests[{id}].cold/warm conversion uses prior, no matched input-work identity"));
                }
                let sync_ms = if remote {
                    if r.fact.member.as_ref() == Some(&grant.member) && warmth == r.fact.warmth {
                        r.fact.sync_ms.unwrap_or(0)
                    } else {
                        let samples: Vec<_> = trace
                            .requests
                            .iter()
                            .filter(|s| {
                                s.fact.member.as_ref() == Some(&grant.member)
                                    && s.fact.warmth == warmth
                            })
                            .filter_map(|s| s.fact.sync_ms.map(|v| v as f64))
                            .collect();
                        crate::workload::demand::median(samples)
                            .map(|ms| ms as u64)
                            .unwrap_or_else(|| {
                                missing.insert(format!(
                                    "requests[{id}].sync input unknown on selected copy; prior used"
                                ));
                                match warmth {
                                    Some(Warmth::Warm) => {
                                        m.physics_parameters.placement.sync_prior_warm.as_millis()
                                            as u64
                                    }
                                    Some(Warmth::ColdWithHistory) => m
                                        .physics_parameters
                                        .placement
                                        .sync_prior_cold_with_history
                                        .as_millis()
                                        as u64,
                                    _ => m.physics_parameters.placement.sync_prior_cold.as_millis()
                                        as u64,
                                }
                            })
                    }
                } else {
                    0
                };
                let (demand, identified) =
                    model::demand(trace, r, &grant.member, world.lock().unwrap().envelope);
                if !identified && !r.fact.nested {
                    missing.insert(format!(
                        "requests[{id}].effective demand unidentified on {}",
                        grant.member
                    ));
                }
                let family = members[&r.fact.calling_member]
                    .scheduler
                    .record(&grant.run_id)?
                    .shares_capacity_of
                    .map_or_else(|| id.clone(), |root| root.to_string());
                world.lock().unwrap().occupancy.insert(
                    id.clone(),
                    Occupancy {
                        member: grant.member.clone(),
                        demand,
                        declared: grant.size.as_f64(),
                        nested: grant.nested_in.is_some(),
                        family,
                        live: true,
                        workspace: workspace.clone(),
                    },
                );
                let (leftovers_wait_ms, leftovers_uncertain) = if remote {
                    let remaining = grant
                        .placed
                        .as_ref()
                        .unwrap()
                        .start_by
                        .since(Timestamp::from_millis(now));
                    workspace.as_ref().map_or((0, false), |workspace| {
                        super::lingering::settle(
                            &mut world.lock().unwrap(),
                            &grant.member,
                            workspace,
                            now,
                            remaining,
                        )
                    })
                } else {
                    (0, false)
                };
                if leftovers_uncertain {
                    missing.insert(format!(
                        "requests[{id}].leftover stopping latency uncertain"
                    ));
                }
                let sync_timeout = grant
                    .placed
                    .as_ref()
                    .map(|p| p.start_by.millis())
                    .filter(|at| *at < now + sync_ms + leftovers_wait_ms);
                let fixed_at = fixed_failure(r).and_then(|_| model::terminal(r));
                let cpu_ms = r.fact.resources.as_ref().and_then(|v| v.cpu_ms).map(|cpu| {
                    (cpu as f64 * ms / reference.map_or(1.0, |v| v.0).max(0.001)).round() as u64
                });
                active.insert(
                    id,
                    Active {
                        index: p.index,
                        pid: p.pid,
                        grant,
                        remaining: ms,
                        reference: ms,
                        has_reference: reference.is_some(),
                        sync_until: now + sync_ms + leftovers_wait_ms,
                        sync_ms,
                        leftovers_wait_ms,
                        run_ms: 0.0,
                        slowdown: 1.0,
                        uncertain: !known || !identified || leftovers_uncertain,
                        eligible_members,
                        workspace,
                        fixed_at: fixed_at.into_iter().chain(sync_timeout).min(),
                        command_started: false,
                        execution_failure: sync_timeout.map(|_| "fleet_busy".into()),
                        cpu_ms,
                    },
                );
            }
        }
    }
    for a in active
        .values_mut()
        .filter(|a| !a.command_started && a.execution_failure.is_none() && now >= a.sync_until)
    {
        begin_command(trace, members, a)?;
        a.command_started = true;
    }
    Ok(())
}

pub(super) fn milliseconds_until(ms: f64) -> u64 {
    let nearest = ms.round();
    if (ms - nearest).abs() < 1e-6 {
        nearest.max(1.0) as u64
    } else {
        ms.ceil().max(1.0) as u64
    }
}

pub(super) fn rate(trace: &Trace, world: &Arc<Mutex<World>>, a: &Active, now: u64) -> (f64, bool) {
    let r = &trace.requests[a.index];
    if r.fact.kind == Kind::Lease {
        return (1.0, true);
    }
    let w = world.lock().unwrap();
    let own_family = w
        .occupancy
        .get(&r.fact.run_id)
        .map(|o| o.family.as_str())
        .unwrap_or(&r.fact.run_id);
    let other = w
        .family_resources(&a.grant.member)
        .filter(|o| o.family != own_family)
        .map(|o| o.declared)
        .sum();
    let (curve, known) =
        model::contention(trace, &a.grant.member, family(&r.fact), other, w.envelope);
    let demand: f64 = w.family_resources(&a.grant.member).map(|o| o.demand).sum();
    let leftover: f64 = w
        .leftovers
        .values()
        .filter(|(member, _, _)| *member == a.grant.member)
        .map(|(_, cores, _)| cores)
        .sum();
    let physical = model::bucket(trace, &a.grant.member, now)
        .zip(w.outside(&a.grant.member))
        .map(|(bucket, outside)| {
            ((demand + leftover) / (bucket.weighted_cores.as_f64() - outside).max(0.001)).max(1.0)
        });
    (
        curve.max(physical.unwrap_or(1.0)),
        known && physical.is_some() && !w.deferred_family(&a.grant.member),
    )
}

pub(super) fn begin_command(
    trace: &Trace,
    members: &BTreeMap<String, Member>,
    a: &Active,
) -> Result<()> {
    let r = &trace.requests[a.index];
    if a.grant.placed.is_some() {
        let host = &members[&a.grant.member];
        let command = super::processes::Processes::frame(a.pid);
        host.processes.insert(command, 1);
        let sync = SyncSummary {
            duration_ms: Some(a.sync_ms),
            ..Default::default()
        };
        host.scheduler.hosted_synced(&a.grant.run_id, sync.clone());
        host.scheduler.hosted_waited_for_leftovers(
            &a.grant.run_id,
            std::time::Duration::from_millis(a.leftovers_wait_ms),
        );
        host.scheduler.hosted_started(&a.grant.run_id, command);
        members[&r.fact.calling_member].scheduler.placed_report(
            a.pid,
            &a.grant.run_id,
            PlacedReport {
                reached: Reached::Started,
                sync: Some(sync),
                ..Default::default()
            },
        )?;
    } else if r.fact.kind == Kind::Run {
        let local = &members[&r.fact.calling_member];
        let command = super::processes::Processes::frame(a.pid);
        local.processes.insert(command, a.pid);
        local.scheduler.command_started(
            a.pid,
            &a.grant.run_id,
            crate::scheduling::messages::CommandStarted { pid: command },
        )?;
    }
    Ok(())
}

pub(super) fn finish(
    trace: &Trace,
    members: &BTreeMap<String, Member>,
    world: &Arc<Mutex<World>>,
    a: &Active,
) -> Result<bool> {
    let r = &trace.requests[a.index];
    let id = &a.grant.run_id;
    let family_end = {
        let mut w = world.lock().unwrap();
        let family_end = w.end_occupancy(&r.fact.run_id);
        if !family_end.still_live
            && let Some((member, s)) = &family_end.released_copy
        {
            let warmth = if a.command_started {
                Warmth::Warm
            } else {
                w.copies
                    .get(&(member.clone(), s.clone()))
                    .map_or(Warmth::Cold, |c| c.warmth)
            };
            w.copies.insert(
                (member.clone(), s.clone()),
                CopyState {
                    warmth,
                    busy: false,
                },
            );
        }
        family_end
    };
    let caller = &members[&r.fact.calling_member].scheduler;
    let caller_live = caller
        .record(id)
        .is_ok_and(|r| r.state == crate::scheduling::record::State::Running);
    let cpu = a.cpu_ms;
    if a.grant.placed.is_some() {
        let host = &members[&a.grant.member].scheduler;
        let sync = SyncSummary {
            duration_ms: Some(a.sync_ms.min(
                world.lock().unwrap().clock.now().millis().saturating_sub(
                    r.fact.requested_at.millis() + a.grant.waited_ms + a.leftovers_wait_ms,
                ),
            )),
            ..Default::default()
        };
        host.hosted_synced(id, sync.clone());
        host.hosted_ended(
            id,
            if let Some(code) = &a.execution_failure {
                HostedEnd::Failed {
                    code: code.clone(),
                    reached: Reached::Placed,
                    exit: None,
                }
            } else {
                match &r.outcome {
                    Some(TraceOutcome::Cancelled) => HostedEnd::Failed {
                        code: "cancelled".into(),
                        reached: Reached::Started,
                        exit: None,
                    },
                    Some(TraceOutcome::Failed { code }) => HostedEnd::Failed {
                        code: code.clone(),
                        reached: Reached::Started,
                        exit: None,
                    },
                    Some(TraceOutcome::Signaled { signal }) => HostedEnd::Signaled {
                        signal: *signal,
                        cpu_ms: cpu,
                    },
                    _ => HostedEnd::Exited {
                        code: match r.outcome {
                            Some(TraceOutcome::Exited { exit_code }) => exit_code,
                            _ => 0,
                        },
                        cpu_ms: cpu,
                    },
                }
            },
            None,
        );
        if caller_live {
            caller.placed_report(
                a.pid,
                id,
                PlacedReport {
                    reached: Reached::Finished,
                    sync: Some(sync),
                    exit_code: Some(0),
                    cpu_ms: cpu,
                    ..Default::default()
                },
            )?;
        }
    }
    if caller_live {
        if matches!(r.outcome, Some(TraceOutcome::HolderExited)) {
            let m = &members[&r.fact.calling_member];
            let holder = super::processes::Processes::frame(a.pid);
            m.processes.0.lock().unwrap().remove(&holder);
            m.scheduler.process_exited(holder);
            if r.fact.kind == Kind::Run {
                m.processes.0.lock().unwrap().remove(&a.pid);
                m.scheduler.process_exited(a.pid);
            }
        } else if r.fact.kind == Kind::Lease {
            caller.release(
                a.pid,
                Release {
                    run_id: Some(id.clone()),
                    holder_pid: None,
                },
            )?;
        } else {
            caller.run_ended(
                a.pid,
                id,
                if let Some(code) = &a.execution_failure {
                    RunEnded::Failed {
                        code: code.clone(),
                        remote_failure_phase: None,
                    }
                } else {
                    match &r.outcome {
                        Some(TraceOutcome::Signaled { signal }) => RunEnded::Signaled {
                            plan_observation: None,
                            signal: *signal,
                            cpu_ms: cpu,
                            run_phases: None,
                        },
                        Some(TraceOutcome::Failed { code }) => RunEnded::Failed {
                            code: code.clone(),
                            remote_failure_phase: None,
                        },
                        Some(TraceOutcome::Cancelled) => RunEnded::Failed {
                            code: "cancelled".into(),
                            remote_failure_phase: None,
                        },
                        Some(TraceOutcome::Exited { exit_code }) => RunEnded::Exited {
                            plan_observation: None,
                            exit_code: *exit_code,
                            cpu_ms: cpu,
                            run_phases: None,
                        },
                        _ => RunEnded::Exited {
                            plan_observation: None,
                            exit_code: 0,
                            cpu_ms: cpu,
                            run_phases: None,
                        },
                    }
                },
            )?;
        }
    }
    let now = world.lock().unwrap().clock.now().millis();
    if a.execution_failure.is_some() {
        return Ok(family_end.uncertain);
    }
    for l in trace.leftovers.iter().filter(|l| l.run_id == r.fact.run_id) {
        let duration = l.duration_ms.unwrap_or(l.cap_ms).min(l.cap_ms);
        let cores = l.cpu_ms.map_or(r.fact.size.as_f64(), |cpu| {
            cpu as f64 / duration.max(1) as f64
        });
        world.lock().unwrap().leftovers.insert(
            l.run_id.clone(),
            (a.grant.member.clone(), cores, now + duration),
        );
        if duration > 0
            && let Some(workspace) = &a.workspace
        {
            let key = (a.grant.member.clone(), workspace.clone());
            let mut w = world.lock().unwrap();
            w.leftover_copies.insert(l.run_id.clone(), key);
        }
    }
    Ok(family_end.uncertain)
}

pub(super) fn advance(
    trace: &Trace,
    world: &Arc<Mutex<World>>,
    active: &mut BTreeMap<String, Active>,
    now: u64,
    next: u64,
) {
    for a in active.values_mut() {
        if now >= a.sync_until {
            let dt = (next - now) as f64;
            a.run_ms += dt;
            if a.has_reference {
                let (ratio, known) = rate(trace, world, a, now);
                a.remaining = (a.remaining - dt / ratio).max(0.0);
                a.slowdown = a.slowdown.max(ratio);
                a.uncertain |= !known;
            }
        }
    }
}

#[cfg(test)]
mod family_tests {
    use crate::scheduling::{
        capacity::{Capacity, Cores},
        clock::VirtualClock,
        messages::{CommandStarted, RunEnded},
        parameters::Parameters,
        record::State,
        request::{AdmissionRequest, Kind, Placement, Urgency},
        run_id::RunId,
    };
    use crate::simulation::{Options, simulate};
    use crate::simulation::{
        adapters::{Processes, World},
        member::{FleetAdapters, Member},
        model::Envelope,
        settings::Settings,
    };
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    #[tokio::test(start_paused = true)]
    async fn nested_lease_holders_are_distinct_and_do_not_end_or_reacquire_the_outer_run() {
        let trace = Arc::new(crate::simulation::engine::tests::trace());
        let clock = Arc::new(VirtualClock::new(trace.since));
        let world = Arc::new(Mutex::new(World::new(
            trace.clone(),
            clock.clone(),
            Envelope::Point,
        )));
        let adapters = FleetAdapters {
            world,
            registry: Arc::default(),
            clock,
            master: Some("a".into()),
        };
        let temp = tempfile::tempdir().unwrap();
        let member = Member::start(
            "a",
            Capacity {
                weighted_cores: Cores::from_millis(2000),
                reserved: Cores::ONE,
                capacity: Cores::ONE,
            },
            Parameters::default(),
            &Settings::parse(&trace, &[]).unwrap(),
            &adapters,
            temp.path(),
            0,
        )
        .unwrap();
        member.scheduler.tick().unwrap();
        let request = |kind, holder_pid| AdmissionRequest {
            workload: None,
            kind,
            holder_pid,
            size: Some(Cores::ONE),
            label: Some("work".into()),
            placement: Placement::Pinned,
            urgency: Urgency::Interactive {
                hold_limit_ms: Some(5000),
            },
            workspace: None,
            settings: None,
        };
        let root = RunId::parse("r-000000000001").unwrap();
        member.processes.prepare_request(100, Kind::Run, None);
        member.ids.0.lock().unwrap().push_back(root.clone());
        member
            .scheduler
            .request(100, request(Kind::Run, None))
            .await
            .unwrap();
        member.processes.insert(101, 100);
        member
            .scheduler
            .command_started(100, &root, CommandStarted { pid: 101 })
            .unwrap();
        let mut grants = Vec::new();
        for (pid, id) in [(102, "r-000000000002"), (104, "r-000000000003")] {
            let id = RunId::parse(id).unwrap();
            let holder = member
                .processes
                .prepare_request(pid, Kind::Lease, Some(101));
            member.ids.0.lock().unwrap().push_back(id.clone());
            let grant = member
                .scheduler
                .request(pid, request(Kind::Lease, holder))
                .await
                .unwrap();
            assert_eq!(grant.run_id, id);
            assert!(!grant.reused);
            assert_eq!(grant.nested_in, Some(root.clone()));
            grants.push((Processes::frame(pid), id));
        }
        for (holder, id) in grants {
            tokio::time::advance(Duration::from_millis(500)).await;
            member.processes.0.lock().unwrap().remove(&holder);
            member.scheduler.process_exited(holder);
            assert_eq!(member.scheduler.record(&id).unwrap().state, State::Ended);
            assert_eq!(
                member.scheduler.record(&root).unwrap().state,
                State::Running
            );
            assert_eq!(member.scheduler.member_state().in_use, Cores::ONE);
        }
        member
            .scheduler
            .run_ended(
                100,
                &root,
                RunEnded::Exited {
                    plan_observation: None,
                    exit_code: 0,
                    cpu_ms: None,
                    run_phases: None,
                },
            )
            .unwrap();
        assert_eq!(member.scheduler.member_state().in_use, Cores::ZERO);
    }

    #[test]
    fn nested_holder_exits_preserve_outer_capacity_until_its_command_ends() {
        use crate::workload::trace::TraceOutcome;
        let mut trace = crate::simulation::engine::tests::trace();
        trace.requests.truncate(1);
        let start = trace.since;
        trace.requests[0].fact.run_ms = Some(5000);
        trace.requests[0].fact.resources.as_mut().unwrap().ended_at =
            Some(start.after(Duration::from_secs(5)));
        let root = trace.requests[0].fact.run_id.clone();
        for (n, arrival, run) in [(2, 100, 1000), (3, 200, 1500)] {
            let mut child = trace.requests[0].clone();
            child.fact.run_id = format!("r-{n:012x}");
            child.fact.kind = Kind::Lease;
            child.outcome = Some(TraceOutcome::HolderExited);
            child.fact.requested_at = start.after(Duration::from_millis(arrival));
            child.fact.run_ms = Some(run);
            child.fact.nested = true;
            child.nested_in = Some(root.clone());
            child.shares_capacity_of = Some(root.clone());
            child.fact.resources.as_mut().unwrap().started_at = Some(child.fact.requested_at);
            child.fact.resources.as_mut().unwrap().ended_at =
                Some(child.fact.requested_at.after(Duration::from_millis(run)));
            trace.requests.push(child);
        }
        let mut later = trace.requests[0].clone();
        later.fact.run_id = "r-000000000004".into();
        later.fact.requested_at = start.after(Duration::from_secs(3));
        later.fact.waited_ms = Some(2000);
        later.fact.run_ms = Some(1000);
        later.fact.resources.as_mut().unwrap().started_at =
            Some(start.after(Duration::from_secs(5)));
        later.fact.resources.as_mut().unwrap().ended_at = Some(start.after(Duration::from_secs(6)));
        trace.requests.push(later);
        let output = simulate(trace, Options::default()).unwrap();
        assert_eq!(output["outcomes"][3]["waited_ms"], 2000);
        assert_eq!(output["metrics"]["members"]["a"]["grant_core_seconds"], 6.0);
    }

    #[test]
    fn holder_exit_releases_a_nine_core_lease_before_later_work() {
        use crate::workload::trace::TraceOutcome;
        let mut trace = crate::simulation::engine::tests::trace();
        trace.requests.truncate(2);
        let start = trace.since;
        let a = &mut trace.members[0];
        a.scheduler_settings[0].capacity = Capacity {
            weighted_cores: Cores::from_millis(12000),
            reserved: Cores::from_millis(2000),
            capacity: Cores::from_millis(10000),
        };
        a.load[0].weighted_cores = Cores::from_millis(12000);
        a.load[0].capacity = Cores::from_millis(10000);
        let lease = &mut trace.requests[0];
        lease.fact.kind = Kind::Lease;
        lease.fact.size = Cores::from_millis(9000);
        lease.fact.run_ms = Some(4002);
        lease.fact.resources.as_mut().unwrap().ended_at =
            Some(start.after(Duration::from_millis(4002)));
        lease.outcome = Some(TraceOutcome::HolderExited);
        let later = &mut trace.requests[1];
        later.fact.size = Cores::from_millis(3000);
        later.fact.requested_at = start.after(Duration::from_secs(5));
        later.fact.waited_ms = Some(0);
        later.fact.resources.as_mut().unwrap().started_at = Some(later.fact.requested_at);
        later.fact.resources.as_mut().unwrap().ended_at = Some(start.after(Duration::from_secs(6)));
        let output = simulate(trace, Options::default()).unwrap();
        assert_eq!(output["outcomes"][1]["waited_ms"], 0);
        let granted = output["metrics"]["members"]["a"]["grant_core_seconds"]
            .as_f64()
            .unwrap();
        assert!((granted - 39.018).abs() < 1e-8, "{output}");
    }

    #[test]
    fn unknown_execution_stays_unresolved_and_keeps_capacity_instead_of_teaching_zero() {
        let mut trace = crate::simulation::engine::tests::trace();
        trace.requests.truncate(2);
        let unknown = &mut trace.requests[0];
        unknown.fact.label = Some("unknown input work".into());
        unknown.fact.family = None;
        unknown.fact.run_ms = None;
        unknown.fact.unfinished = true;
        unknown.fact.completed = false;
        unknown.outcome = None;
        unknown.fact.resources.as_mut().unwrap().ended_at = None;
        let output = simulate(trace, Options::default()).unwrap();
        assert_eq!(output["outcomes"][0]["state"], "running");
        assert!(output["outcomes"][0]["run_ms"].is_null());
        assert!(output["outcomes"][0]["terminal_at"].is_null());
        assert_eq!(output["outcomes"][1]["failure"], "fleet_busy");
        assert_eq!(output["outcomes"][1]["waited_ms"], 5000);
        assert_eq!(
            output["metrics"]["members"]["a"]["grant_core_seconds"],
            60.0
        );
        assert_eq!(
            output["metrics"]["objective"]["unknown_reference_run_ids"][0],
            "r-000000000001"
        );
        assert_ne!(output["metrics"]["status"], "feasible");
    }

    #[test]
    fn nested_run_outliving_root_keeps_effective_and_declared_exposure() {
        let mut trace = crate::simulation::engine::tests::trace();
        trace.requests.truncate(1);
        let mut nested = trace.requests[0].clone();
        nested.fact.run_id = "r-000000000002".into();
        nested.fact.requested_at = nested
            .fact
            .requested_at
            .after(std::time::Duration::from_millis(1));
        nested.fact.resources.as_mut().unwrap().started_at = Some(nested.fact.requested_at);
        nested.fact.resources.as_mut().unwrap().ended_at = Some(
            nested
                .fact
                .requested_at
                .after(std::time::Duration::from_millis(10_000)),
        );
        nested.fact.run_ms = Some(10_000);
        nested.fact.resources.as_mut().unwrap().cpu_ms = Some(10_000);
        nested.fact.nested = true;
        nested.nested_in = Some(trace.requests[0].fact.run_id.clone());
        nested.shares_capacity_of = nested.nested_in.clone();
        trace.requests.push(nested);
        let output = simulate(trace, Options::default()).unwrap();
        let member = &output["metrics"]["members"]["a"];
        let effective = member["cpu_core_seconds"].as_f64().unwrap();
        let declared = member["grant_core_seconds"].as_f64().unwrap();
        assert!((effective - 10.001).abs() < 1e-8, "{output}");
        assert!((declared - 10.001).abs() < 1e-8, "{output}");
        assert_eq!(output["metrics"]["objective"]["runs"], 1);
        assert_eq!(output["outcomes"][1]["uncertain"], true);
        let upper_excess = output["sensitivity"]["upper"]["reserve"]["excess_core_seconds"]
            .as_f64()
            .unwrap();
        assert!((upper_excess - 5.0005).abs() < 1e-8, "{output}");
    }
}
