//! Release discovery, job admission, and the service side of an install handoff.
use super::{
    Discovery, FleetDistribution, FleetFollowUp, FollowUpState, Job, JobId, Phase, ReleaseCatalog,
    ReleaseVersion, Status, invalid, now, store::Store,
};
use crate::{
    build_identity::BuildId,
    paths::Paths,
    private_files,
    scheduling::service::{Scheduler, UpdateHold},
};
use elsecall_failure::{Failure, Result};
use std::{
    fs,
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub trait HelperLauncher: Send + Sync {
    fn start(&self, paths: &Paths, job: &Job) -> Result<()>;
}
pub struct DetachedHelper;
impl HelperLauncher for DetachedHelper {
    fn start(&self, paths: &Paths, job: &Job) -> Result<()> {
        let store = Store::new(paths);
        let worker = store.worker(&job.id);
        let bytes =
            fs::read(paths.executable()).map_err(|e| Failure::io("Read update helper", e))?;
        if crate::build_identity::Digest::of(&bytes).build_id() != job.from_build {
            return Err(invalid(
                "The installed CLI changed while preparing the update; retry.",
            ));
        }
        private_files::write(&worker, &bytes, 0o700)?;
        let mut command = Command::new(&worker);
        command
            .args(["update-helper", "--id", job.id.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_remove("ELSECALL_ADMISSION");
        // The helper must survive launchd unregistering the service that started it.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = crate::child_start::spawn(&mut command)
            .map_err(|e| Failure::io("Start release helper", e))?;
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = fs::remove_file(worker);
        });
        Ok(())
    }
}

pub struct UpdateService {
    paths: Paths,
    current: ReleaseVersion,
    running_build: BuildId,
    store: Store,
    discovery: Mutex<Discovery>,
    checking: tokio::sync::Mutex<()>,
    preparing: tokio::sync::Mutex<()>,
    catalog: Arc<dyn ReleaseCatalog>,
    launcher: Arc<dyn HelperLauncher>,
    scheduler: Arc<Scheduler>,
    hold: Mutex<Option<UpdateHold>>,
    /// Set on a member whose fleet state is readable; see [`Self::distribute_with`].
    fleet: OnceLock<Arc<dyn FleetDistribution>>,
    /// Whether this process is distributing the last update's build to the fleet.
    distributing: Arc<AtomicBool>,
}
impl UpdateService {
    pub fn open(
        paths: Paths,
        current: ReleaseVersion,
        running_build: BuildId,
        catalog: Arc<dyn ReleaseCatalog>,
        launcher: Arc<dyn HelperLauncher>,
        scheduler: Arc<Scheduler>,
    ) -> Result<Arc<Self>> {
        let store = Store::new(&paths);
        store.prepare(&paths)?;
        let discovery = store.discovery().unwrap_or_else(|error| Discovery {
            error: Some(error),
            ..Discovery::default()
        });
        let service = Arc::new(Self {
            paths,
            current,
            running_build,
            store,
            discovery: Mutex::new(discovery),
            checking: tokio::sync::Mutex::new(()),
            preparing: tokio::sync::Mutex::new(()),
            catalog,
            launcher,
            scheduler,
            hold: Mutex::new(None),
            fleet: OnceLock::new(),
            distributing: Arc::new(AtomicBool::new(false)),
        });
        service.reconcile()?;
        Ok(service)
    }
    fn reconcile(&self) -> Result<()> {
        let Some(mut job) = self.store.job()? else {
            return Ok(());
        };
        if !job.phase.terminal()
            && !self.store.helper_alive()?
            && now().saturating_sub(job.updated_at) > 30
        {
            let error = Failure::new(
                "release_update_interrupted",
                "The release update was interrupted.",
                format!(
                    "Run elsecall doctor. The previous CLI, if backed up, is at {}. Run its install command to restore an unavailable service, or retry the update if the service is healthy.",
                    self.store.backup().join("elsecall").display()
                ),
            );
            self.store.advance(
                &mut job,
                Phase::Failed {
                    error,
                    rolled_back: false,
                },
            )?;
        }
        let mut hold = self.hold.lock().expect("update hold");
        if job.phase.terminal() {
            *hold = None;
        } else if job.phase.installation_started() && hold.is_none() {
            *hold = self.scheduler.hold_update_when_restartable();
            if hold.is_none() {
                return Err(invalid(
                    "An active update cannot close admission because work a restart would cut short is in flight.",
                ));
            }
        }
        Ok(())
    }
    /// Updates on this machine bring the rest of its fleet along from now on, when it
    /// is the master: once an update has installed this release here, the members
    /// receive the same build through `fleet`.
    pub fn distribute_with(&self, fleet: Arc<dyn FleetDistribution>) {
        let _ = self.fleet.set(fleet);
        self.follow_fleet();
    }

    /// After an update installed the release this service runs, on a master with other
    /// members, give them the same build, once per update. A distribution a restart
    /// cut short starts again; members already on the build are skipped. Members left
    /// behind, such as busy or unreachable ones, keep their build and are marked in
    /// the fleet window. Nothing here can undo this machine's own update.
    fn follow_fleet(&self) {
        if let Err(error) = self.try_follow_fleet() {
            crate::service_log!(
                "{{\"event\":\"release_fleet_follow_up_failed\",\"code\":{:?}}}",
                error.code
            );
        }
    }

    fn try_follow_fleet(&self) -> Result<()> {
        let Some(fleet) = self.fleet.get() else {
            return Ok(());
        };
        let Some(job) = self.store.job()? else {
            return Ok(());
        };
        if !matches!(job.phase, Phase::Succeeded) || job.release.version != self.current {
            return Ok(());
        }
        match self.store.fleet_follow_up()? {
            Some(follow_up) if follow_up.job == job.id => {
                let interrupted = matches!(follow_up.state, FollowUpState::Distributing { .. });
                if !interrupted || self.distributing.load(Ordering::SeqCst) {
                    return Ok(());
                }
            }
            _ => {}
        }
        if !fleet.leads_others() || self.distributing.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // Another caller may have started and finished this job's distribution between
        // the read above and taking the flag.
        let finished = self.store.fleet_follow_up()?.is_some_and(|follow_up| {
            follow_up.job == job.id
                && !matches!(follow_up.state, FollowUpState::Distributing { .. })
        });
        if finished {
            self.distributing.store(false, Ordering::SeqCst);
            return Ok(());
        }
        let started = FleetFollowUp {
            job: job.id.clone(),
            state: FollowUpState::Distributing { started_at: now() },
        };
        if let Err(error) = self.store.save_fleet_follow_up(&started) {
            self.distributing.store(false, Ordering::SeqCst);
            return Err(error);
        }
        let (store, fleet, distributing) = (
            self.store.clone(),
            Arc::clone(fleet),
            Arc::clone(&self.distributing),
        );
        tokio::spawn(async move {
            let state = match fleet.distribute(job.wait_seconds).await {
                Ok(outcome) => FollowUpState::Distributed { outcome },
                Err(error) => FollowUpState::Failed { error },
            };
            let ended = FleetFollowUp { job: job.id, state };
            if let Err(error) = store.save_fleet_follow_up(&ended) {
                crate::service_log!(
                    "{{\"event\":\"release_fleet_follow_up_unrecorded\",\"code\":{:?}}}",
                    error.code
                );
            }
            distributing.store(false, Ordering::SeqCst);
        });
        Ok(())
    }

    pub fn status(&self) -> Result<Status> {
        self.reconcile()?;
        self.follow_fleet();
        let discovery = self.discovery.lock().expect("release discovery").clone();
        Ok(Status {
            current_version: self.current.clone(),
            available: discovery
                .latest
                .filter(|release| release.version > self.current),
            checked_at: discovery.checked_at,
            check_error: discovery.error,
            job: self.store.job()?,
            // An unreadable record, such as one a newer build wrote, must not block
            // the status every update starts from.
            fleet: self.store.fleet_follow_up().ok().flatten(),
        })
    }
    pub async fn check(self: &Arc<Self>) -> Result<Status> {
        let _checking = self.checking.lock().await;
        let catalog = self.catalog.clone();
        let result = tokio::task::spawn_blocking(move || catalog.latest())
            .await
            .expect("release catalog does not panic");
        let mut discovery = self.discovery.lock().expect("release discovery");
        match result {
            Ok(latest) => {
                *discovery = Discovery {
                    latest,
                    checked_at: Some(now()),
                    error: None,
                };
            }
            Err(error) => {
                discovery.error = Some(error);
            }
        }
        self.store.save_discovery(&discovery)?;
        drop(discovery);
        self.status()
    }
    pub async fn start(self: &Arc<Self>, reopen: bool, wait_seconds: u64) -> Result<Job> {
        if wait_seconds > 3600 {
            return Err(invalid("An update may wait for work for at most one hour."));
        }
        let _request = self
            .store
            .request_lock()?
            .ok_or_else(|| invalid("An update request is already being prepared."))?;
        if self.status()?.job.is_some_and(|job| !job.phase.terminal()) {
            return Err(Failure::new(
                "update_in_progress",
                "An Elsecall release update is already running.",
                "Use elsecall update status to follow its progress.",
            ));
        }
        let release = self.status()?.available.ok_or_else(|| {
            Failure::new(
                "no_update_available",
                "No newer stable release is available.",
                "Run elsecall update check to refresh release information.",
            )
        })?;
        let mut job = Job {
            id: JobId::new()?,
            release,
            from_build: self.running_build.clone(),
            started_at_ms: super::now_millis(),
            reopen,
            wait_seconds,
            phase: Phase::Starting,
            updated_at: now(),
        };
        self.store.save_job(&job)?;
        let launcher = self.launcher.clone();
        let paths = self.paths.clone();
        let launching = job.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || launcher.start(&paths, &launching))
            .await
            .expect("helper launch does not panic")
        {
            self.store.advance(
                &mut job,
                Phase::Failed {
                    error: error.clone(),
                    rolled_back: false,
                },
            )?;
            return Err(error);
        }
        Ok(job)
    }
    pub async fn prepare(&self, id: &JobId) -> Result<()> {
        let _prepare = self.preparing.lock().await;
        let mut job = self
            .store
            .job()?
            .filter(|job| &job.id == id && !job.phase.terminal())
            .ok_or_else(|| invalid("This update is no longer active."))?;
        if job.from_build != self.running_build || !self.store.helper_alive()? {
            return Err(invalid(
                "The update no longer belongs to this running service.",
            ));
        }
        if self.hold.lock().expect("update hold").is_some() && job.phase.installation_started() {
            return Ok(());
        }
        if !matches!(job.phase, Phase::WaitingForWork) {
            return Err(invalid("This update is not ready to install."));
        }
        let _internal = self.scheduler.hold_internal();
        let _hosting = self.scheduler.hold_hosting();
        let deadline = Instant::now() + Duration::from_secs(job.wait_seconds);
        loop {
            if let Some(hold) = self.scheduler.hold_update_when_restartable() {
                *self.hold.lock().expect("update hold") = Some(hold);
                self.store.advance(&mut job, Phase::Installing)?;
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Failure::new(
                    "update_deferred",
                    "Work a restart would cut short (a run placed between members, a placement on its way to a member, a housekeeping pass, or a request queued by an elsecall too old to ask the new service again) was still in flight; nothing was installed.",
                    "Retry the update after it finishes; elsecall runs lists it.",
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    /// Tracks a helper across restart and releases admission after its terminal record.
    pub fn monitor(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let service = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = service.reconcile();
                // The helper records a finished update after this service starts;
                // the fleet follows without anyone asking for the status.
                service.follow_fleet();
            }
        })
    }
}

/// Provider wiring kept outside the update coordinator, also used by service tests.
pub struct UpdateAdapters {
    pub catalog: Arc<dyn ReleaseCatalog>,
    pub launcher: Arc<dyn HelperLauncher>,
    pub automatic: bool,
}
impl UpdateAdapters {
    /// The channel this build follows, which only its compile-time features choose.
    pub fn production(automatic: bool) -> Result<Self> {
        let channel = super::channel::Channel::this_build()?;
        if let super::channel::Channel::Local(local) = &channel {
            crate::service_log!(
                "{}",
                serde_json::json!({"event": "test_seam_in_use", "detail": format!("this build follows the local release channel at {}", local.location())})
            );
        }
        Ok(Self {
            catalog: channel.catalog(),
            launcher: Arc::new(DetachedHelper),
            automatic,
        })
    }
}
impl UpdateService {
    /// Poll independently of fleet reads. A fresh persisted check survives service restarts.
    pub fn schedule(self: &Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        let service = self.clone();
        tokio::spawn(async move {
            let delay = {
                let cached = service.discovery.lock().expect("release discovery");
                cached
                    .checked_at
                    .filter(|_| cached.error.is_none())
                    .map(|at| {
                        interval.saturating_sub(Duration::from_secs(now().saturating_sub(at)))
                    })
            };
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            loop {
                let _ = service.check().await;
                tokio::time::sleep(interval).await;
            }
        })
    }
}
