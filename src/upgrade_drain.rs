//! Admission as an upgrade sees it: the scheduler's work in flight, and its hold on
//! placed runs, behind the upgrade's `MemberWork` port.
use crate::{
    fleet_upgrade::member::{MemberWork, WorkInFlight, WorkKind, WorkState},
    scheduling::{request::Kind, service::Scheduler},
};
use std::sync::Arc;

pub struct SchedulerWork(pub Arc<Scheduler>);
impl MemberWork for SchedulerWork {
    fn in_flight(&self) -> Vec<WorkInFlight> {
        self.0
            .in_flight()
            .into_iter()
            .map(|work| WorkInFlight {
                run_id: work.run_id,
                kind: match work.kind {
                    Kind::Run => WorkKind::Run,
                    Kind::Lease => WorkKind::Lease,
                },
                label: work.label,
                state: if work.running {
                    WorkState::Running
                } else {
                    WorkState::Queued
                },
                placed: work.hosted,
                internal: work.internal,
            })
            .collect()
    }
    fn hold_placed(&self) -> Box<dyn Send> {
        Box::new(self.0.hold_hosting())
    }
    fn hold_internal(&self) -> Box<dyn Send> {
        Box::new(self.0.hold_internal())
    }
}
