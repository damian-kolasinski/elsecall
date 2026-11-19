//! Admission as an upgrade sees it: the scheduler's work in flight that a restart
//! would cut short, and its holds, behind the upgrade's `MemberWork` port.
use crate::{
    fleet_upgrade::member::{MemberWork, WorkInFlight, WorkKind, WorkState},
    scheduling::service::Scheduler,
};
use elsecall_scheduling_model::request::Kind;
use std::sync::Arc;

pub struct SchedulerWork(pub Arc<Scheduler>);
impl MemberWork for SchedulerWork {
    fn in_flight(&self) -> Vec<WorkInFlight> {
        self.0
            .in_flight()
            .into_iter()
            .filter(|work| !work.outlasts_restart)
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
    fn hold_install(&self) -> Option<Box<dyn Send>> {
        self.0
            .hold_update_when_restartable()
            .map(|hold| Box::new(hold) as Box<dyn Send>)
    }
}
