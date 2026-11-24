use crate::fleet_callers::FleetCallers;
use crate::{peer_api, peer_listener};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use elsecall_contract::api;
use elsecall_contract::failure::{Failure, Result};
use elsecall_contract::inspection::Inspection;
use elsecall_fleet::{fleet::Fleet, fleet_local_api, fleet_placement::FleetMembers};
use elsecall_host::{local_access, paths::Paths, private_files};
use elsecall_peers::{member_key::MemberKey, membership_store::MembershipStore};
use elsecall_runs::run_relay;
use elsecall_runs::{placement_api, placement_host::PlacementHost};
use elsecall_scheduling::service::{Adapters, Scheduler};
use elsecall_workspace::housekeeping::{
    self,
    caller_state::CallerState,
    callers::{Callers, Nobody},
    catalog::Catalog,
    pass::{Housekeeper, Parts},
};
use elsecall_workspace::{copies::Copies, location::Locations, object_sources::ObjectSources};
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

/// Hands every local request the process that made it, for the slices that
/// identify their caller.
async fn identify_caller(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<Peer>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    request
        .extensions_mut()
        .insert(elsecall_host::local_access::LocalCaller { pid: peer.pid });
    next.run(request).await
}

/// Tuning trust evidence for reports, from rehearsal's certificates.
struct TuningTrust;
impl elsecall_reports::report_api::TrustEvidence for TuningTrust {
    fn fleet_status<'a>(
        &'a self,
        paths: &'a elsecall_host::paths::Paths,
        fleet: &'a std::result::Result<Arc<elsecall_fleet::fleet::Fleet>, Failure>,
    ) -> futures_util::future::BoxFuture<'a, serde_json::Value> {
        Box::pin(elsecall_tuning::tuning_trust_api::fleet_status(
            paths, fleet,
        ))
    }
}

/// The scheduler's placed runs, as the fleet view shows them: the latest with
/// each member, and the missing tools no pin in the run's workspace (or the
/// global mise config under `home`) covers yet.
struct PlacedRunsOf(Arc<Scheduler>, std::path::PathBuf);
impl elsecall_fleet::fleet_view::PlacedRuns for PlacedRunsOf {
    fn tool_history(&self) -> elsecall_fleet::fleet_view::PlacementTools {
        use elsecall_tools::pins::lookup;
        let mut lookups = std::collections::BTreeMap::new();
        let history = self.0.tool_history();
        let unpinned = history
            .missing
            .into_iter()
            .filter_map(|standing| {
                let workspace = standing.workspace.clone();
                let read = lookups.entry(workspace.clone()).or_insert_with(|| {
                    lookup::read(workspace.as_deref().map(std::path::Path::new), &self.1)
                });
                let manager = standing.missing.manager.as_str();
                let pin_in = match read {
                    Ok(pins) if pins.pins(manager, standing.missing.tool.as_str()) => return None,
                    Ok(pins) => pins.file_for(manager),
                    // A workspace that cannot be read (moved, or a file in it
                    // malformed) leaves the item standing, naming the usual file.
                    Err(_) => lookup::default_file(manager, workspace.is_some()),
                };
                Some(elsecall_fleet::fleet_view::UnpinnedTool { standing, pin_in })
            })
            .collect();
        elsecall_fleet::fleet_view::PlacementTools {
            unpinned,
            exclusions: history.exclusions,
        }
    }

    fn latest_with(&self, member: &str) -> Option<elsecall_fleet::fleet_view::PlacedRunSummary> {
        self.0.latest_placed_with(member).map(|record| {
            elsecall_fleet::fleet_view::PlacedRunSummary {
                run_id: record.run_id.to_string(),
                started_at: record.started_at,
                warnings: record.warnings,
            }
        })
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
    process_sampler: Arc<dyn elsecall_scheduling::outside_pressure::ProcessSampler>,
    members_listener: peer_listener::Settings,
) -> Result<()> {
    elsecall_lifecycle::release_update::rehearsal::startup()?;
    let updates = elsecall_lifecycle::release_update::service::UpdateAdapters::production(
        !elsecall_macos::process_service::requested()?,
    )?;
    serve_with_updates(
        paths,
        inspection,
        scheduling,
        process_sampler,
        members_listener,
        updates,
    )
    .await
}

pub async fn serve_with_updates(
    paths: Paths,
    inspection: Arc<Inspection>,
    scheduling: Adapters,
    process_sampler: Arc<dyn elsecall_scheduling::outside_pressure::ProcessSampler>,
    members_listener: peer_listener::Settings,
    updates: elsecall_lifecycle::release_update::service::UpdateAdapters,
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
    elsecall_peers::peer_diagnostics::initialize(&paths)?;
    if let Err(error) = elsecall_host::state_temps::prune(&paths) {
        // Optional sync/upgrade state must not take down local admission.
        elsecall_host::service_log!(
            "{}",
            serde_json::json!({"event":"state_scratch_cleanup_failed", "code":error.code})
        );
    }
    // Identify the build before anything could replace this executable, and name
    // it on every member request from now on.
    let build = elsecall_contract::build_identity::announce()?;
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
    let mut configuration = elsecall_sync::service_configuration::load(&paths, name.as_ref())?;
    // Placement prefers members that have what a workspace pins, as the
    // provisioning loop, started below, finds them.
    let members = Arc::new(crate::placement_pins::PinnedMembers {
        members: Arc::new(FleetMembers::new(fleet.as_ref().ok().cloned())),
        provisioning: std::sync::OnceLock::new(),
    });
    let load_log = Arc::new(elsecall_scheduling::load_log::LoadLog::open(
        &paths.load_history(),
    )?);
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
    elsecall_host::service_log::configure(configuration.logs.clone());
    if let (Ok(fleet), Ok(sync)) = (&fleet, &config_sync) {
        fleet.report_sync(sync.clone());
        fleet.hand_over_with(sync.clone());
        fleet.observe_role_with(Arc::new(elsecall_sync::config_sync::service::RoleChanges(
            sync.clone(),
        )));
    }
    if let (Ok(fleet), Err(error)) = (&fleet, &config_sync) {
        elsecall_host::service_log!(
            "{{\"event\":\"config_sync_unavailable\",\"code\":{:?}}}",
            error.code
        );
        // A move must not skip the handover because sync is broken here.
        fleet.hand_over_with(Arc::new(
            elsecall_sync::config_sync::service::UnavailableHandover(error.clone()),
        ));
    }
    // Placed runs bring their member's synced configuration up to date first.
    let pre_run = Arc::new(crate::placement_config_sync::PlacementConfigSync::new(
        config_sync.clone(),
    ));
    let (scheduler, _scheduling) = Scheduler::start(
        &paths.records(),
        (&configuration).into(),
        scheduling,
        members.clone(),
        pre_run,
        load_log.clone(),
    )?;
    if let Ok(fleet) = &fleet {
        fleet.report_grants(scheduler.clone());
    }
    if let Ok(sync) = &config_sync {
        sync.report_effective_settings(scheduler.effective_settings_fingerprint());
    }
    // Broken outside pressure history must not take down admission.
    let outside_pressure = elsecall_scheduling::outside_pressure::Detector::open(
        &paths.outside_pressure(),
        process_sampler.clone(),
    )
    .map(Arc::new);
    // Nor must broken early warnings; they are shares of physical memory.
    let outside_processes = inspection
        .machine()
        .and_then(|machine| {
            elsecall_scheduling::outside_pressure::Watcher::open(
                &paths.outside_processes(),
                process_sampler,
                machine.memory.total_bytes,
            )
        })
        .map(Arc::new);
    let _outside_processes = match &outside_processes {
        Ok(watcher) => Some(AbortOnDrop(
            tokio::spawn(elsecall_scheduling::outside_pressure::watcher::follow(
                watcher.clone(),
                Arc::new(elsecall_scheduling::outside_pressure_gate::SchedulerGate(
                    scheduler.clone(),
                )),
            ))
            .abort_handle(),
        )),
        Err(error) => {
            elsecall_host::service_log!(
                "{}",
                serde_json::json!({"event":"outside_processes_unavailable", "code":error.code})
            );
            None
        }
    };
    let warnings = outside_processes
        .as_ref()
        .map(|watcher| {
            watcher.clone() as Arc<dyn elsecall_scheduling::outside_pressure::WarningReport>
        })
        .map_err(Clone::clone);
    if let (Ok(fleet), Ok(warnings)) = (&fleet, &warnings) {
        fleet.report_warnings(warnings.clone());
    }
    let _outside_pressure = match &outside_pressure {
        Ok(detector) => Some(AbortOnDrop(
            tokio::spawn(elsecall_scheduling::outside_pressure::detector::follow(
                detector.clone(),
                Arc::new(elsecall_scheduling::outside_pressure_gate::SchedulerGate(
                    scheduler.clone(),
                )),
                elsecall_scheduling::service::SAMPLE_INTERVAL,
            ))
            .abort_handle(),
        )),
        Err(error) => {
            elsecall_host::service_log!(
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
        Ok(fleet) => elsecall_lifecycle::fleet_upgrade::member::Upgrades::open(
            elsecall_lifecycle::fleet_upgrade::journal::Journal::new(&paths.data),
            paths.executable(),
            fleet.clone(),
            Arc::new(elsecall_lifecycle::upgrade_drain::SchedulerWork(
                scheduler.clone(),
            )),
            Arc::new(elsecall_lifecycle::upgrade_helper_process::HelperProcess),
            build.clone(),
        )
        .map(Arc::new),
        Err(error) => Err(error.clone()),
    };
    let automatic_updates = updates.automatic;
    // The CLI's `fleet upgrade` and a release update's fleet half share one distributor.
    let distributor = fleet.as_ref().ok().map(|fleet| {
        elsecall_lifecycle::fleet_upgrade::distributor::Distributor::new(
            fleet.clone(),
            paths.executable(),
        )
    });
    let updates = elsecall_lifecycle::release_update::service::UpdateService::open(
        paths.clone(),
        elsecall_lifecycle::release_update::ReleaseVersion::running(),
        build.build.clone(),
        updates.catalog,
        updates.launcher,
        scheduler.clone(),
    );
    if let (Ok(updates), Some(distributor)) = (&updates, &distributor) {
        updates.distribute_with(distributor.clone());
    }
    let _update_monitor = updates
        .as_ref()
        .ok()
        .map(|updates| AbortOnDrop(updates.monitor().abort_handle()));
    let _release_checks = updates
        .as_ref()
        .ok()
        .filter(|_| automatic_updates)
        .map(|updates| {
            AbortOnDrop(
                updates
                    .schedule(std::time::Duration::from_secs(3600))
                    .abort_handle(),
            )
        });
    let update_routes = match updates {
        Ok(updates) => elsecall_lifecycle::release_update_api::routes(updates),
        Err(error) => elsecall_lifecycle::release_update_api::unavailable(error),
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
    let rehearsal = elsecall_tuning::rehearsal::member::Member::new(
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
        paths.home.clone(),
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
    let reporter = elsecall_reports::report_api::Reporter {
        paths: paths.clone(),
        own: elsecall_reports::own_history::OwnHistory {
            scheduler: scheduler.clone(),
            load: load_log.clone(),
            outside: outside_detector,
            warnings: warnings.ok(),
        },
        fleet: fleet.clone(),
        evidence: Arc::new(TuningTrust),
    };
    let sync_routes = match &config_sync {
        Ok(sync) => elsecall_sync::config_sync_local_api::routes(sync.clone()),
        Err(error) => elsecall_sync::config_sync_local_api::unavailable(error.clone()),
    };
    let run_workspaces = Arc::new(elsecall_runs::run_workspaces::RunWorkspaces::open(
        &paths.data,
    ));
    let recent_workspaces: Arc<dyn elsecall_tools::plan_api::RecentWorkspaces> = Arc::new(
        crate::run_workspace_pins::RunWorkspacePins(run_workspaces.clone()),
    );
    let applier = elsecall_tools::install::Applier::new(
        paths.data.clone(),
        Arc::new(scheduler.clone()),
        fleet.clone(),
    );
    if let Ok(fleet) = &fleet {
        fleet.report_provisioning(applier.clone());
    }
    let planning = |recent| elsecall_tools::plan_api::Planning::new(fleet.clone(), &paths, recent);
    let drift = elsecall_tools::drift::Drift::new(
        recent_workspaces.clone(),
        applier.clone(),
        build.build.as_str().to_owned(),
        &paths.data,
    );
    // The master keeps every member in step with the pins by itself.
    let provisioning = elsecall_tools::provisioning_loop::ProvisioningLoop::new(
        planning(recent_workspaces.clone()),
        applier.clone(),
        drift.clone(),
    );
    let _provisioning = AbortOnDrop(provisioning.start().abort_handle());
    let _ = members.provisioning.set(provisioning.clone());
    let tools_routes = elsecall_tools::tools_api::routes(fleet.clone())
        .merge(elsecall_tools::plan_api::routes(planning(
            recent_workspaces.clone(),
        )))
        .merge(elsecall_tools::apply_api::routes(
            planning(recent_workspaces.clone()),
            applier.clone(),
        ))
        .merge(elsecall_tools::provisioning_api::routes(provisioning));
    let desired_routes = elsecall_tools::toolset_api::routes(fleet.clone(), &paths);
    let fleet_routes = match fleet {
        Ok(fleet) => {
            let mut member_routes = placement_api::routes(host)
                .merge(crate::workspace_check_api::routes(caller_state))
                .merge(elsecall_reports::report_api::peer_routes(reporter.clone()))
                .merge(elsecall_tuning::trace_api::peer_routes(reporter.clone()))
                .merge(elsecall_tuning::rehearsal_api::peer_routes(
                    rehearsal.clone(),
                ))
                .merge(elsecall_tuning::tuning_trust_api::peer_routes(
                    paths.clone(),
                ))
                .merge(elsecall_tools::tools_api::peer_routes())
                .merge(elsecall_tools::plan_api::peer_routes(recent_workspaces))
                .merge(elsecall_tools::apply_api::peer_routes(applier))
                .merge(elsecall_tools::drift::peer_routes(drift));
            if let Ok(sync) = &config_sync {
                member_routes =
                    member_routes.merge(elsecall_sync::config_sync_api::routes(sync.clone()));
            }
            member_routes = member_routes.merge(match &upgrades {
                Ok(upgrades) => elsecall_lifecycle::fleet_upgrade_api::routes(upgrades.clone()),
                Err(error) => elsecall_lifecycle::fleet_upgrade_api::unavailable(error.clone()),
            });
            let peer_routes = peer_api::routes(fleet.clone(), member_routes);
            peer_listener::start(fleet.clone(), members_listener, peer_routes).await;
            elsecall_tuning::rehearsal_api::routes(fleet.clone(), rehearsal)
                .merge(elsecall_lifecycle::fleet_upgrade_local_api::routes(
                    distributor
                        .clone()
                        .expect("a readable fleet has a distributor"),
                ))
                .merge(fleet_local_api::routes(
                    fleet,
                    Arc::new(PlacedRunsOf(scheduler.clone(), paths.home.clone())),
                ))
        }
        Err(error) => {
            elsecall_host::service_log!(
                "{{\"event\":\"fleet_unavailable\",\"code\":{:?}}}",
                error.code
            );
            fleet_local_api::unavailable(error)
        }
    };
    let _drift = AbortOnDrop(elsecall_tuning::tuning_drift::start(
        paths.clone(),
        scheduler.clone(),
        configuration.housekeeping.interval,
    ));
    let trust_routes =
        elsecall_tuning::tuning_trust_api::routes(paths.clone(), reporter.fleet.clone());
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
        .merge(elsecall_tuning::replay_api::routes(scheduler.clone()))
        .merge(elsecall_runs::admission_api::routes(
            scheduler,
            run_workspaces,
        ))
        .merge(elsecall_workspace::copies_api::routes(housekeeper))
        .merge(tools_routes)
        .merge(desired_routes)
        .merge(crate::ui_app::routes(paths.clone()))
        .merge(update_routes)
        .merge(elsecall_tuning::trace_api::routes(reporter.clone()))
        .merge(elsecall_reports::report_api::routes(reporter))
        .merge(relay)
        .merge(sync_routes)
        .merge(outside_pressure_routes)
        .merge(fleet_routes);
    // Register handlers before signaling readiness, and release the socket on shutdown.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| Failure::io("Register shutdown handler", e))?;
    elsecall_host::service_log!(
        "{{\"event\":\"service_ready\",\"version\":\"{}\",\"build\":\"{}\"}}",
        build.version,
        build.build
    );
    let result = axum::serve(
        LocalListener(listener),
        app.layer(axum::middleware::from_fn(identify_caller))
            .into_make_service_with_connect_info::<Peer>(),
    )
    .with_graceful_shutdown(async move {
        tokio::select! {
            _ = terminate.recv() => (),
            _ = tokio::signal::ctrl_c() => (),
            _ = elsecall_sync::service_relaunch::when_idle(restart_sync, restart_scheduler) => {
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
        elsecall_host::service_log!("{{\"event\":\"service_configuration_relaunch\"}}");
        return elsecall_sync::service_relaunch::execute(&service_lock);
    }
    Ok(())
}

async fn health(
    State(inspection): State<Arc<Inspection>>,
) -> Json<api::Response<elsecall_contract::inspection::Health>> {
    let mut health = inspection.health();
    health.open_file_limit = elsecall_macos::macos::open_files::soft_limit();
    Json(api::Response::new(health))
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
    configuration: &elsecall_sync::configuration::ServiceConfiguration,
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
) -> Result<Arc<elsecall_sync::config_sync::service::ConfigSync>> {
    use elsecall_sync::config_sync::{
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
