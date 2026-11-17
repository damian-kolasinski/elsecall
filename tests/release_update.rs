#![cfg(target_os = "macos")]
//! Update policy exercised with a real scheduler/API and fakes only at provider boundaries.
use elsecall::{
    build_identity::Digest,
    configuration::ServiceConfiguration,
    failure::Result,
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
        placed::Alone,
        pre_run_sync::{PreRunSync, Synced},
        processes::{ProcessIdentity, ProcessInfo, ProcessTable},
        request::{AdmissionRequest, Kind, Placement, Urgency},
        run_id::RandomRunIds,
        service::{Adapters, Driving, Runtime, Scheduler},
        speed::SpeedMeter,
    },
};
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
        ServiceConfiguration::default(),
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
async fn install_defers_busy_work_then_closes_new_admission_atomically_and_resumes_after_failure() {
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
    let grant = scheduler.request(100, request()).await.unwrap();
    assert_eq!(
        service.prepare(&job.id).await.unwrap_err().code,
        "update_deferred"
    );
    assert_eq!(scheduler.in_flight().len(), 1);
    assert!(!scheduler.hosting_held());
    scheduler
        .run_ended(
            100,
            &grant.run_id,
            elsecall::scheduling::messages::RunEnded::Exited {
                exit_code: 0,
                cpu_ms: None,
                run_phases: None,
                plan_observation: None,
            },
        )
        .unwrap();
    assert!(scheduler.in_flight().is_empty());
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
    assert!(scheduler.request(100, request()).await.is_ok());
    assert!(!grant.run_id.as_str().is_empty());
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
