//! `elsecall fleet` commands: thin requests to the local service, which holds the
//! member key and talks to the other members.
use crate::{
    client,
    failure::Result,
    fleet_local_api::{CodeRequest, ForceRequest, MemberRequest, SyncRequest},
    paths::Paths,
    roster::MemberName,
};
use clap::{Args, Subcommand};
use hyper::Method;
use serde_json::Value;
use std::time::Duration;

/// Fleet operations wait for every member's answer, each bounded by its own timeout.
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct FleetArgs {
    #[command(subcommand)]
    command: Option<FleetCommand>,
}

#[derive(Subcommand)]
enum FleetCommand {
    /// Print this member's name, key, endpoints and the code another member enrols it with
    Identity,
    /// Enrol another machine by the code its `fleet identity` printed (step 1 of 2)
    Enroll {
        code: String,
        /// Change the fleet even if some members are unreachable
        #[arg(long)]
        force: bool,
    },
    /// Join the fleet with the code `fleet enroll` printed on the enrolling member (step 2 of 2)
    Join { code: String },
    /// Remove another member from every reachable member's roster (step 1 of 2)
    Remove {
        name: MemberName,
        /// Change the fleet even if some members are unreachable
        #[arg(long)]
        force: bool,
    },
    /// Bring every member to this member's roster, e.g. after a forced change or a conflict
    Sync {
        /// Sync the reachable members even if some are unreachable
        #[arg(long)]
        force: bool,
        /// Replace a newer roster that other members record with this member's
        #[arg(long = "override")]
        override_newer: bool,
    },
    /// Move the master role to a member, updating every member
    MoveMaster {
        name: MemberName,
        /// Move even if some members are unreachable; they keep trusting the old master
        #[arg(long)]
        force: bool,
    },
    /// Ask a member whether it accepts placement authority from this member
    CheckAuthority { name: MemberName },
    /// On the master: send its installed build to every member, which installs it,
    /// restarts and confirms it is ready, or rolls back
    #[command(
        after_help = "Exits 0 when every member runs the build. A member with runs or leases waits for them up to --wait, then is deferred; nothing running is stopped. README \"Upgrading the fleet\" describes each outcome."
    )]
    Upgrade {
        /// Only this member
        #[arg(long)]
        member: Option<MemberName>,
        /// How long each member waits for its runs and leases to end (at most 1h)
        #[arg(long, default_value = "5m", value_parser = humantime::parse_duration)]
        wait: Duration,
        /// Distribute this build instead of the master's installed executable, for
        /// example to try a build on one member first
        #[arg(long)]
        executable: Option<std::path::PathBuf>,
    },
    /// Forget the fleet on this machine after it was removed elsewhere (step 2 of 2)
    Leave {
        /// Leave even though this member is the master
        #[arg(long)]
        force: bool,
    },
}

/// Runs a fleet command. Returns the data to print and whether the command fully
/// succeeded; a change that some members did not record exits 1.
pub async fn run(paths: &Paths, args: FleetArgs) -> Result<(Value, bool)> {
    let socket = paths.socket();
    let (method, route, body) = match args.command {
        Some(FleetCommand::Upgrade {
            member,
            wait,
            executable,
        }) => return upgrade(paths, member, wait, executable).await,
        None => (Method::GET, "/v1/fleet", None),
        Some(FleetCommand::Identity) => (Method::GET, "/v1/fleet/identity", None),
        Some(FleetCommand::Enroll { code, force }) => (
            Method::POST,
            "/v1/fleet/enroll",
            Some(serde_json::to_value(CodeRequest { code, force })),
        ),
        Some(FleetCommand::Join { code }) => (
            Method::POST,
            "/v1/fleet/join",
            Some(serde_json::to_value(CodeRequest { code, force: false })),
        ),
        Some(FleetCommand::Remove { name, force }) => (
            Method::POST,
            "/v1/fleet/remove",
            Some(serde_json::to_value(MemberRequest { name, force })),
        ),
        Some(FleetCommand::Sync {
            force,
            override_newer,
        }) => (
            Method::POST,
            "/v1/fleet/sync",
            Some(serde_json::to_value(SyncRequest {
                force,
                override_newer,
            })),
        ),
        Some(FleetCommand::MoveMaster { name, force }) => (
            Method::POST,
            "/v1/fleet/master",
            Some(serde_json::to_value(MemberRequest { name, force })),
        ),
        Some(FleetCommand::CheckAuthority { name }) => (
            Method::POST,
            "/v1/fleet/placement-probe",
            Some(serde_json::to_value(MemberRequest { name, force: false })),
        ),
        Some(FleetCommand::Leave { force }) => (
            Method::POST,
            "/v1/fleet/leave",
            Some(serde_json::to_value(ForceRequest { force })),
        ),
    };
    let body = body.map(|b| b.expect("serializable request"));
    let data: Value = client::send(&socket, method, route, body, TIMEOUT).await?;
    let complete = data
        .get("complete")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    Ok((data, complete))
}

/// Distributes a build. It takes as long as members' work and restarts take, so the
/// request waits for the service's answer without the usual bound.
async fn upgrade(
    paths: &Paths,
    member: Option<MemberName>,
    wait: Duration,
    executable: Option<std::path::PathBuf>,
) -> Result<(Value, bool)> {
    let executable = executable
        .map(|path| {
            std::fs::canonicalize(&path).map_err(|e| {
                crate::failure::Failure::new(
                    "build_unreadable",
                    format!("The build at {} could not be read: {e}", path.display()),
                    "Pass the path of a built elsecall executable.",
                )
            })
        })
        .transpose()?;
    let request = crate::fleet_upgrade::master::UpgradeRequest {
        member,
        wait_seconds: wait.as_secs(),
        executable,
    };
    let data: Value = client::post_large(&paths.socket(), "/v1/fleet/upgrade", &request).await?;
    let complete = data
        .get("complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok((data, complete))
}
