//! Admitted state at the beginning of a trace. Carry-in is hydrated through the
//! scheduler's initial-state boundary, never submitted again under later gates.
use super::{
    adapters::{Processes, workspace},
    engine::settings,
};
use crate::{
    failure::{Failure, Result},
    scheduling::{
        messages::Grant,
        placed::{PlacedRecord, Reached, Side, SyncSummary},
        processes::ProcessIdentity,
        record::{Record, State},
        request::Kind,
        run_id::RunId,
        service::Scheduler,
        timestamp::Timestamp,
    },
    workload::trace::{Request, Trace},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

fn missing(id: &str, field: &str) -> Failure {
    Failure::new(
        "initial_state_missing",
        format!("requests[{id}].{field} missing at the trace boundary."),
        "Export complete carry-in admission, execution and capacity-family state; current settings cannot reconstruct historical admission.",
    )
}

/// Recorded admission precedes sync; ResourceUse.started_at is command start.
pub fn admitted_at(request: &Request) -> Option<Timestamp> {
    request
        .fact
        .waited_ms
        .map(|wait| request.fact.requested_at.after(Duration::from_millis(wait)))
        .or_else(|| {
            request.fact.resources.as_ref()?.started_at.map(|start| {
                Timestamp::from_millis(
                    start
                        .millis()
                        .saturating_sub(request.fact.sync_ms.unwrap_or(0)),
                )
            })
        })
}

pub fn ended_at(request: &Request) -> Option<Timestamp> {
    request
        .fact
        .resources
        .as_ref()
        .and_then(|r| r.ended_at)
        .or_else(|| {
            Some(
                admitted_at(request)?.after(Duration::from_millis(
                    request
                        .fact
                        .sync_ms
                        .unwrap_or(0)
                        .saturating_add(request.fact.run_ms?),
                )),
            )
        })
}

pub fn is_active(request: &Request, at: Timestamp) -> bool {
    request.fact.requested_at < at
        && request.fact.member.is_some()
        && admitted_at(request).is_some_and(|started| started <= at)
        && ended_at(request).is_none_or(|ended| ended > at)
}

pub fn restore(
    trace: &Trace,
    at: Timestamp,
    schedulers: &BTreeMap<String, Arc<Scheduler>>,
    processes: &BTreeMap<String, Arc<Processes>>,
    pids: &BTreeMap<String, u32>,
) -> Result<Vec<(usize, Grant)>> {
    let mut waiting: BTreeSet<_> = trace
        .requests
        .iter()
        .enumerate()
        .filter(|(_, r)| is_active(r, at))
        .map(|(i, _)| i)
        .collect();
    let mut restored = BTreeSet::new();
    let mut grants = Vec::new();
    while !waiting.is_empty() {
        let next = waiting.iter().copied().find(|&i| {
            trace.requests[i]
                .shares_capacity_of
                .as_ref()
                .is_none_or(|root| restored.contains(root))
        });
        let Some(index) = next else {
            let request = &trace.requests[*waiting.first().expect("not empty")];
            return Err(missing(
                &request.fact.run_id,
                "outer capacity grant (ended or omitted ancestor cannot be inferred from nested size)",
            ));
        };
        waiting.remove(&index);
        let request = &trace.requests[index];
        let fact = &request.fact;
        let pid = *pids
            .get(&fact.run_id)
            .ok_or_else(|| missing(&fact.run_id, "virtual process ID"))?;
        let outer_frame = request
            .nested_in
            .as_ref()
            .and_then(|id| pids.get(id))
            .map(|pid| Processes::frame(*pid));
        let caller_processes = &processes[&fact.calling_member];
        caller_processes.prepare_request(pid, fact.kind, outer_frame);
        let caller_identity = ProcessIdentity {
            pid,
            started_us: u64::from(pid),
        };
        let frame = Processes::frame(pid);
        let frame_identity = ProcessIdentity {
            pid: frame,
            started_us: u64::from(frame),
        };
        let started =
            admitted_at(request).ok_or_else(|| missing(&fact.run_id, "admission timestamp"))?;
        let member = fact.member.clone().expect("active has member");
        let remote = member != fact.calling_member;
        if !remote && fact.kind == Kind::Run {
            caller_processes.insert(frame, pid);
        }
        let placed = if remote {
            let tree = workspace(request).ok_or_else(|| {
                missing(&fact.run_id, "workspace identity for active remote copy")
            })?;
            let command_started = fact
                .resources
                .as_ref()
                .and_then(|r| r.started_at)
                .is_some_and(|s| s <= at);
            Some(PlacedRecord {
                side: Side::Caller,
                calling_member: fact.calling_member.clone(),
                member: member.clone(),
                caller_workspace: tree.root,
                repository: Some(tree.common_dir),
                copy: Some("/virtual/copy".into()),
                location: Some("mapped".into()),
                warmth: fact.warmth,
                reached: if command_started {
                    Reached::Started
                } else {
                    Reached::Placed
                },
                sync: command_started.then(|| SyncSummary {
                    duration_ms: fact.sync_ms,
                    ..Default::default()
                }),
                exit_code: None,
                signal: None,
                last_contact_at: Some(at),
                leftovers: None,
                leftovers_wait_ms: None,
                interrupted: None,
            })
        } else {
            None
        };
        let expected = fact
            .predicted
            .as_ref()
            .and_then(|p| p.run_ms)
            .map(Duration::from_millis)
            .or_else(|| request.hold_limit.and_then(|h| h.expected_run_seconds));
        let record = Record {
            plan_observation: None,
            workload: None,
            run_id: RunId::parse(&fact.run_id).expect("validated ID"),
            kind: fact.kind,
            synthetic: fact.synthetic,
            internal: false,
            peer_io: None,
            label: fact.label.clone(),
            placement: request.placement_request.clone(),
            urgency: request.urgency,
            size: fact.size,
            size_basis: fact.resources.as_ref().and_then(|r| r.size_basis),
            calling_member: fact.calling_member.clone(),
            workspace: request.workspace.clone(),
            workspace_recorded: request.workspace_recorded,
            member: Some(member.clone()),
            placement_note: None,
            nested_in: request.nested_in.as_deref().and_then(RunId::parse),
            shares_capacity_of: request.shares_capacity_of.as_deref().and_then(RunId::parse),
            state: State::Running,
            outcome: None,
            requested_at: fact.requested_at,
            started_at: Some(started),
            ended_at: None,
            waited_ms: fact.waited_ms,
            run_ms: None,
            run_phases: None,
            cpu_ms: None,
            hold_limit: request.hold_limit,
            decision: request.decision.clone(),
            warnings: Vec::new(),
            requested_by: caller_identity,
            processes: if fact.kind == Kind::Lease {
                vec![frame_identity]
            } else if remote {
                vec![caller_identity]
            } else {
                vec![caller_identity, frame_identity]
            },
            placed,
            settings: settings(request),
            reacquired: 0,
            config_sync: None,
            pre_run_history: Some(Default::default()),
            fallbacks: Vec::new(),
        };
        if remote {
            let host_processes = &processes[&member];
            let command_started = fact
                .resources
                .as_ref()
                .and_then(|r| r.started_at)
                .is_some_and(|s| s <= at);
            let mut hosted = record.clone();
            hosted.placed.as_mut().expect("remote has placement").side = Side::Member;
            hosted.processes = if command_started {
                host_processes.insert(frame, 1);
                vec![frame_identity]
            } else {
                Vec::new()
            };
            schedulers[&member]
                .restore_active(hosted, expected)
                .map_err(|e| missing(&fact.run_id, &e.message))?;
        }
        let grant = schedulers[&fact.calling_member]
            .restore_active(record, expected)
            .map_err(|e| missing(&fact.run_id, &e.message))?;
        restored.insert(fact.run_id.clone());
        grants.push((index, grant));
    }
    Ok(grants)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        configuration::ServiceConfiguration,
        load_log::LoadLog,
        reporting::fixtures,
        scheduling::{
            capacity::Cores,
            clock::{Clock, VirtualClock},
            load::Gate,
            placed::{OfferReply, PlacementOffer, Warmth},
            record::Outcome,
            request::{Kind, Placement},
            service::{Adapters, Driving, Runtime},
        },
        simulation::{
            adapters::{Channel, ConfigurationSync, Ids, Observation, Registry, Speed, World},
            model::Envelope,
        },
    };
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    fn active(n: u64, caller: &str, member: &str, at: Timestamp) -> Record {
        let mut record = fixtures::base(n, "carry-in", caller);
        record.kind = Kind::Run;
        record.size = Cores::ONE;
        record.member = Some(member.into());
        record.state = State::Ended;
        record.requested_at = Timestamp::from_millis(at.millis() - 2000);
        record.started_at = Some(Timestamp::from_millis(at.millis() - 1000));
        record.ended_at = Some(at.after(Duration::from_secs(1)));
        record.waited_ms = Some(1000);
        record.run_ms = Some(2000);
        record.outcome = Some(Outcome::Exited { exit_code: 0 });
        record
    }

    #[tokio::test(start_paused = true)]
    async fn carry_in_restores_remote_local_and_nested_capacity_under_a_closed_gate() {
        let mut trace = crate::simulation::engine::tests::trace();
        let at = trace.since.after(Duration::from_secs(5));
        let mut remote = active(1, "a", "b", at);
        remote.placement = Placement::Member("b".into());
        remote.workspace = Some(crate::scheduling::record::WorkspaceIdentity {
            workspace: "a".repeat(64),
            repository: "b".repeat(64),
        });
        remote.placed = Some(PlacedRecord {
            side: Side::Caller,
            calling_member: "a".into(),
            member: "b".into(),
            caller_workspace: "/never/exported".into(),
            repository: None,
            copy: None,
            location: None,
            warmth: Some(Warmth::Warm),
            reached: Reached::Finished,
            sync: Some(SyncSummary {
                duration_ms: Some(100),
                ..Default::default()
            }),
            exit_code: Some(0),
            signal: None,
            last_contact_at: None,
            leftovers: None,
            leftovers_wait_ms: None,
            interrupted: None,
        });
        let mut nested = active(2, "b", "b", at);
        nested.requested_at = Timestamp::from_millis(at.millis() - 500);
        nested.started_at = Some(nested.requested_at);
        nested.waited_ms = Some(0);
        nested.nested_in = Some(remote.run_id.clone());
        nested.shares_capacity_of = Some(remote.run_id.clone());
        let local = active(3, "a", "a", at);
        let mut ended = active(4, "a", "a", at);
        ended.ended_at = Some(Timestamp::from_millis(at.millis() - 1));
        // Deliberately list child first; restore must order the capacity family.
        trace.requests = vec![
            Request::of(&nested).unwrap(),
            Request::of(&remote).unwrap(),
            Request::of(&local).unwrap(),
            Request::of(&ended).unwrap(),
        ];
        for member in &mut trace.members {
            member.load[0].cpu_mean = Some(1.0);
            member.load[0].cpu_max = Some(1.0);
            member.load[0].outside_mean = Some(1.0);
            member.scheduler_settings[0]
                .parameters
                .admission
                .saturated_utilization = 0.4;
        }
        let clock: Arc<dyn Clock> = Arc::new(VirtualClock::new(at));
        let trace = Arc::new(trace);
        let world = Arc::new(Mutex::new(World::new(
            trace.clone(),
            clock.clone(),
            Envelope::Point,
        )));
        let registry: Registry = Arc::default();
        let mut schedulers = BTreeMap::new();
        let mut tables = BTreeMap::new();
        let mut tasks = Vec::new();
        let temp = tempfile::tempdir().unwrap();
        for member in &trace.members {
            let processes = Arc::new(Processes::default());
            let (_, exits) = mpsc::unbounded_channel();
            let (scheduler, task) = Scheduler::start_with_runtime(
                &temp.path().join(format!("{}.sqlite", member.member)),
                ServiceConfiguration {
                    reserved: if member.member == "a" {
                        Cores::from_millis(1500)
                    } else {
                        Cores::ONE
                    },
                    parameters: member.scheduler_settings[0].parameters.clone(),
                    ..Default::default()
                },
                Adapters {
                    machine: Arc::new(Observation {
                        world: world.clone(),
                        member: member.member.clone(),
                        cores: crate::inspection::CoreCounts {
                            performance: 2,
                            efficiency: 0,
                        },
                    }),
                    speed: Arc::new(Speed(1.0)),
                    processes: processes.clone(),
                    exits,
                },
                Arc::new(Channel {
                    own: member.member.clone(),
                    master: Some("a".into()),
                    registry: registry.clone(),
                    world: world.clone(),
                }),
                Arc::new(ConfigurationSync {
                    world: world.clone(),
                    caller: member.member.clone(),
                }),
                Arc::new(
                    LoadLog::open(&temp.path().join(format!("{}-load.sqlite", member.member)))
                        .unwrap(),
                ),
                Runtime {
                    clock: clock.clone(),
                    ids: Arc::new(Ids::default()),
                    driving: Driving::Controlled,
                },
            )
            .unwrap();
            registry
                .lock()
                .unwrap()
                .insert(member.member.clone(), Arc::downgrade(&scheduler));
            scheduler.tick().unwrap();
            assert!(matches!(
                scheduler.member_state().gate,
                Gate::Saturated { .. }
            ));
            tables.insert(member.member.clone(), processes);
            schedulers.insert(member.member.clone(), scheduler);
            tasks.push(task);
        }
        let pids = trace
            .requests
            .iter()
            .enumerate()
            .map(|(i, r)| (r.fact.run_id.clone(), 100 + i as u32 * 2))
            .collect();
        let restored = restore(&trace, at, &schedulers, &tables, &pids).unwrap();
        assert_eq!(restored.len(), 3, "ended request must not be re-admitted");
        assert_eq!(
            schedulers["a"].member_state().in_use,
            Cores::ONE,
            "remote caller consumes no local capacity"
        );
        assert_eq!(
            schedulers["a"].member_state().capacity.capacity,
            Cores::from_millis(500),
            "candidate capacity reductions do not re-decide historical carry-in"
        );
        assert_eq!(
            schedulers["b"].member_state().in_use,
            Cores::ONE,
            "nested shares hosted root's capacity"
        );
        assert!(matches!(
            schedulers["a"].member_state().gate,
            Gate::Saturated { .. }
        ));
        let remote_grant = &restored
            .iter()
            .find(|(i, _)| trace.requests[*i].fact.run_id == remote.run_id.as_str())
            .unwrap()
            .1;
        assert_eq!(remote_grant.member, "b");
        assert!(remote_grant.placed.is_some());
        assert_eq!(remote_grant.waited_ms, 1000);
        assert_eq!(
            schedulers["a"].record(&remote.run_id).unwrap().started_at,
            remote.started_at
        );
        assert!(
            schedulers["b"]
                .record(&remote.run_id)
                .unwrap()
                .placed
                .is_some_and(|p| p.side == Side::Member)
        );
        let quote = schedulers["b"].quote_state();
        assert_eq!(quote.holding.len(), 1);
        assert_eq!(quote.holding[0].running_ms, 1000);
        // The host still refuses a fresh offer, rather than having lost carry-in
        // when its current outside load would refuse the old request.
        let offer = PlacementOffer {
            workload: None,
            run_id: RunId::parse("r-000000000099").unwrap(),
            size: Cores::ONE,
            label: None,
            workspace: crate::scheduling::placed::WorkspaceRef {
                root: "/virtual/new".into(),
                common_dir: "/virtual/new/.git".into(),
            },
            start_within_ms: 0,
            size_basis: None,
            expected_run_ms: None,
        };
        assert!(matches!(
            schedulers["b"]
                .accept_placed("a", &offer, true, "/virtual/copy", "mapped", Warmth::Warm)
                .unwrap(),
            OfferReply::Busy { .. }
        ));
        drop(tasks);
    }

    #[test]
    fn admission_and_execution_timestamps_keep_sync_distinct() {
        let mut request = crate::simulation::engine::tests::trace().requests[0].clone();
        request.fact.requested_at = Timestamp::from_millis(1000);
        request.fact.waited_ms = Some(200);
        request.fact.sync_ms = Some(300);
        request.fact.run_ms = Some(500);
        request.fact.resources = None;
        assert_eq!(admitted_at(&request), Some(Timestamp::from_millis(1200)));
        assert_eq!(ended_at(&request), Some(Timestamp::from_millis(2000)));
        assert!(is_active(&request, Timestamp::from_millis(1400)));
        assert!(!is_active(&request, Timestamp::from_millis(2000)));
    }
}
