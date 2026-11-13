//! Virtual process identities, ancestry and lifetime at the process-table port.
use crate::scheduling::{
    processes::{ProcessIdentity, ProcessInfo, ProcessTable},
    request::Kind,
};
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Default)]
pub(super) struct Processes(pub Mutex<BTreeMap<u32, ProcessInfo>>);
impl Processes {
    /// Each trace request owns a requester and a separate execution frame.
    /// Descendants inherit the frame, never another request's holder identity.
    pub(super) fn frame(pid: u32) -> u32 {
        pid + 1
    }
    pub(super) fn prepare_request(
        &self,
        pid: u32,
        kind: Kind,
        outer_frame: Option<u32>,
    ) -> Option<u32> {
        let outer = outer_frame.unwrap_or(1);
        match kind {
            Kind::Run => {
                self.insert(pid, outer);
                None
            }
            Kind::Lease => {
                let holder = Self::frame(pid);
                self.insert(holder, outer);
                self.insert(pid, holder);
                Some(holder)
            }
        }
    }
    pub(super) fn insert(&self, pid: u32, parent: u32) {
        self.0.lock().unwrap().insert(
            pid,
            ProcessInfo {
                identity: ProcessIdentity {
                    pid,
                    started_us: u64::from(pid),
                },
                parent,
            },
        );
    }
}
impl ProcessTable for Processes {
    fn describe(&self, pid: u32) -> Option<ProcessInfo> {
        self.0.lock().unwrap().get(&pid).copied()
    }
    fn watch(&self, p: ProcessIdentity) -> bool {
        self.describe(p.pid).is_some_and(|i| i.identity == p)
    }
    fn kill_group(&self, p: ProcessIdentity, _: u32) -> bool {
        self.0.lock().unwrap().remove(&p.pid).is_some()
    }
}
