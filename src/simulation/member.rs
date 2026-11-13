//! Lifecycle of one virtual service. Recorded settings changes use the same
//! shutdown and durable recovery as a production service restart.
use super::{
    adapters::{Channel, ConfigurationSync, Ids, Observation, Processes, Registry, Speed, World},
    engine::invalid,
    settings::Settings,
};
use crate::{
    configuration::ServiceConfiguration,
    failure::Result,
    load_log::LoadLog,
    scheduling::{
        capacity::Capacity,
        clock::Clock,
        parameters::Parameters,
        records::SchedulerSettings,
        service::{Adapters, Driving, Runtime, Scheduler, Tasks},
    },
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

#[derive(Clone)]
pub(super) struct FleetAdapters {
    pub world: Arc<Mutex<World>>,
    pub registry: Registry,
    pub clock: Arc<dyn Clock>,
    pub master: Option<String>,
}

pub(super) struct Member {
    pub scheduler: Arc<Scheduler>,
    pub _tasks: Tasks,
    pub processes: Arc<Processes>,
    pub ids: Arc<Ids>,
    pub capacity: Capacity,
    pub baseline_capacity: Capacity,
    /// Frozen physical fallback priors; candidate settings and later changes to
    /// estimate priors do not change the amount of input work being simulated.
    pub physics_parameters: Parameters,
    pub records: PathBuf,
    pub load_log: Arc<LoadLog>,
    name: String,
    adapters: FleetAdapters,
}

impl Member {
    pub fn start(
        name: &str,
        baseline_capacity: Capacity,
        base_parameters: Parameters,
        candidates: &Settings,
        adapters: &FleetAdapters,
        scratch: &Path,
        index: usize,
    ) -> Result<Self> {
        let capacity = candidates.capacity(name, &baseline_capacity)?;
        let parameters = candidates.parameters(&base_parameters)?;
        let processes = Arc::new(Processes::default());
        let ids = Arc::new(Ids::default());
        let records = scratch.join(format!("records-{index}.sqlite"));
        let load_log = Arc::new(LoadLog::open(
            &scratch.join(format!("load-{index}.sqlite")),
        )?);
        let (scheduler, tasks) = start_scheduler(
            name,
            capacity,
            parameters,
            adapters,
            (&records, load_log.clone()),
            processes.clone(),
            ids.clone(),
        )?;
        adapters
            .registry
            .lock()
            .unwrap()
            .insert(name.to_owned(), Arc::downgrade(&scheduler));
        Ok(Self {
            scheduler,
            _tasks: tasks,
            processes,
            ids,
            capacity,
            baseline_capacity,
            physics_parameters: base_parameters,
            records,
            load_log,
            name: name.to_owned(),
            adapters: adapters.clone(),
        })
    }

    /// In-memory scheduler work must drain, except when every ready offer is
    /// suspended in recorded external I/O whose deadline is still in the future.
    pub fn has_ready_scheduler_work(&self) -> bool {
        if !self.scheduler.has_immediate_work() {
            return false;
        }
        let now = self.adapters.clock.now().millis();
        if self
            .scheduler
            .next_event_at()
            .is_some_and(|at| at.millis() <= now)
        {
            return true;
        }
        let ready = self.scheduler.ready_offer_targets();
        if ready.is_empty() {
            return true;
        }
        let world = self.adapters.world.lock().unwrap();
        !ready.iter().all(|target| {
            world
                .pending_pre_run
                .get(&(self.name.clone(), target.clone()))
                .is_some_and(|deadline| *deadline > now)
        })
    }

    /// Local live grants survive real recovery; placed connections are closed
    /// with connection_lost and waiting requests receive service_unavailable.
    /// The event driver observes those records before continuing execution.
    pub fn restart(&mut self, observed: &SchedulerSettings, candidates: &Settings) -> Result<()> {
        let capacity = candidates.capacity(&self.name, &observed.capacity)?;
        let parameters = candidates.parameters(&observed.parameters)?;
        // Validate first: a rejected candidate must not stop the running service.
        core_types(capacity)?;
        self.scheduler.shutdown();
        let (scheduler, tasks) = start_scheduler(
            &self.name,
            capacity,
            parameters,
            &self.adapters,
            (&self.records, self.load_log.clone()),
            self.processes.clone(),
            self.ids.clone(),
        )?;
        self.adapters
            .registry
            .lock()
            .unwrap()
            .insert(self.name.clone(), Arc::downgrade(&scheduler));
        self.scheduler = scheduler;
        self._tasks = tasks;
        self.capacity = capacity;
        self.baseline_capacity = observed.capacity;
        Ok(())
    }
}

fn core_types(capacity: Capacity) -> Result<crate::inspection::CoreCounts> {
    let weighted = capacity.weighted_cores.millis();
    if !weighted.is_multiple_of(500) {
        return Err(invalid(
            "Weighted capacity cannot be represented by machine core types.",
        ));
    }
    Ok(crate::inspection::CoreCounts {
        performance: weighted / 1000,
        efficiency: weighted % 1000 / 500,
    })
}

fn start_scheduler(
    name: &str,
    capacity: Capacity,
    parameters: Parameters,
    adapters: &FleetAdapters,
    persistence: (&Path, Arc<LoadLog>),
    processes: Arc<Processes>,
    ids: Arc<Ids>,
) -> Result<(Arc<Scheduler>, Tasks)> {
    let cores = core_types(capacity)?;
    let (records, load_log) = persistence;
    let (_, exits) = mpsc::unbounded_channel();
    Scheduler::start_with_runtime(
        records,
        ServiceConfiguration {
            reserved: capacity.reserved,
            parameters,
            ..Default::default()
        },
        Adapters {
            machine: Arc::new(Observation {
                world: adapters.world.clone(),
                member: name.to_owned(),
                cores,
            }),
            speed: Arc::new(Speed(1.0)),
            processes,
            exits,
        },
        Arc::new(Channel {
            own: name.to_owned(),
            master: adapters.master.clone(),
            registry: adapters.registry.clone(),
            world: adapters.world.clone(),
        }),
        Arc::new(ConfigurationSync {
            world: adapters.world.clone(),
            caller: name.to_owned(),
        }),
        load_log,
        Runtime {
            clock: adapters.clock.clone(),
            ids,
            driving: Driving::Controlled,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        reporting::fixtures,
        scheduling::{
            capacity::Cores,
            clock::VirtualClock,
            placed::Side,
            processes::ProcessIdentity,
            record::{Outcome, State},
            request::{AdmissionRequest, Kind, Placement, Urgency},
            run_id::RunId,
        },
        simulation::model::Envelope,
    };
    use std::time::Duration;

    fn request() -> AdmissionRequest {
        AdmissionRequest {
            workload: None,
            kind: Kind::Run,
            holder_pid: None,
            size: Some(Cores::ONE),
            label: Some("restart fixture".into()),
            placement: Placement::Pinned,
            urgency: Urgency::Interactive {
                hold_limit_ms: Some(5000),
            },
            workspace: None,
            settings: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn recorded_restart_uses_real_recovery_preserving_local_and_closing_remote_work() {
        let trace = Arc::new(crate::simulation::engine::tests::trace());
        let clock: Arc<dyn Clock> = Arc::new(VirtualClock::new(trace.since));
        let adapters = FleetAdapters {
            world: Arc::new(Mutex::new(World::new(
                trace.clone(),
                clock.clone(),
                Envelope::Point,
            ))),
            registry: Arc::default(),
            clock: clock.clone(),
            master: Some("a".into()),
        };
        let candidates = Settings::parse(&trace, &[]).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let baseline = trace.members[0].scheduler_settings[0].clone();
        let mut a = Member::start(
            "a",
            baseline.capacity,
            baseline.parameters.clone(),
            &candidates,
            &adapters,
            temp.path(),
            0,
        )
        .unwrap();
        let mut b = Member::start(
            "b",
            baseline.capacity,
            baseline.parameters.clone(),
            &candidates,
            &adapters,
            temp.path(),
            1,
        )
        .unwrap();
        a.processes.insert(101, 1);
        a.processes.insert(100, 101);
        let local_id = RunId::parse("r-000000000091").unwrap();
        a.ids.0.lock().unwrap().push_back(local_id.clone());
        let local = a.scheduler.request(100, request()).await.unwrap();
        assert_eq!(local.run_id, local_id);
        let remote_id = RunId::parse("r-000000000092").unwrap();
        let mut remote = fixtures::placed(fixtures::Run {
            member: "b",
            ..Default::default()
        });
        remote.run_id = remote_id.clone();
        remote.calling_member = "a".into();
        remote.member = Some("b".into());
        remote.size = Cores::ONE;
        remote.state = State::Running;
        remote.requested_at = clock.now();
        remote.started_at = Some(clock.now());
        remote.ended_at = None;
        remote.outcome = None;
        remote.run_ms = None;
        let process = ProcessIdentity {
            pid: 200,
            started_us: 200,
        };
        remote.requested_by = process;
        remote.processes = vec![process];
        remote.placed.as_mut().unwrap().calling_member = "a".into();
        remote.placed.as_mut().unwrap().copy = Some("/fixture/copy".into());
        a.processes.insert(200, 1);
        b.processes.insert(200, 1);
        let mut hosted = remote.clone();
        hosted.placed.as_mut().unwrap().side = Side::Member;
        b.scheduler.restore_active(hosted, None).unwrap();
        a.scheduler.restore_active(remote, None).unwrap();
        let waiting_id = RunId::parse("r-000000000093").unwrap();
        a.processes.insert(300, 301);
        a.processes.insert(301, 1);
        a.ids.0.lock().unwrap().push_back(waiting_id);
        let waiting = tokio::spawn({
            let scheduler = a.scheduler.clone();
            async move { scheduler.request(300, request()).await }
        });
        while a.scheduler.member_state().queued.interactive != 1 {
            tokio::task::yield_now().await;
        }
        let before = a.scheduler.clone();
        tokio::time::advance(Duration::from_millis(500)).await;
        let mut changed = baseline.clone();
        changed.at = clock.now();
        changed.capacity.reserved = Cores::from_millis(500);
        changed.capacity.capacity = Cores::from_millis(1500);
        changed.parameters.placement.keep_local_margin = 0.2;
        a.restart(&changed, &candidates).unwrap();
        assert!(!Arc::ptr_eq(&before, &a.scheduler));
        assert!(Arc::ptr_eq(
            &adapters.registry.lock().unwrap()["a"].upgrade().unwrap(),
            &a.scheduler
        ));
        assert_eq!(
            waiting.await.unwrap().unwrap_err().code,
            "service_unavailable"
        );
        assert_eq!(a.scheduler.member_state().in_use, Cores::ONE);
        assert_eq!(
            a.scheduler.member_state().capacity.capacity,
            Cores::from_millis(1500)
        );
        assert_eq!(a.scheduler.parameters().placement.keep_local_margin, 0.2);
        assert_eq!(a.physics_parameters.placement.keep_local_margin, 0.1);
        assert_eq!(a.scheduler.record(&local_id).unwrap().state, State::Running);
        assert_eq!(
            a.scheduler.record(&remote_id).unwrap().outcome,
            Some(Outcome::Failed {
                code: "connection_lost".into()
            })
        );
        assert_eq!(
            a.scheduler
                .settings_history(trace.since, clock.now().after(Duration::from_millis(1)))
                .unwrap()
                .len(),
            2
        );
        b.restart(
            &SchedulerSettings {
                at: clock.now(),
                ..baseline
            },
            &candidates,
        )
        .unwrap();
        assert_eq!(b.scheduler.member_state().in_use, Cores::ZERO);
        assert!(
            !b.processes.0.lock().unwrap().contains_key(&200),
            "real hosted restart stops the command through the process boundary"
        );
        assert_eq!(
            b.scheduler.record(&remote_id).unwrap().outcome,
            Some(Outcome::Failed {
                code: "connection_lost".into()
            })
        );
    }
}
