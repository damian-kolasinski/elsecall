//! What survives a service restart: grants whose processes still run keep their
//! capacity, so it is never handed out twice; everything else is closed.
use crate::scheduling::{
    capacity::Cores,
    processes::ProcessIdentity,
    record::{Outcome, Record, State},
    run_id::RunId,
    timestamp::Timestamp,
};
use std::collections::{BTreeMap, BTreeSet};

/// A surviving grant and the nested grants that share its capacity.
pub struct Family {
    pub root: RunId,
    pub size: Cores,
    /// Whether the root itself survived, or only work nested in it.
    pub root_alive: bool,
    pub members: Vec<Survivor>,
}
pub struct Survivor {
    pub record: Record,
    pub alive: BTreeSet<ProcessIdentity>,
}
pub struct Recovery {
    /// Records closed now, to be saved.
    pub closed: Vec<Record>,
    pub families: Vec<Family>,
}

/// Plans recovery of the requests that had not ended when the service stopped.
/// `size_of` reads the size of an ended root whose nested work survived.
pub fn plan(
    unfinished: Vec<Record>,
    now: Timestamp,
    alive: impl Fn(ProcessIdentity) -> bool,
    size_of: impl Fn(&RunId) -> Option<Cores>,
) -> Recovery {
    let mut closed = Vec::new();
    let mut families: BTreeMap<RunId, Vec<Survivor>> = BTreeMap::new();
    for mut record in unfinished {
        let survivors: BTreeSet<_> = record
            .processes
            .iter()
            .copied()
            .filter(|p| alive(*p))
            .collect();
        if record.state == State::Running && !survivors.is_empty() {
            let root = record
                .shares_capacity_of
                .clone()
                .unwrap_or_else(|| record.run_id.clone());
            families.entry(root).or_default().push(Survivor {
                record,
                alive: survivors,
            });
            continue;
        }
        if record.workload.is_some() && record.plan_observation.is_none() {
            record.plan_observation = Some(if record.state == State::Running {
                crate::workload_metadata::Observation::Censored {
                    coverage: None,
                    reason: crate::workload_metadata::CensorReason::ServiceRestart,
                }
            } else {
                crate::workload_metadata::Observation::Unknown {}
            });
        }
        record.outcome = Some(match record.state {
            State::Queued => Outcome::Cancelled {
                reason: "The service stopped while the request waited.".into(),
            },
            _ => Outcome::HolderExited,
        });
        record.state = State::Ended;
        record.ended_at = Some(now);
        record.run_ms = record
            .started_at
            .map(|started| now.since(started).as_millis() as u64);
        closed.push(record);
    }
    let families = families
        .into_iter()
        .map(|(root, members)| {
            let own = members.iter().find(|m| m.record.run_id == root);
            let size = own
                .map(|m| m.record.size)
                .or_else(|| size_of(&root))
                .unwrap_or_else(|| members.iter().map(|m| m.record.size).max().unwrap());
            Family {
                root_alive: own.is_some(),
                root,
                size,
                members,
            }
        })
        .collect();
    Recovery { closed, families }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::request::{Kind, Placement, Urgency};

    fn process(pid: u32) -> ProcessIdentity {
        ProcessIdentity { pid, started_us: 1 }
    }
    fn record(n: u8, state: State, size: u32, pid: u32, root: Option<u8>) -> Record {
        let id = |n: u8| RunId::parse(&format!("r-{n:012x}")).unwrap();
        Record {
            plan_observation: None,
            workload: None,
            run_id: id(n),
            kind: Kind::Lease,
            synthetic: false,
            internal: false,
            peer_io: None,
            label: None,
            placement: Placement::Pinned,
            urgency: Urgency::Interactive {
                hold_limit_ms: None,
            },
            size: Cores::from_millis(size * 1000),
            size_basis: None,
            calling_member: "m".into(),
            workspace: None,
            workspace_recorded: true,
            member: None,
            placement_note: None,
            nested_in: root.map(id),
            shares_capacity_of: root.map(id),
            state,
            outcome: None,
            requested_at: Timestamp::from_millis(0),
            started_at: Some(Timestamp::from_millis(0)),
            ended_at: None,
            waited_ms: None,
            run_ms: None,
            run_phases: None,
            cpu_ms: None,
            hold_limit: None,
            decision: None,
            warnings: Vec::new(),
            requested_by: process(pid + 1000),
            processes: vec![process(pid)],
            placed: None,
            settings: None,
            reacquired: 0,
            config_sync: None,
            pre_run_history: Some(Default::default()),
            fallbacks: Vec::new(),
        }
    }

    #[test]
    fn keeps_surviving_families_and_closes_the_rest() {
        let unfinished = vec![
            record(1, State::Queued, 1, 10, None),
            record(2, State::Running, 2, 20, None),
            record(3, State::Running, 1, 30, Some(2)),
            // Nested work whose root ended before the restart.
            record(5, State::Running, 1, 50, Some(4)),
            record(6, State::Running, 1, 60, None),
        ];
        let alive = |p: ProcessIdentity| [20, 30, 50].contains(&p.pid);
        let recovery = plan(unfinished, Timestamp::from_millis(5000), alive, |id| {
            (id.as_str() == "r-000000000004").then_some(Cores::from_millis(3000))
        });
        let closed: Vec<_> = recovery
            .closed
            .iter()
            .map(|r| (r.run_id.as_str(), r.outcome.clone().unwrap()))
            .collect();
        assert_eq!(closed.len(), 2);
        assert!(matches!(
            closed[0],
            ("r-000000000001", Outcome::Cancelled { .. })
        ));
        assert_eq!(closed[1], ("r-000000000006", Outcome::HolderExited));
        assert_eq!(recovery.closed[1].run_ms, Some(5000));
        let families: Vec<_> = recovery
            .families
            .iter()
            .map(|f| (f.root.as_str(), f.size, f.root_alive, f.members.len()))
            .collect();
        assert_eq!(
            families,
            vec![
                ("r-000000000002", Cores::from_millis(2000), true, 2),
                ("r-000000000004", Cores::from_millis(3000), false, 1),
            ]
        );
    }
}
