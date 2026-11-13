//! The processes that hold or request capacity. Admission binds grants to them so
//! capacity is released when they exit, however they exit.
use serde::{Deserialize, Serialize};

/// A process, distinguished from a later one that reuses its PID by its start time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started_us: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    pub identity: ProcessIdentity,
    pub parent: u32,
}

/// Reads and watches processes of this machine. Exits are reported by PID on a
/// channel the adapter hands out when it starts.
pub trait ProcessTable: Send + Sync {
    /// A running process; `None` once it exited or when it cannot be read.
    fn describe(&self, pid: u32) -> Option<ProcessInfo>;
    /// Starts watching for the process's exit. False if it has already exited, in
    /// which case no exit is reported.
    fn watch(&self, process: ProcessIdentity) -> bool;
    /// Kills process group `group` if `process` still runs in it, and says
    /// whether it did; for placed commands, which lead their group, and what they
    /// left running, once their caller is gone for good.
    fn kill_group(&self, process: ProcessIdentity, group: u32) -> bool;
}

/// How far up the tree lineage looks; real process trees are far shallower.
const MAXIMUM_DEPTH: usize = 128;

/// The process and its ancestors, nearest first, stopping below launchd (PID 1)
/// or at the first process that cannot be read.
pub fn lineage(table: &dyn ProcessTable, pid: u32) -> Vec<ProcessInfo> {
    let mut lineage = Vec::new();
    let mut next = pid;
    while next > 1 && lineage.len() < MAXIMUM_DEPTH {
        let Some(info) = table.describe(next) else {
            break;
        };
        lineage.push(info);
        next = info.parent;
    }
    lineage
}

pub fn alive(table: &dyn ProcessTable, process: ProcessIdentity) -> bool {
    table
        .describe(process.pid)
        .is_some_and(|info| info.identity == process)
}
