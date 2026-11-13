use clap::{Parser, Subcommand};
use elsecall::{
    admission_cli::{self, LeaseCommand},
    api, client,
    failure::{Failure, Result},
    fleet_cli,
    inbound_firewall::{Inbound, InboundFirewall},
    inspection::{Health, Inspection, Machine},
    installation::{self, ServiceManager},
    local_access,
    macos::{
        application_firewall::ApplicationFirewall, launch_agent::LaunchAgent, machine::MacMachine,
        processes::MacProcesses, tailscale::TailscaleCli,
    },
    paths::Paths,
    peer_listener, scheduling, tailnet,
};
use serde::Serialize;
use std::{process::ExitCode, sync::Arc, time::Duration};

#[derive(Parser)]
#[command(
    version,
    about = "Inspect this machine and admit work onto it through the local Elsecall service",
    after_help = "Commands return one JSON document. Errors exit 1; invalid arguments exit 2. Help and version are text."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install or update the CLI and background service for this user
    Install {
        /// Where the service runs. Updates keep the current session; new installations
        /// use the desktop session if there is one, else the background session.
        #[arg(long, value_enum)]
        session: Option<SessionArg>,
    },
    /// Stop the service and remove Elsecall-owned installation files
    Uninstall {
        /// Uninstall even though this machine is enrolled in a fleet (deletes its member key)
        #[arg(long)]
        force: bool,
    },
    /// Check whether the local API is ready
    Status,
    /// Read basic machine facts, memory and load averages
    Machine,
    /// Report installed tools, or show and edit the fleet's desired versions
    Tools(elsecall::tools_cli::ToolsArgs),
    /// Diagnose installation and API connectivity (works while the service is down)
    Doctor,
    /// Show every fleet member's reachability, role and machine state, or change membership
    Fleet(fleet_cli::FleetArgs),
    /// Mirror declared configuration and secret files from the master to every
    /// member: status, plan, enable, disable, push and history
    #[command(
        after_help = "Output names files and shows short keyed fingerprints only, never their content. README \"Configuration sync\" describes the rules."
    )]
    Sync(elsecall::config_sync_cli::SyncArgs),
    /// Manage the installed background service
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Run the local service in the foreground for diagnosis
    Serve,
    /// Run a command once this machine has room, passing its input, output,
    /// signals and exit code through; Elsecall's own failures exit 125
    #[command(
        after_help = "Prints nothing of its own unless Elsecall fails; then the last stderr line is the JSON error envelope. Use --status-file to tell the command's own exit 125 from Elsecall's."
    )]
    Run(elsecall::passthrough::RunArgs),
    /// Hold capacity on this machine for a section of a script's own work
    Lease {
        #[command(subcommand)]
        command: LeaseCommand,
    },
    /// Record terminal observations from an admitted workload wrapper
    Workload(elsecall::workload_observation_cli::WorkloadArgs),
    /// List recent runs and leases, with this machine's capacity and load
    Runs {
        /// How many records to list, newest first
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Show one run or lease record with its admission decision in full
    Explain {
        /// The run or lease ID, such as r-3f9a0c41d2b7
        run_id: String,
    },
    /// List the copies of other members' workspaces kept on this machine, their
    /// object stores and recent removals, or run housekeeping now
    Copies(elsecall::copies_cli::CopiesArgs),
    /// Score this member's recorded placement decisions again under changed
    /// parameters, to see which would flip before changing config.toml
    Replay(elsecall::replay_cli::ReplayArgs),
    /// Installs a build the master distributed; the service starts this itself
    #[command(hide = true)]
    UpgradeHelper {
        #[arg(long)]
        id: String,
    },
    /// How the fleet did over a period: load and balance per member, queue waits,
    /// fleet_busy, the placement mix, estimate accuracy and the most notable findings
    #[command(
        after_help = "Asks every member for its requests and load history over the member channel; a member that cannot answer is named and the rest of the report stands. README \"Fleet reports\" defines every metric."
    )]
    Report(elsecall::report_cli::ReportArgs),
    /// Export a versioned workload trace and empirical resource models
    Trace(elsecall::trace_cli::TraceArgs),
    /// Replay workload traces through the real scheduler on a virtual fleet
    Simulate(elsecall::simulate_cli::SimulateArgs),
    /// Replay a workload as bounded synthetic work on the enrolled fleet
    Rehearse(elsecall::rehearse_cli::RehearseArgs),

    /// Search scheduling settings offline and print proposals only
    Tune(elsecall::tune_cli::TuneArgs),
}
#[derive(Clone, Copy, clap::ValueEnum)]
enum SessionArg {
    /// Start at desktop login, stop at logout
    Desktop,
    /// Run without a desktop login and across ssh disconnects
    Background,
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Stop and start the installed service, then wait until it is ready
    Restart,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) if error.use_stderr() => {
            let arguments: Vec<_> = std::env::args_os().skip(1).take(2).collect();
            if arguments.first().is_some_and(|command| command == "run") {
                return elsecall::passthrough::usage_failure();
            }
            // Do not echo argument values: callers may accidentally pass secrets.
            let failure = if arguments == ["lease", "acquire"] {
                // A run ID file named among the options must not keep a stale ID.
                let all: Vec<_> = std::env::args_os().collect();
                if let Some(path) =
                    elsecall::passthrough::option_argument(&all, "acquire", "--run-id-file")
                {
                    let _ = elsecall::run_id_file::RunIdFile::at(path).record(None);
                }
                let failure = Failure::new(
                    "invalid_arguments",
                    "Invalid lease acquire options.",
                    "Run elsecall lease acquire --help for valid options, then rerun the corrected command.",
                );
                elsecall::guidance::write(&elsecall::guidance::failure_line(&failure));
                failure
            } else {
                Failure::new(
                    "invalid_arguments",
                    "Invalid command or arguments.",
                    "Run elsecall --help for supported commands.",
                )
            };
            print_error(failure);
            return ExitCode::from(2);
        }
        Err(error) => {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
    };
    let cli = match cli.command {
        Command::Tune(mut args) => {
            if let Some(elsecall::tune_cli::TuneCommand::Preflight(preflight)) = args.command.take()
            {
                return match elsecall::tuning_preflight_cli::run(preflight).await {
                    Ok(data) => {
                        let passed = data["passed"] == true;
                        output(data);
                        if passed {
                            ExitCode::SUCCESS
                        } else {
                            ExitCode::FAILURE
                        }
                    }
                    Err(error) => fail(error),
                };
            }
            return match tokio::task::spawn_blocking(move || elsecall::tune_cli::run(args)).await {
                Ok(Ok(data)) => {
                    output(elsecall::tune_cli::current_trust(data).await);
                    ExitCode::SUCCESS
                }
                Ok(Err(error)) => fail(error),
                Err(_) => fail(Failure::new(
                    "tuning_failed",
                    "The tuning task failed.",
                    "Check the trace and retry.",
                )),
            };
        }
        Command::Simulate(args) => {
            return match tokio::task::spawn_blocking(move || elsecall::simulate_cli::run(args))
                .await
            {
                Ok(Ok(data)) => {
                    output(data);
                    ExitCode::SUCCESS
                }
                Ok(Err(error)) => fail(error),
                Err(_) => fail(Failure::new(
                    "simulation_failed",
                    "The simulation task failed.",
                    "Check the trace and retry.",
                )),
            };
        }
        // Run passes the command's output through rather than printing JSON.
        Command::Run(args) => return elsecall::passthrough::main(args).await,
        // Leases check the platform and user themselves, so ELSECALL=off needs neither.
        Command::Lease { command } => {
            let acquire = matches!(command, LeaseCommand::Acquire { .. });
            return match admission_cli::lease(command).await {
                Ok(data) => {
                    output(data);
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    if acquire {
                        elsecall::guidance::write(&elsecall::guidance::failure_line(&error));
                    }
                    fail(error)
                }
            };
        }
        command => Cli { command },
    };
    match run(cli).await {
        Ok(healthy) => {
            if healthy {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => fail(error),
    }
}

/// Prints a failure and exits 2 for an invalid request, 1 otherwise.
fn fail(error: Failure) -> ExitCode {
    let code = if error.code == "invalid_request" {
        2
    } else {
        1
    };
    print_error(error);
    ExitCode::from(code)
}

fn output<T: Serialize>(data: T) {
    println!(
        "{}",
        serde_json::to_string(&api::Response::new(data)).expect("serializable response")
    );
}
fn print_error(error: Failure) {
    if elsecall::service_log::active() {
        elsecall::service_log!(
            "{}",
            serde_json::json!({"event":"service_failed", "code":error.code})
        );
    }
    println!(
        "{}",
        serde_json::to_string(&api::ErrorResponse::new(error)).expect("serializable failure")
    );
}

async fn run(cli: Cli) -> Result<bool> {
    if !cfg!(target_os = "macos") {
        return Err(Failure::new(
            "unsupported_platform",
            "This version supports macOS only.",
            "Linux support is planned; no files have been changed.",
        ));
    }
    local_access::require_user()?;
    let paths = Paths::current()?;
    if matches!(&cli.command, Command::Serve | Command::UpgradeHelper { .. }) {
        elsecall::service_log::initialize(&paths)?;
    }
    let manager: Box<dyn ServiceManager> = if elsecall::process_service::requested()? {
        Box::new(elsecall::process_service::ProcessService::new(
            paths.clone(),
        ))
    } else {
        Box::new(LaunchAgent)
    };
    let manager = &*manager;
    match cli.command {
        Command::Install { session } => {
            let source =
                std::env::current_exe().map_err(|e| Failure::io("Find current executable", e))?;
            let session = session.map(|session| match session {
                SessionArg::Desktop => installation::Session::Desktop,
                SessionArg::Background => installation::Session::Background,
            });
            let installed = installation::install(&paths, &source, manager, session)?;
            wait_ready(&paths).await?;
            // Other members can reach the service only if the firewall lets them.
            let mut data = serde_json::to_value(installed).expect("serializable installation");
            data["member_listener_firewall"] =
                serde_json::to_value(ApplicationFirewall.inbound(&paths.executable()))
                    .expect("serializable firewall state");
            output(data);
        }
        Command::Uninstall { force } => {
            installation::uninstall(&paths, manager, force)?;
            output(serde_json::json!({"status": "uninstalled"}));
        }
        Command::Status => output(client::get::<Health>(&paths.socket(), "/v1/health").await?),
        Command::Machine => output(client::get::<Machine>(&paths.socket(), "/v1/machine").await?),
        Command::Tools(args) => output(elsecall::tools_cli::run(&paths, args).await?),
        Command::Doctor => {
            let configuration = elsecall::service_configuration::load(&paths, None);
            let log_settings = configuration
                .as_ref()
                .map(|c| c.logs.clone())
                .unwrap_or_default();
            let state = elsecall::state_sizes::inspect(&paths, &log_settings);
            let health = client::get::<Health>(&paths.socket(), "/v1/health").await;
            let loaded = manager.loaded();
            let installed = installation::installed(&paths);
            let launch_agent_current = installation::launch_agent_current(&paths, manager);
            // Fleet problems make doctor unhealthy only for an enrolled member; on a solo
            // machine they are reported for information.
            let membership =
                client::get::<serde_json::Value>(&paths.socket(), "/v1/fleet/identity").await;
            let enrolled = paths.fleet_membership().exists();
            let firewall = ApplicationFirewall.inbound(&paths.executable());
            let reachable =
                !enrolled || (!matches!(firewall, Inbound::Blocked { .. }) && membership.is_ok());
            let healthy = matches!(installed, Ok(true))
                && matches!(loaded, Ok(true))
                && health.is_ok()
                && reachable;
            // Version skew is a fleet condition, not this machine's health: it is
            // flagged with its repair, and doctor's verdict stays about this machine.
            let fleet = if enrolled && health.is_ok() {
                Some(
                    client::get::<elsecall::fleet_view::FleetBuilds>(
                        &paths.socket(),
                        "/v1/fleet/builds",
                    )
                    .await,
                )
            } else {
                None
            };
            let (fleet_builds, version_skew, mut blocked, mut warnings, fleet_error) = match fleet {
                Some(Ok(builds)) => (
                    Some(builds.members),
                    builds.version_skew,
                    builds.blocked,
                    builds.outside_warnings,
                    None,
                ),
                Some(Err(error)) => (None, None, Vec::new(), Vec::new(), Some(error)),
                None => (None, None, Vec::new(), Vec::new(), None),
            };
            // This member's own report covers a member outside any fleet, and one
            // whose fleet view failed.
            let this_member = if health.is_ok() {
                Some(
                    client::get::<elsecall::outside_pressure_api::ThisMember>(
                        &paths.socket(),
                        elsecall::outside_pressure_api::PATH,
                    )
                    .await,
                )
            } else {
                None
            };
            let outside_pressure_error = match this_member {
                Some(Ok(this)) => {
                    if !warnings.iter().any(|w| w.member == this.member) {
                        warnings.extend(this.warnings.into_iter().map(|warning| {
                            elsecall::fleet_view::MemberWarning {
                                member: this.member.clone(),
                                warning,
                            }
                        }));
                    }
                    if let Some(own) = this.blocked
                        && !blocked.iter().any(|b| b.member == this.member)
                    {
                        blocked.push(elsecall::fleet_view::BlockedMember {
                            member: this.member,
                            blocked: own,
                        });
                    }
                    None
                }
                // A service that predates the report has no such route.
                Some(Err(error)) if error.code == "not_found" => None,
                Some(Err(error)) => Some(error),
                None => None,
            };
            // The longest-blocked member first, and the longest-standing warning.
            blocked.sort_by_key(|b| b.blocked.since);
            warnings.sort_by_key(|w| w.warning.since);
            let service_configurations = if health.is_ok() {
                elsecall::client::post_large::<serde_json::Value, _>(
                    &paths.socket(),
                    "/v1/sync",
                    &serde_json::json!({}),
                )
                .await
                .ok()
            } else {
                None
            };
            let next_step = if !healthy {
                if !reachable {
                    "Other members cannot reach this member: follow member_identity_error.hint, or run the commands in member_listener_firewall.commands (they need administrator rights)."
                } else {
                    "Run elsecall install to repair or start the service; inspect the service log if startup fails."
                }
                .to_owned()
            } else if version_skew.is_some() {
                "Members run different builds, so work between them is refused: follow version_skew.hint.".to_owned()
            } else if launch_agent_current == Some(false) {
                "The LaunchAgent predates this build (a fleet upgrade installs through the build it replaces): run elsecall install here to apply it; it restarts the service.".to_owned()
            } else if let Some(first) = blocked.first() {
                elsecall::outside_pressure::remedy::next_step(
                    &first.member,
                    &first.blocked,
                    elsecall::scheduling::timestamp::Timestamp::now(),
                )
            } else if service_configurations
                .as_ref()
                .and_then(|s| s["members"].as_array())
                .is_some_and(|members| {
                    members.iter().any(|m| {
                        m["sync"]["service"]["current"] == false
                            || !m["sync"]["service"]["rejection"].is_null()
                    })
                })
            {
                "Service configuration needs attention: follow configuration_sync.next_step and each member's service state.".to_owned()
            } else if let Some(first) = warnings.first() {
                elsecall::outside_pressure::remedy::warning_step(
                    &first.member,
                    &first.warning,
                    elsecall::scheduling::timestamp::Timestamp::now(),
                )
            } else {
                "No action needed.".to_owned()
            };
            output(serde_json::json!({
                "healthy": healthy,
                "state": state,
                "configuration_error": configuration.err().map(|error| serde_json::json!({"code":error.code, "hint":error.hint})),
                "installation_present": installed.as_ref().ok(),
                "installation_error": installed.err(),
                "launch_agent_loaded": loaded.as_ref().ok(),
                "session": installation::session(&paths, manager),
                "launch_agent_current": launch_agent_current,
                "launch_agent_error": loaded.err(),
                "service": health.as_ref().ok(),
                "service_error": health.err(),
                "fleet_enrolled": enrolled,
                "member_identity": membership.as_ref().ok(),
                "member_identity_error": membership.as_ref().err(),
                "member_listener_firewall": firewall,
                "fleet_builds": fleet_builds,
                "fleet_error": fleet_error,
                "version_skew": version_skew,
                "outside_pressure": blocked,
                "outside_pressure_error": outside_pressure_error,
                "outside_warnings": warnings,
                "configuration_sync": service_configurations,
                "paths": {"executable": paths.bin, "launch_agent": paths.agent, "socket": paths.socket(), "log": paths.log()},
                "next_step": next_step,
            }));
            return Ok(healthy);
        }
        Command::Service {
            command: ServiceCommand::Restart,
        } => {
            installation::restart(&paths, manager)?;
            output(wait_ready(&paths).await?);
        }
        Command::Serve => {
            let machine: Arc<dyn elsecall::inspection::MachineInspector> =
                if let Some(file) = elsecall::machine_script::load_file() {
                    Arc::new(elsecall::machine_script::ScriptedLoad {
                        machine: MacMachine::default(),
                        file,
                    })
                } else if elsecall::idle_load::requested() {
                    Arc::new(elsecall::idle_load::IdleLoad(MacMachine::default()))
                } else {
                    Arc::new(MacMachine::default())
                };
            let speed: Arc<dyn elsecall::scheduling::speed::SpeedMeter> =
                match elsecall::machine_script::speed() {
                    Some(fixed) => Arc::new(fixed),
                    None => Arc::new(elsecall::macos::core_benchmark::CoreBenchmark),
                };
            let (processes, exits) = MacProcesses::start()?;
            elsecall::server::serve(
                paths,
                Arc::new(Inspection::new(machine.clone())),
                scheduling::service::Adapters {
                    machine,
                    speed,
                    processes,
                    exits,
                },
                Arc::new(elsecall::macos::process_sampler::MacProcessSampler),
                member_listener()?,
            )
            .await?;
        }
        Command::Fleet(args) => {
            let (data, complete) = fleet_cli::run(&paths, args).await?;
            output(data);
            return Ok(complete);
        }
        Command::Sync(args) => {
            let (data, complete) = elsecall::config_sync_cli::run(&paths, args).await?;
            output(data);
            return Ok(complete);
        }
        Command::Run(_) | Command::Lease { .. } => {
            unreachable!("run and lease are dispatched before other commands")
        }
        Command::Workload(args) => {
            output(elsecall::workload_observation_cli::run(args, &paths).await?)
        }
        Command::Runs { limit } => output(admission_cli::runs(&paths, limit).await?),
        Command::Explain { run_id } => output(admission_cli::explain(&paths, &run_id).await?),
        Command::Report(args) => output(elsecall::report_cli::run(&paths, args).await?),
        Command::Trace(args) => output(elsecall::trace_cli::run(&paths, args).await?),
        Command::Rehearse(args) => {
            let result = elsecall::rehearse_cli::run(&paths, args).await?;
            let complete =
                result["cleanup_complete"] == true && result["rehearsal_error"].is_null();
            output(result);
            return Ok(complete);
        }

        Command::Tune(_) => unreachable!("handled before platform and service checks"),
        Command::Simulate(_) => unreachable!("handled before platform and service checks"),
        Command::Copies(args) => {
            let (data, clean) = elsecall::copies_cli::run(&paths, args).await?;
            output(data);
            return Ok(clean);
        }
        Command::Replay(args) => output(elsecall::replay_cli::run(&paths, args).await?),
        Command::UpgradeHelper { id } => {
            let id = serde_json::from_value(serde_json::Value::String(id)).map_err(|_| {
                Failure::new(
                    "invalid_request",
                    "The upgrade ID is invalid.",
                    "The service starts this helper itself; run elsecall fleet upgrade on the master.",
                )
            })?;
            let phase = elsecall::fleet_upgrade::helper::run(&paths, manager, &id).await?;
            output(phase);
        }
    }
    Ok(true)
}

async fn wait_ready(paths: &Paths) -> Result<Health> {
    let ready = async {
        loop {
            if let Ok(health) = client::get::<Health>(&paths.socket(), "/v1/health").await
                && health.version == env!("CARGO_PKG_VERSION")
            {
                return health;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(8), ready).await.map_err(|_| Failure::new(
        "startup_failed",
        "The installation is present but the service did not become ready within eight seconds.",
        "Run elsecall doctor and inspect its log path, then run elsecall service restart. Run elsecall uninstall to remove this installation.",
    ))
}

/// The member listener's discovery and port. `ELSECALL_TAILNET_NAME`,
/// `ELSECALL_TAILNET_ADDRESSES` and `ELSECALL_FLEET_PORT` override Tailscale discovery
/// and the default port, for diagnosis and for several members on one machine.
fn member_listener() -> Result<peer_listener::Settings> {
    let tailnet: Arc<dyn tailnet::Tailnet> = match tailnet::Configured::from_env() {
        Some(configured) => Arc::new(configured?),
        None => Arc::new(TailscaleCli),
    };
    let port = match std::env::var("ELSECALL_FLEET_PORT") {
        Err(_) => tailnet::DEFAULT_PORT,
        Ok(text) => text.parse().ok().filter(|port| *port != 0).ok_or_else(|| {
            Failure::new(
                "invalid_fleet_port",
                "ELSECALL_FLEET_PORT must be a TCP port from 1 to 65535.",
                "Unset it to use the default port.",
            )
        })?,
    };
    Ok(peer_listener::Settings { tailnet, port })
}
