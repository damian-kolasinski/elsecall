#![cfg(target_os = "macos")]
//! Update policy exercised with a real scheduler/API and fakes only at provider boundaries.
use elsecall::{
    inspection::{CoreCounts, LoadSample, Machine, MachineInspector, MemoryPressure, ThermalState},
    load_log::LoadLog,
    paths::Paths,
    private_files,
    release_update::{
        self, Job, JobId, Phase, Release, ReleaseCatalog, ReleaseVersion, Status,
        service::{HelperLauncher, UpdateService},
        store::Store,
    },
    scheduling::{
        capacity::Cores,
        clock::SystemClock,
        placed::{Alone, OfferReply, PlacementOffer, Warmth, WorkspaceRef},
        pre_run_sync::{PreRunSync, Synced},
        processes::{ProcessIdentity, ProcessInfo, ProcessTable},
        request::{AdmissionRequest, Kind, Placement, Urgency},
        run_id::{RandomRunIds, RunId},
        service::{Adapters, Driving, HostedEnd, Runtime, Scheduler, SchedulerConfiguration},
        speed::SpeedMeter,
    },
};
use elsecall_build_identity::build_identity::Digest;
use elsecall_failure::Result;
use std::{
    future::IntoFuture,
    sync::{Arc, Mutex},
};

struct MachineBoundary;
impl MachineInspector for MachineBoundary {
    fn inspect(&self) -> Result<Machine> {
        Err(release_update::invalid("No machine details in this test."))
    }
    fn sample_load(&self) -> Result<LoadSample> {
        Ok(LoadSample {
            cores: CoreCounts {
                performance: 4,
                efficiency: 0,
            },
            cpu_utilization: Some(0.0),
            memory_pressure: MemoryPressure::Normal,
            thermal_state: ThermalState::Nominal,
            load_one_minute: None,
        })
    }
}
struct Speed;
impl SpeedMeter for Speed {
    fn measure(&self) -> Result<f64> {
        Ok(1.0)
    }
}
struct Processes;
impl ProcessTable for Processes {
    fn describe(&self, pid: u32) -> Option<ProcessInfo> {
        Some(ProcessInfo {
            identity: ProcessIdentity { pid, started_us: 1 },
            parent: 0,
        })
    }
    fn watch(&self, _: ProcessIdentity) -> bool {
        true
    }
    fn kill_group(&self, _: ProcessIdentity, _: u32) -> bool {
        false
    }
}
struct SyncBoundary;
impl PreRunSync for SyncBoundary {
    fn bring_up_to_date(
        &self,
        _: &str,
    ) -> futures_util::future::BoxFuture<'static, Result<Synced>> {
        Box::pin(async { Ok(Synced::NothingSynced) })
    }
}
struct Catalog(Mutex<Result<Option<Release>>>);
impl ReleaseCatalog for Catalog {
    fn latest(&self) -> Result<Option<Release>> {
        self.0.lock().unwrap().clone()
    }
}
struct Launch {
    calls: Mutex<Vec<Job>>,
    fail: bool,
}
impl HelperLauncher for Launch {
    fn start(&self, _: &Paths, job: &Job) -> Result<()> {
        self.calls.lock().unwrap().push(job.clone());
        if self.fail {
            Err(release_update::invalid("Helper could not start."))
        } else {
            Ok(())
        }
    }
}
fn release(version: &str) -> Release {
    Release {
        version: ReleaseVersion::try_from(version.to_owned()).unwrap(),
    }
}
fn fixture() -> (tempfile::TempDir, Paths, Arc<Scheduler>) {
    let home = tempfile::Builder::new()
        .prefix("ec-release-")
        .tempdir_in("/tmp")
        .unwrap();
    let paths = Paths::for_home(home.path()).unwrap();
    private_files::owned_tree(&paths.home, &paths.data).unwrap();
    private_files::directory(&paths.data).unwrap();
    let (_, exits) = tokio::sync::mpsc::unbounded_channel();
    let (scheduler, _tasks) = Scheduler::start_with_runtime(
        &paths.data.join("records.sqlite"),
        SchedulerConfiguration::default(),
        Adapters {
            machine: Arc::new(MachineBoundary),
            speed: Arc::new(Speed),
            processes: Arc::new(Processes),
            exits,
        },
        Arc::new(Alone),
        Arc::new(SyncBoundary),
        Arc::new(LoadLog::open(&paths.load_history()).unwrap()),
        Runtime {
            clock: Arc::new(SystemClock),
            ids: Arc::new(RandomRunIds),
            driving: Driving::Controlled,
        },
    )
    .unwrap();
    scheduler.tick().unwrap();
    scheduler.observe_speed(1.0);
    (home, paths, scheduler)
}
fn request() -> AdmissionRequest {
    AdmissionRequest {
        workload: None,
        kind: Kind::Run,
        holder_pid: None,
        size: Some(Cores::from_millis(1000)),
        label: Some("acme test Checkout".into()),
        placement: Placement::Pinned,
        urgency: Urgency::Interactive {
            hold_limit_ms: Some(10000),
        },
        workspace: None,
        settings: None,
    }
}
fn service(
    paths: &Paths,
    scheduler: Arc<Scheduler>,
    catalog: Arc<Catalog>,
    launcher: Arc<Launch>,
) -> Arc<UpdateService> {
    UpdateService::open(
        paths.clone(),
        release("0.1.0").version,
        Digest::of(b"old").build_id(),
        catalog,
        launcher,
        scheduler,
    )
    .unwrap()
}

#[test]
fn the_latest_release_is_offered_only_when_it_is_stable_and_fully_published() {
    use release_update::catalog::latest_installable;
    let page = "https://github.com/damian-kolasinski/elsecall/releases";
    let everything = |_: &Release, _: &str| Ok(true);
    assert_eq!(
        latest_installable(&format!("{page}/tag/v0.2.1"), everything).unwrap(),
        Some(release("0.2.1"))
    );
    // A repository without releases redirects to its release list.
    assert_eq!(latest_installable(page, everything).unwrap(), None);
    for tag in ["v2.0.0-beta.1", "v01.0.0", "0.2.1", "v0.2.1+other"] {
        assert_eq!(
            latest_installable(&format!("{page}/tag/{tag}"), everything).unwrap(),
            None,
            "{tag}"
        );
    }
    let asked = std::sync::Mutex::new(Vec::new());
    let unsigned = |release: &Release, name: &str| {
        asked
            .lock()
            .unwrap()
            .push(format!("{} {name}", release.version));
        Ok(name != "packslip.sigstore.json")
    };
    assert_eq!(
        latest_installable(&format!("{page}/tag/v0.2.1"), unsigned).unwrap(),
        None
    );
    assert_eq!(
        *asked.lock().unwrap(),
        vec![
            "0.2.1 elsecall-v0.2.1-aarch64-apple-darwin.tar.gz",
            "0.2.1 packslip.sigstore.json"
        ]
    );
    // Another repository's release is never offered.
    assert!(
        latest_installable(
            "https://github.com/example/other/releases/tag/v9.0.0",
            everything
        )
        .is_err()
    );
    for version in ["../0.2.0", "0.2.0-beta.1", "0.2.0+other", "v0.2.0", ""] {
        assert!(ReleaseVersion::try_from(version.to_owned()).is_err());
    }
    assert!(JobId::try_from("../job".to_owned()).is_err());
}

#[tokio::test]
async fn check_retains_a_cached_offer_on_network_failure_and_never_offers_an_older_build() {
    let (_home, paths, scheduler) = fixture();
    let catalog = Arc::new(Catalog(Mutex::new(Ok(Some(release("0.1.1"))))));
    let launch = Arc::new(Launch {
        calls: Mutex::default(),
        fail: false,
    });
    let service = service(&paths, scheduler, catalog.clone(), launch.clone());
    let status = service.check().await.unwrap();
    assert_eq!(status.available, Some(release("0.1.1")));
    assert!(status.check_error.is_none());
    assert!(status.checked_at.is_some());
    *catalog.0.lock().unwrap() = Err(release_update::invalid("Network unavailable."));
    let status = service.check().await.unwrap();
    assert_eq!(status.available, Some(release("0.1.1")));
    assert!(status.check_error.is_some());
    for version in ["0.1.0", "0.0.9"] {
        *catalog.0.lock().unwrap() = Ok(Some(release(version)));
        assert!(service.check().await.unwrap().available.is_none());
    }
    assert!(launch.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn api_and_real_cli_check_start_and_follow_the_same_durable_job() {
    let (_home, paths, scheduler) = fixture();
    let catalog = Arc::new(Catalog(Mutex::new(Ok(Some(release("0.1.1"))))));
    let launch = Arc::new(Launch {
        calls: Mutex::default(),
        fail: false,
    });
    let service = service(&paths, scheduler, catalog, launch.clone());
    let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
    let task = tokio::spawn(
        axum::serve(
            listener,
            elsecall::release_update_api::routes(service.clone()),
        )
        .into_future(),
    );
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", &paths.home)
        .args(["update", "check"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["data"]["available"]["version"], "0.1.1");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", &paths.home)
        .args(["update", "install", "--no-wait", "--reopen"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["data"]["state"], "starting");
    let job = service.status().unwrap().job.unwrap();
    assert!(job.reopen);
    assert_eq!(launch.calls.lock().unwrap().len(), 1);
    assert_eq!(
        service.start(true, 1).await.unwrap_err().code,
        "update_in_progress"
    );
    let mut done = job.clone();
    Store::new(&paths)
        .advance(&mut done, Phase::Succeeded)
        .unwrap();
    let status: Status = elsecall::client::get(&paths.socket(), "/v1/update")
        .await
        .unwrap();
    assert!(matches!(status.job.unwrap().phase, Phase::Succeeded));
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn install_waits_for_placed_runs_but_not_local_work_then_closes_admission_until_it_ends() {
    let (_home, paths, scheduler) = fixture();
    let catalog = Arc::new(Catalog(Mutex::new(Ok(Some(release("0.1.1"))))));
    let launch = Arc::new(Launch {
        calls: Mutex::default(),
        fail: false,
    });
    let service = service(&paths, scheduler.clone(), catalog, launch);
    service.check().await.unwrap();
    let mut job = service.start(false, 0).await.unwrap();
    let store = Store::new(&paths);
    let _helper = store.helper_lock().unwrap().unwrap();
    store.advance(&mut job, Phase::WaitingForWork).unwrap();
    // A local run keeps running through the restart; a run another member placed
    // here would be cut short by it.
    let local = scheduler.request(100, request()).await.unwrap();
    let placed = RunId::parse("r-0000000000ff").unwrap();
    let offer = PlacementOffer {
        workload: None,
        run_id: placed.clone(),
        size: Cores::from_millis(1000),
        label: Some("acme test Checkout".into()),
        workspace: WorkspaceRef {
            root: "/Users/me/acme".into(),
            common_dir: "/Users/me/acme/.git".into(),
        },
        start_within_ms: 10_000,
        size_basis: None,
        expected_run_ms: None,
    };
    let reply = scheduler
        .accept_placed(
            "mac-mini",
            &offer,
            true,
            "/tmp/copy",
            "mapped",
            Warmth::Warm,
        )
        .unwrap();
    assert!(matches!(reply, OfferReply::Accepted { .. }));
    let deferred = service.prepare(&job.id).await.unwrap_err();
    assert_eq!(deferred.code, "update_deferred");
    assert_eq!(scheduler.in_flight().len(), 2);
    assert!(!scheduler.hosting_held());
    scheduler.hosted_ended(
        &placed,
        HostedEnd::Exited {
            code: 0,
            cpu_ms: None,
        },
        None,
    );
    assert_eq!(scheduler.in_flight().len(), 1, "the local run still runs");
    service.prepare(&job.id).await.unwrap();
    assert_eq!(
        scheduler.request(100, request()).await.unwrap_err().code,
        "update_in_progress"
    );
    assert!(!scheduler.stop_if_idle());
    store
        .advance(
            &mut job,
            Phase::Failed {
                error: release_update::invalid("Install failed."),
                rolled_back: true,
            },
        )
        .unwrap();
    service.status().unwrap();
    assert!(scheduler.request(101, request()).await.is_ok());
    assert!(!local.run_id.as_str().is_empty());
}

#[tokio::test]
async fn failed_helper_launch_and_stale_jobs_are_retryable() {
    let (_home, paths, scheduler) = fixture();
    let catalog = Arc::new(Catalog(Mutex::new(Ok(Some(release("0.1.1"))))));
    let launch = Arc::new(Launch {
        calls: Mutex::default(),
        fail: true,
    });
    let service = service(&paths, scheduler, catalog, launch);
    service.check().await.unwrap();
    assert!(service.start(false, 0).await.is_err());
    assert!(matches!(
        service.status().unwrap().job.unwrap().phase,
        Phase::Failed {
            rolled_back: false,
            ..
        }
    ));
    let store = Store::new(&paths);
    let mut job = store.job().unwrap().unwrap();
    job.phase = Phase::Downloading;
    job.updated_at = release_update::now() - 31;
    store.save_job(&job).unwrap();
    let Phase::Failed { error, .. } = service.status().unwrap().job.unwrap().phase else {
        panic!("interrupted job must fail");
    };
    assert_eq!(error.code, "release_update_interrupted");
    assert!(
        error
            .hint
            .contains(store.backup().join("elsecall").to_str().unwrap())
    );
    assert!(service.start(false, 3601).await.is_err());
}

struct ScheduledCatalog {
    calls: std::sync::atomic::AtomicUsize,
}
impl ReleaseCatalog for ScheduledCatalog {
    fn latest(&self) -> Result<Option<Release>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Some(release("0.1.1")))
    }
}
#[tokio::test(start_paused = true)]
async fn automatic_discovery_checks_at_startup_and_hourly_and_reuses_a_fresh_persisted_check() {
    use std::sync::atomic::Ordering;
    let (_home, paths, scheduler) = fixture();
    let catalog = Arc::new(ScheduledCatalog {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let launcher = Arc::new(Launch {
        calls: Mutex::default(),
        fail: false,
    });
    let service = UpdateService::open(
        paths.clone(),
        release("0.1.0").version,
        Digest::of(b"old").build_id(),
        catalog.clone(),
        launcher.clone(),
        scheduler.clone(),
    )
    .unwrap();
    let task = service.schedule(std::time::Duration::from_secs(3600));
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while service.status().unwrap().checked_at.is_none() {
        assert!(std::time::Instant::now() < until);
        tokio::task::yield_now().await;
    }
    assert_eq!(catalog.calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(std::time::Duration::from_secs(3599)).await;
    assert_eq!(catalog.calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    while catalog.calls.load(Ordering::SeqCst) < 2 {
        assert!(std::time::Instant::now() < until);
        tokio::task::yield_now().await;
    }
    task.abort();
    let _ = task.await;
    let restarted = UpdateService::open(
        paths.clone(),
        release("0.1.0").version,
        Digest::of(b"old").build_id(),
        catalog.clone(),
        launcher.clone(),
        scheduler,
    )
    .unwrap();
    let task = restarted.schedule(std::time::Duration::from_secs(3600));
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(3599)).await;
    assert_eq!(catalog.calls.load(Ordering::SeqCst), 2);
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    while catalog.calls.load(Ordering::SeqCst) < 3 {
        assert!(std::time::Instant::now() < until);
        tokio::task::yield_now().await;
    }
    assert!(launcher.calls.lock().unwrap().is_empty());
    assert_eq!(
        restarted.status().unwrap().available,
        Some(release("0.1.1"))
    );
    task.abort();
    let _ = task.await;
}

/// The fleet at the boundary: whether this machine leads others, and what giving them
/// the build returns.
struct FakeFleet {
    leads: bool,
    waits: Mutex<Vec<u64>>,
    fail: bool,
}
impl release_update::FleetDistribution for FakeFleet {
    fn leads_others(&self) -> bool {
        self.leads
    }
    fn distribute(
        &self,
        wait_seconds: u64,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<release_update::FleetOutcome>> + Send + '_>,
    > {
        self.waits.lock().unwrap().push(wait_seconds);
        let result = if self.fail {
            Err(release_update::invalid("The members could not be reached."))
        } else {
            Ok(release_update::FleetOutcome {
                complete: false,
                members: vec![
                    release_update::MemberOutcome {
                        name: "macbook-pro".into(),
                        outcome: "upgraded".into(),
                    },
                    release_update::MemberOutcome {
                        name: "mac-studio".into(),
                        outcome: "deferred".into(),
                    },
                ],
            })
        };
        Box::pin(async move { result })
    }
}
fn fake_fleet(leads: bool, fail: bool) -> Arc<FakeFleet> {
    Arc::new(FakeFleet {
        leads,
        waits: Mutex::new(Vec::new()),
        fail,
    })
}
/// An update job, as the helper leaves it, for `version` in `phase`.
fn finished_job(paths: &Paths, version: &str, phase: Phase) -> Job {
    let job = Job {
        id: JobId::new().unwrap(),
        release: release(version),
        from_build: Digest::of(b"older").build_id(),
        started_at_ms: 1,
        reopen: false,
        wait_seconds: 120,
        phase,
        updated_at: release_update::now(),
    };
    let store = Store::new(paths);
    store.prepare(paths).unwrap();
    store.save_job(&job).unwrap();
    job
}
fn quiet_service(paths: &Paths, scheduler: Arc<Scheduler>) -> Arc<UpdateService> {
    service(
        paths,
        scheduler,
        Arc::new(Catalog(Mutex::new(Ok(None)))),
        Arc::new(Launch {
            calls: Mutex::new(Vec::new()),
            fail: false,
        }),
    )
}
/// The fleet half once it has ended, or what it still is after five seconds.
async fn fleet_settled(service: &UpdateService) -> Option<release_update::FleetFollowUp> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let fleet = service.status().unwrap().fleet;
        let distributing = matches!(
            fleet,
            Some(release_update::FleetFollowUp {
                state: release_update::FollowUpState::Distributing { .. },
                ..
            })
        );
        if !distributing || std::time::Instant::now() > until {
            return fleet;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_masters_update_brings_the_fleet_to_its_build_once() {
    let (_home, paths, scheduler) = fixture();
    let job = finished_job(&paths, "0.1.0", Phase::Succeeded);
    let service = quiet_service(&paths, scheduler);
    let fleet = fake_fleet(true, false);
    service.distribute_with(fleet.clone());
    let settled = fleet_settled(&service).await.unwrap();
    assert_eq!(settled.job, job.id);
    let release_update::FollowUpState::Distributed { outcome } = settled.state else {
        panic!("expected a distributed fleet, got {:?}", settled.state);
    };
    assert!(!outcome.complete);
    assert_eq!(outcome.members[1].outcome, "deferred");
    // Members wait for their work as long as the master's update did.
    assert_eq!(*fleet.waits.lock().unwrap(), vec![120]);
    for _ in 0..3 {
        let fleet = service.status().unwrap().fleet.unwrap();
        assert!(matches!(
            fleet.state,
            release_update::FollowUpState::Distributed { .. }
        ));
    }
    assert_eq!(fleet.waits.lock().unwrap().len(), 1);
    // A member left behind stays behind; only the next update or fleet upgrade moves it.
    let restarted = quiet_service(&paths, fixture().2);
    let again = fake_fleet(true, false);
    restarted.distribute_with(again.clone());
    restarted.status().unwrap();
    assert!(again.waits.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn members_lone_machines_and_unfinished_or_other_updates_never_distribute() {
    for (leads, version, phase) in [
        (false, "0.1.0", Phase::Succeeded),
        (true, "0.1.0", Phase::Installing),
        (true, "0.1.1", Phase::Succeeded),
    ] {
        let (_home, paths, scheduler) = fixture();
        finished_job(&paths, version, phase);
        let service = quiet_service(&paths, scheduler);
        let fleet = fake_fleet(leads, false);
        service.distribute_with(fleet.clone());
        assert!(service.status().unwrap().fleet.is_none(), "{version}");
        assert!(fleet.waits.lock().unwrap().is_empty(), "{version}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_distribution_is_reported_once_and_the_masters_update_stands() {
    let (_home, paths, scheduler) = fixture();
    finished_job(&paths, "0.1.0", Phase::Succeeded);
    let service = quiet_service(&paths, scheduler);
    let fleet = fake_fleet(true, true);
    service.distribute_with(fleet.clone());
    let settled = fleet_settled(&service).await.unwrap();
    let release_update::FollowUpState::Failed { error } = settled.state else {
        panic!("expected a failure, got {:?}", settled.state);
    };
    assert_eq!(error.code, "release_update_failed");
    service.status().unwrap();
    assert_eq!(fleet.waits.lock().unwrap().len(), 1);
    assert!(matches!(
        service.status().unwrap().job.unwrap().phase,
        Phase::Succeeded
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_distribution_cut_short_by_a_restart_starts_again() {
    let (_home, paths, scheduler) = fixture();
    let job = finished_job(&paths, "0.1.0", Phase::Succeeded);
    Store::new(&paths)
        .save_fleet_follow_up(&release_update::FleetFollowUp {
            job: job.id.clone(),
            state: release_update::FollowUpState::Distributing { started_at: 1 },
        })
        .unwrap();
    let service = quiet_service(&paths, scheduler);
    let fleet = fake_fleet(true, false);
    service.distribute_with(fleet.clone());
    let settled = fleet_settled(&service).await.unwrap();
    assert!(matches!(
        settled.state,
        release_update::FollowUpState::Distributed { .. }
    ));
    assert_eq!(fleet.waits.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fleet_follows_an_update_the_helper_finishes_after_startup_unasked() {
    let (_home, paths, scheduler) = fixture();
    let service = quiet_service(&paths, scheduler);
    let _monitor = service.monitor();
    let fleet = fake_fleet(true, false);
    // As in production: the restarted service starts before the helper records the
    // finished update, and nothing asks for the status afterwards.
    service.distribute_with(fleet.clone());
    let job = finished_job(&paths, "0.1.0", Phase::Succeeded);
    let store = Store::new(&paths);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let recorded = loop {
        match store.fleet_follow_up().unwrap() {
            Some(follow_up)
                if !matches!(
                    follow_up.state,
                    release_update::FollowUpState::Distributing { .. }
                ) =>
            {
                break follow_up;
            }
            _ if std::time::Instant::now() > until => panic!("the fleet never followed"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    };
    assert_eq!(recorded.job, job.id);
    assert!(matches!(
        recorded.state,
        release_update::FollowUpState::Distributed { .. }
    ));
    assert_eq!(fleet.waits.lock().unwrap().len(), 1);
}

#[test]
fn a_fleet_state_from_a_newer_build_still_reads() {
    let follow_up: release_update::FleetFollowUp = serde_json::from_value(serde_json::json!({
        "job": "a".repeat(32),
        "state": "superseded",
        "by": "something new"
    }))
    .unwrap();
    assert!(matches!(
        follow_up.state,
        release_update::FollowUpState::Unknown
    ));
}
