//! Which processes requests are bound to, so a process's exit reaches every
//! request it waits for or holds.
use crate::scheduling::{
    processes::{self, ProcessIdentity, ProcessTable},
    run_id::RunId,
};
use std::collections::{BTreeSet, HashMap};

#[derive(Default)]
pub struct Bindings {
    by_pid: HashMap<u32, Binding>,
}
struct Binding {
    process: ProcessIdentity,
    runs: BTreeSet<RunId>,
}

/// Requests whose bound process is gone.
pub type Lost = Vec<(RunId, ProcessIdentity)>;

impl Bindings {
    /// Binds `id` to `process` and watches it. `None` if the process already
    /// exited. Otherwise returns requests bound to an earlier process with the
    /// same PID, which must have exited unnoticed.
    pub fn bind(
        &mut self,
        table: &dyn ProcessTable,
        id: &RunId,
        process: ProcessIdentity,
    ) -> Option<Lost> {
        if !table.watch(process) {
            return None;
        }
        let mut lost = Vec::new();
        let binding = self.by_pid.entry(process.pid).or_insert_with(|| Binding {
            process,
            runs: BTreeSet::new(),
        });
        if binding.process != process {
            let earlier = std::mem::replace(
                binding,
                Binding {
                    process,
                    runs: BTreeSet::new(),
                },
            );
            lost.extend(earlier.runs.into_iter().map(|run| (run, earlier.process)));
        }
        binding.runs.insert(id.clone());
        Some(lost)
    }

    pub fn unbind(&mut self, id: &RunId, process: ProcessIdentity) {
        if let Some(binding) = self.by_pid.get_mut(&process.pid)
            && binding.process == process
        {
            binding.runs.remove(id);
            if binding.runs.is_empty() {
                self.by_pid.remove(&process.pid);
            }
        }
    }

    /// The process with `pid` was reported to have exited. A report that arrives
    /// after the PID was reused by a live process that is bound now is stale.
    pub fn exited(&mut self, table: &dyn ProcessTable, pid: u32) -> Lost {
        match self.by_pid.get(&pid) {
            Some(binding) if !processes::alive(table, binding.process) => {
                let binding = self.by_pid.remove(&pid).expect("checked above");
                binding
                    .runs
                    .into_iter()
                    .map(|run| (run, binding.process))
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// Requests bound to this exact process.
    pub fn runs_of(&self, process: ProcessIdentity) -> impl Iterator<Item = &RunId> {
        self.by_pid
            .get(&process.pid)
            .filter(|binding| binding.process == process)
            .into_iter()
            .flat_map(|binding| &binding.runs)
    }
}

#[cfg(test)]
pub mod fake {
    //! A process table whose processes the test starts and ends.
    use crate::scheduling::processes::{ProcessIdentity, ProcessInfo, ProcessTable};
    use std::{collections::HashMap, sync::Mutex};

    #[derive(Default)]
    pub struct FakeProcesses(pub Mutex<HashMap<u32, ProcessInfo>>);
    impl FakeProcesses {
        pub fn start(&self, pid: u32, started_us: u64, parent: u32) -> ProcessIdentity {
            let identity = ProcessIdentity { pid, started_us };
            self.0
                .lock()
                .unwrap()
                .insert(pid, ProcessInfo { identity, parent });
            identity
        }
        pub fn end(&self, pid: u32) {
            self.0.lock().unwrap().remove(&pid);
        }
    }
    impl ProcessTable for FakeProcesses {
        fn describe(&self, pid: u32) -> Option<ProcessInfo> {
            self.0.lock().unwrap().get(&pid).copied()
        }
        fn watch(&self, process: ProcessIdentity) -> bool {
            self.describe(process.pid)
                .is_some_and(|info| info.identity == process)
        }
        fn kill_group(&self, process: ProcessIdentity, _group: u32) -> bool {
            let alive = self.watch(process);
            if alive {
                self.end(process.pid);
            }
            alive
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{fake::FakeProcesses, *};

    fn id(n: u8) -> RunId {
        RunId::parse(&format!("r-{n:012x}")).unwrap()
    }

    #[test]
    fn exits_reach_every_bound_request_once() {
        let table = FakeProcesses::default();
        let mut bindings = Bindings::default();
        let shell = table.start(10, 1, 1);
        assert_eq!(bindings.bind(&table, &id(1), shell), Some(vec![]));
        assert_eq!(bindings.bind(&table, &id(2), shell), Some(vec![]));
        bindings.unbind(&id(2), shell);
        assert_eq!(bindings.runs_of(shell).collect::<Vec<_>>(), vec![&id(1)]);
        assert!(bindings.exited(&table, 10).is_empty(), "still alive");
        table.end(10);
        assert_eq!(bindings.exited(&table, 10), vec![(id(1), shell)]);
        assert!(bindings.exited(&table, 10).is_empty());
        assert_eq!(bindings.bind(&table, &id(3), shell), None, "already exited");
    }

    #[test]
    fn a_reused_pid_neither_leaks_nor_ends_the_wrong_request() {
        let table = FakeProcesses::default();
        let mut bindings = Bindings::default();
        let first = table.start(10, 1, 1);
        bindings.bind(&table, &id(1), first);
        // The first process exits, its PID is reused, and the exit report is late.
        table.end(10);
        let second = table.start(10, 2, 1);
        assert_eq!(
            bindings.bind(&table, &id(2), second),
            Some(vec![(id(1), first)]),
            "the earlier request is reported lost, not leaked"
        );
        assert!(
            bindings.exited(&table, 10).is_empty(),
            "the late report does not end the live process's request"
        );
        assert_eq!(bindings.runs_of(first).count(), 0);
        assert_eq!(bindings.runs_of(second).collect::<Vec<_>>(), vec![&id(2)]);
    }
}
