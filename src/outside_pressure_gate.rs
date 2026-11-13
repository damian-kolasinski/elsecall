//! The scheduler's gate as outside pressure detection sees it: the gate, and
//! the processes whose descendants are Elsecall's own work.
use crate::{
    outside_pressure::{GateSource, Observation, attribution::OwnWork},
    scheduling::service::Scheduler,
};
use std::sync::Arc;

pub struct SchedulerGate(pub Arc<Scheduler>);
impl GateSource for SchedulerGate {
    fn observe(&self) -> Observation {
        let own = self.0.own_load();
        // The service runs placed commands and housekeeping itself; local runs
        // descend from the holders of their grants, and what they start outside
        // that tree is told apart by when it started.
        let mut roots = own.holders;
        roots.push(std::process::id());
        Observation {
            at: own.at,
            gate: own.gate,
            own: OwnWork {
                roots,
                granted_since: own.granted_since,
            },
        }
    }
}
