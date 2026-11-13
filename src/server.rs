use crate::{
    api,
    failure::{Failure, Result},
    fleet::Fleet,
    fleet_local_api,
    fleet_placement::FleetMembers,
    inspection::Inspection,
    local_access,
    member_key::MemberKey,
    membership_store::MembershipStore,
    paths::Paths,
    peer_api, peer_listener, placement_api,
    placement_host::PlacementHost,
    private_files, run_relay,
    scheduling::service::{Adapters, Scheduler},
    workspace::{copies::Copies, location::Locations, object_sources::ObjectSources},
};
use crate::{
    fleet_callers::FleetCallers,
    housekeeping::{
        self,
        caller_state::CallerState,
        callers::{Callers, Nobody},
        catalog::Catalog,
        pass::{Housekeeper, Parts},
    },
};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use std::{
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    sync::Arc,
};
use tokio::net::{UnixListener, UnixStream};

/// The process at the other end of a local connection.
#[derive(Clone, Copy, Debug)]
pub struct Peer {
    pub pid: Option<u32>,
}
impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, LocalListener>>
    for Peer
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, LocalListener>) -> Self {
        *stream.remote_addr()
    }
}

/// Where same-path copies go: the caller's path itself, unless
/// `ELSECALL_SAME_PATH_BASE` moves them under another directory, as tests on one
/// machine do so a member's copy never lands on the caller's own working tree.
fn same_path_base() -> std::path::PathBuf {
    std::env::var_os("ELSECALL_SAME_PATH_BASE")
        .map(std::path::PathBuf::from)
        .filter(|base| base.is_absolute())
        .unwrap_or_else(|| "/".into())
}

pub async fn serve(
    paths: Paths,
    inspection: Arc<Inspection>,
    scheduling: Adapters,
    process_sampler: Arc<dyn crate::outside_pressure::ProcessSampler>,
    members_listener: peer_listener::Settings,
) -> Result<()> {
    private_files::owned_tree(&paths.home, &paths.data)?;
    private_files::directory(&paths.data)?;
    let Some(service_lock) = private_files::try_lock(&paths.data.join("service.lock"))? else {
        return Err(Failure::new(
            "already_running",
            "Another Elsecall service instance is running.",
            "Use elsecall service restart for the installed service, or stop the other foreground serve process.",
        ));
    };
    crate::peer_diagnostics::initialize(&paths)?;
    if let Err(error) = crate::state_temps::prune(&paths) {
        // Optional sync/upgrade state must not take down local admission.
        crate::service_log!(
            "{}",
            serde_json::json!({"event":"state_scratch_cleanup_failed", "code":error.code})
        );
    }
    // Identify the build before anything could replace this executable, and name
    // it on every member request from now on.
    let build = crate::build_identity::announce()?;
    // Fail before binding, so an invalid setting is never silently ignored.
    // Broken fleet state must not take down the local API: status, machine and the
    // fleet commands' diagnosis keep working, and the member listener stays closed.
    let fleet = MemberKey::load_or_create(&paths.member_key())
        .and_then(|key| {
            Fleet::open(
                key,
                MembershipStore::new(paths.fleet_membership()),
                inspection.clone(),
            )
        })
        .map(Arc::new);
    let name = fleet
        .as_ref()
        .ok()
        .and_then(|f| f.roster().ok().map(|r| f.name_in(&r)));
    let mut configuration = crate::service_configuration::load(&paths, name.as_ref())?;
    let members = Arc::new(FleetMembers::new(fleet.as_ref().ok().cloned()));
    let load_log = Arc::new(crate::load_log::LoadLog::open(&paths.load_history())?);
    let mut locations = configured_locations(&paths, &configuration);
    // Configuration sync needs fleet state; broken sync state degrades only sync.
    let mut config_sync = match &fleet {
        Ok(fleet) => open_config_sync(&paths, &locations, fleet, configuration.sync_backups),
        Err(error) => Err(error.clone()),
    };
    if let Ok(sync) = &config_sync {
        match sync.service_settings() {
            Ok(Some(accepted)) if accepted != configuration => {
                // Takeover may have completed a journaled generation. Build all
                // runtime adapters and path exclusions from that same snapshot.
                configuration = accepted;
                locations = configured_locations(&paths, &configuration);
                config_sync = match &fleet {
                    Ok(fleet) => {
                        open_config_sync(&paths, &locations, fleet, configuration.sync_backups)
                    }
                    Err(error) => Err(error.clone()),
                };
                if let Ok(sync) = &config_sync
                    && let Err(error) = sync.service_settings()
                {
                    config_sync = Err(error);
                }
            }
            Err(error) => config_sync = Err(error),
            _ => (),
        }
    }
    crate::service_log::configure(configuration.logs.clone());
    if let (Ok(fleet), Ok(sync)) = (&fleet, &config_sync) {
        fleet.report_sync(sync.clone());
        fleet.hand_over_with(sync.clone());
        fleet.observe_role_with(Arc::new(crate::config_sync::service::RoleChanges(
            sync.clone(),
        )));
    }
    if let (Ok(fleet), Err(error)) = (&fleet, &config_sync) {
        crate::service_log!(
            "{{\"event\":\"config_sync_unavailable\",\"code\":{:?}}}",
            error.code
        );
        // A move must not skip the handover because sync is broken here.
        fleet.hand_over_with(Arc::new(crate::config_sync::service::UnavailableHandover(
            error.clone(),
        )));
    }
    // Placed runs bring their member's synced configuration up to date first.
    let pre_run = Arc::new(crate::placement_config_sync::PlacementConfigSync::new(
        config_sync.clone(),
    ));
    let (scheduler, _scheduling) = Scheduler::start(
        &paths.records(),
        configuration.clone(),
        scheduling,
        members,
        pre_run,
        load_log.clone(),
    )?;
    if let Ok(sync) = &config_sync {
        sync.report_effective_settings(scheduler.effective_settings_fingerprint());
    }
    // Broken outside pressure history must not take down admission.
    let outside_pressure =
        crate::outside_pressure::Detector::open(&paths.outside_pressure(), process_sampler.clone())
            .map(Arc::new);
    // Nor must broken early warnings; they are shares of physical memory.
    let outside_processes = inspection
        .machine()
        .and_then(|machine| {
            crate::outside_pressure::Watcher::open(
                &paths.outside_processes(),
                process_sampler,
                machine.memory.total_bytes,
            )
        })
        .map(Arc::new);
    let _outside_processes = match &outside_processes {
        Ok(watcher) => Some(AbortOnDrop(
            tokio::spawn(crate::outside_pressure::watcher::follow(
                watcher.clone(),
                Arc::new(crate::outside_pressure_gate::SchedulerGate(
                    scheduler.clone(),
                )),
            ))
            .abort_handle(),
        )),
        Err(error) => {
            crate::service_log!(
                "{}",
                serde_json::json!({"event":"outside_processes_unavailable", "code":error.code})
            );
            None
        }
    };
    let warnings = outside_processes
        .as_ref()
        .map(|watcher| watcher.clone() as Arc<dyn crate::outside_pressure::WarningReport>)
        .map_err(Clone::clone);
    if let (Ok(fleet), Ok(warnings)) = (&fleet, &warnings) {
        fleet.report_warnings(warnings.clone());
    }
    let _outside_pressure = match &outside_pressure {
        Ok(detector) => Some(AbortOnDrop(
            tokio::spawn(crate::outside_pressure::detector::follow(
                detector.clone(),
                Arc::new(crate::outside_pressure_gate::SchedulerGate(
                    scheduler.clone(),
                )),
                crate::scheduling::service::SAMPLE_INTERVAL,
            ))
            .abort_handle(),
        )),
        Err(error) => {
            crate::service_log!(
                "{}",
                serde_json::json!({"event":"outside_pressure_unavailable", "code":error.code})
            );
            None
        }
    };
    if let (Ok(fleet), Ok(detector)) = (&fleet, &outside_pressure) {
        fleet.report_blocked(detector.clone());
    }
    let outside_detector = outside_pressure.as_ref().ok().cloned();
    let outside_pressure_routes = match outside_pressure {
        Ok(detector) => {
            let scheduler = scheduler.clone();
            crate::outside_pressure_api::routes(
                detector,
                warnings.clone().ok(),
                Arc::new(move || scheduler.name()),
            )
        }
        Err(error) => crate::outside_pressure_api::unavailable(error),
    };
    // Upgrades need fleet state; a broken upgrade record refuses only upgrades.
    let upgrades = match &fleet {
        Ok(fleet) => crate::fleet_upgrade::member::Upgrades::open(
            crate::fleet_upgrade::journal::Journal::new(&paths.data),
            paths.executable(),
            fleet.clone(),
            Arc::new(crate::upgrade_drain::SchedulerWork(scheduler.clone())),
            Arc::new(crate::upgrade_helper_process::HelperProcess),
            build.clone(),
        )
        .map(Arc::new),
        Err(error) => Err(error.clone()),
    };
    let catalog = Arc::new(Catalog::open(
        &paths.data.join("copies.sqlite"),
        housekeeping::clock::from_environment(),
    )?);
    let copies = Copies::new(catalog.clone());
    let caller_state = Arc::new(CallerState::new(paths.data.join("workspaces")));
    let callers: Arc<dyn Callers> = match &fleet {
        Ok(fleet) => Arc::new(FleetCallers::new(fleet.clone())),
        Err(_) => Arc::new(Nobody),
    };
    let housekeeper = {
        let scheduler = scheduler.clone();
        Arc::new(Housekeeper::new(Parts {
            catalog,
            copies: copies.clone(),
            locations: locations.clone(),
            volumes: housekeeping::volumes::from_environment(&locations.root),
            settings: configuration.housekeeping.clone(),
            member: Box::new(move || scheduler.name()),
            callers,
            caller_state: caller_state.clone(),
        }))
    };
    if let Ok(fleet) = &fleet {
        fleet.report_copies(Arc::new(crate::fleet_copies::FleetCopies(
            housekeeper.clone(),
        )));
        fleet.report_load(load_log.clone());
    }
    let _housekeeping = AbortOnDrop(housekeeping::schedule::start(
        housekeeper.clone(),
        Arc::new(scheduler.clone()),
    ));
    let rehearsal = crate::rehearsal::member::Member::new(
        paths.clone(),
        scheduler.clone(),
        housekeeper.clone(),
        load_log.clone(),
    );
    let host = PlacementHost::new(
        scheduler.clone(),
        copies,
        locations,
        ObjectSources::new(configuration.object_sources.clone()),
        configuration.placed_path,
        configuration.leftovers_cap,
    );
    let stopping = scheduler.clone();
    let restart_scheduler = scheduler.clone();
    let restart_sync = config_sync.as_ref().ok().cloned();
    let relaunch = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let restarting = relaunch.clone();
    let socket = paths.socket();
    match fs::symlink_metadata(&socket) {
        Ok(meta) if meta.file_type().is_socket() => private_files::remove_file(&socket)?,
        Ok(_) => {
            return Err(Failure::new(
                "socket_conflict",
                "The service socket path contains a non-socket file.",
                "Inspect the service.sock file in the Elsecall state directory; move it before retrying.",
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(Failure::io("Inspect local socket", e)),
    }
    let listener = UnixListener::bind(&socket).map_err(|e| Failure::io("Bind local socket", e))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|e| Failure::io("Restrict local socket", e))?;
    let bound = fs::symlink_metadata(&socket)
        .map_err(|e| Failure::io("Inspect local socket", e))?
        .ino();
    let relay = run_relay::routes(scheduler.clone(), fleet.as_ref().ok().cloned());
    let reporter = crate::report_api::Reporter {
        paths: paths.clone(),
        own: crate::own_history::OwnHistory {
            scheduler: scheduler.clone(),
            load: load_log.clone(),
            outside: outside_detector,
            warnings: warnings.ok(),
        },
        fleet: fleet.clone(),
    };
    let sync_routes = match &config_sync {
        Ok(sync) => crate::config_sync_local_api::routes(sync.clone()),
        Err(error) => crate::config_sync_local_api::unavailable(error.clone()),
    };
    let tools_routes = crate::tools_api::routes(fleet.clone());
    let desired_routes = crate::toolset_api::routes(fleet.clone(), &paths);
    let fleet_routes = match fleet {
        Ok(fleet) => {
            let mut member_routes = placement_api::routes(host)
                .merge(crate::workspace_check_api::routes(caller_state))
                .merge(crate::report_api::peer_routes(reporter.clone()))
                .merge(crate::trace_api::peer_routes(reporter.clone()))
                .merge(crate::rehearsal_api::peer_routes(rehearsal.clone()))
                .merge(crate::tuning_trust_api::peer_routes(paths.clone()))
                .merge(crate::tools_api::peer_routes());
            if let Ok(sync) = &config_sync {
                member_routes = member_routes.merge(crate::config_sync_api::routes(sync.clone()));
            }
            member_routes = member_routes.merge(match &upgrades {
                Ok(upgrades) => crate::fleet_upgrade_api::routes(upgrades.clone()),
                Err(error) => crate::fleet_upgrade_api::unavailable(error.clone()),
            });
            let peer_routes = peer_api::routes(fleet.clone(), member_routes);
            peer_listener::start(fleet.clone(), members_listener, peer_routes).await;
            crate::rehearsal_api::routes(fleet.clone(), rehearsal)
                .merge(crate::fleet_upgrade_local_api::routes(
                    fleet.clone(),
                    paths.executable(),
                ))
                .merge(fleet_local_api::routes(fleet, scheduler.clone()))
        }
        Err(error) => {
            crate::service_log!(
                "{{\"event\":\"fleet_unavailable\",\"code\":{:?}}}",
                error.code
            );
            fleet_local_api::unavailable(error)
        }
    };
    let _drift = AbortOnDrop(crate::tuning_drift::start(
        paths.clone(),
        scheduler.clone(),
        configuration.housekeeping.interval,
    ));
    let trust_routes = crate::tuning_trust_api::routes(paths.clone(), reporter.fleet.clone());
    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/machine", get(machine))
        .fallback(|| async {
            (
                StatusCode::NOT_FOUND,
                Json(api::ErrorResponse::new(Failure::new(
                    "not_found",
                    "Unknown API route.",
                    "Use /v1/health or /v1/machine.",
                ))),
            )
        })
        .with_state(inspection)
        .merge(trust_routes)
        .merge(crate::replay_api::routes(scheduler.clone()))
        .merge(crate::admission_api::routes(scheduler))
        .merge(crate::copies_api::routes(housekeeper))
        .merge(tools_routes)
        .merge(desired_routes)
        .merge(crate::ui_app::routes(paths.clone()))
        .merge(crate::trace_api::routes(reporter.clone()))
        .merge(crate::report_api::routes(reporter))
        .merge(relay)
        .merge(sync_routes)
        .merge(outside_pressure_routes)
        .merge(fleet_routes);
    // Register handlers before signaling readiness, and release the socket on shutdown.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| Failure::io("Register shutdown handler", e))?;
    crate::service_log!(
        "{{\"event\":\"service_ready\",\"version\":\"{}\",\"build\":\"{}\"}}",
        build.version,
        build.build
    );
    let result = axum::serve(
        LocalListener(listener),
        app.into_make_service_with_connect_info::<Peer>(),
    )
    .with_graceful_shutdown(async move {
        tokio::select! {
            _ = terminate.recv() => (),
            _ = tokio::signal::ctrl_c() => (),
            _ = crate::service_relaunch::when_idle(restart_sync, restart_scheduler) => {
                restarting.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        // Queued requests would otherwise keep their connections open until their
        // hold limits, and graceful shutdown waits for every connection.
        stopping.shutdown();
    })
    .await
    .map_err(|e| Failure::io("Serve local API", e));
    // A successor may already have replaced the socket; remove only our own.
    if fs::symlink_metadata(&socket).is_ok_and(|meta| meta.ino() == bound) {
        private_files::remove_file(&socket)?;
    }
    result?;
    if relaunch.load(std::sync::atomic::Ordering::SeqCst) {
        crate::service_log!("{{\"event\":\"service_configuration_relaunch\"}}");
        return crate::service_relaunch::execute(&service_lock);
    }
    Ok(())
}

async fn health(
    State(inspection): State<Arc<Inspection>>,
) -> Json<api::Response<crate::inspection::Health>> {
    Json(api::Response::new(inspection.health()))
}
async fn machine(State(inspection): State<Arc<Inspection>>) -> Response {
    match tokio::task::spawn_blocking(move || inspection.machine()).await {
        Ok(Ok(machine)) => Json(api::Response::new(machine)).into_response(),
        Ok(Err(error)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(api::ErrorResponse::new(error)),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(api::ErrorResponse::new(Failure::new(
                "inspection_failed",
                "Machine inspection failed.",
                "Run elsecall service restart.",
            ))),
        )
            .into_response(),
    }
}

fn configured_locations(
    paths: &Paths,
    configuration: &crate::configuration::ServiceConfiguration,
) -> Locations {
    Locations {
        setting: configuration.location,
        root: configuration
            .workspace_root
            .clone()
            .unwrap_or_else(|| paths.home.join(".elsecall/workspaces")),
        same_path_base: same_path_base(),
        state: paths.data.join("copies"),
    }
}

fn open_config_sync(
    paths: &Paths,
    locations: &Locations,
    fleet: &Arc<Fleet>,
    backup_limit: usize,
) -> Result<Arc<crate::config_sync::service::ConfigSync>> {
    use crate::config_sync::{
        replica::{Layout, Replica},
        service::ConfigSync,
        store::FileStore,
    };
    let mut layout = Layout::for_home(&paths.home, &paths.data);
    // Synced paths never touch Elsecall's own state or this member's copies.
    layout.forbidden = vec![paths.data.clone(), locations.root.clone()];
    let store = Arc::new(FileStore::open(&layout.state.join("objects"))?);
    let replica = Arc::new(Replica::open_with_backup_limit(
        layout,
        store,
        backup_limit,
    )?);
    Ok(ConfigSync::new(
        replica,
        fleet.clone(),
        paths.configuration(),
    ))
}

/// Stops a background task when the service stops.
struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct LocalListener(UnixListener);
impl axum::serve::Listener for LocalListener {
    type Io = UnixStream;
    type Addr = Peer;
    async fn accept(&mut self) -> (UnixStream, Peer) {
        loop {
            match self.0.accept().await {
                Ok((stream, _)) => {
                    if let Ok(peer) = stream.peer_cred()
                        && local_access::authorize_peer(peer.uid()).is_ok()
                    {
                        let pid = peer.pid().and_then(|pid| u32::try_from(pid).ok());
                        return (stream, Peer { pid });
                    }
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
    }
    fn local_addr(&self) -> std::io::Result<Peer> {
        Ok(Peer { pid: None })
    }
}
