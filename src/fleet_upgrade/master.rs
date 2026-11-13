//! The master distributing a build: it sends its installed executable (or an
//! explicitly named build) to each member over the upgrade channel, then follows
//! each member's upgrade record until it ends, and reports every member's outcome.
//! Members are upgraded at the same time; each one installs, confirms and, if
//! needed, rolls back on its own.
use crate::{
    build_identity::{Build, BuildId, Digest},
    executable_format::{self, Architecture},
    failure::{Failure, Result},
    fleet::Fleet,
    fleet_upgrade::{
        journal::{Phase, UpgradeState},
        member::{UpgradeStatus, WorkInFlight},
        protocol::{self, Manifest, Message, Os, UpgradeId},
    },
    peer_client,
    roster::{Member, MemberName},
};
use hyper::Method;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

/// The default bound on waiting for a member's runs and leases.
pub const DEFAULT_WAIT_SECONDS: u64 = 5 * 60;
/// Beyond the wait: the restart, readiness, and a possible rollback's.
const RESTART_ALLOWANCE: Duration = Duration::from_secs(90);
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const CONTENT_TIMEOUT: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(500);

#[derive(Debug, Serialize, Deserialize)]
pub struct UpgradeRequest {
    /// Only this member; every member but the master when absent.
    #[serde(default)]
    pub member: Option<MemberName>,
    #[serde(default = "default_wait")]
    pub wait_seconds: u64,
    /// The build to distribute; the master's installed executable when absent.
    #[serde(default)]
    pub executable: Option<PathBuf>,
}
fn default_wait() -> u64 {
    DEFAULT_WAIT_SECONDS
}

#[derive(Debug, Serialize)]
pub struct Distributed {
    pub build: BuildId,
    pub digest: Digest,
    pub size: u64,
    pub architectures: Vec<Architecture>,
    pub source: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// The member runs the build and its service is ready.
    Upgraded {
        from: Build,
        running: Build,
    },
    /// The member already ran the build; nothing was sent.
    AlreadyCurrent {
        running: Build,
    },
    /// The build did not become ready; the member runs its previous build again.
    RolledBack {
        from: Build,
        running: Build,
        error: Failure,
    },
    /// Neither build became ready; see the error's hint.
    RollbackFailed {
        from: Build,
        error: Failure,
    },
    /// Work still ran when the wait ended; nothing changed on the member.
    Deferred {
        from: Build,
        in_flight: Vec<WorkInFlight>,
        hint: String,
    },
    /// The member refused the build, such as `not_master`, `digest_mismatch`,
    /// `wrong_platform` or `upgrade_in_progress`; nothing changed on it.
    Refused {
        error: Failure,
    },
    /// The attempt ended without installing anything.
    Interrupted {
        error: Failure,
    },
    Unreachable {
        error: Failure,
    },
    /// The member did not report how the upgrade ended in time.
    Unknown {
        error: Failure,
        last_state: Option<serde_json::Value>,
    },
}
impl Outcome {
    pub fn complete(&self) -> bool {
        matches!(
            self,
            Outcome::Upgraded { .. } | Outcome::AlreadyCurrent { .. }
        )
    }
}

#[derive(Debug, Serialize)]
pub struct MemberUpgrade {
    pub name: MemberName,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade_id: Option<UpgradeId>,
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[derive(Debug, Serialize)]
pub struct UpgradeReport {
    pub distributed: Distributed,
    pub members: Vec<MemberUpgrade>,
    /// Whether every member now runs the build.
    pub complete: bool,
}

fn not_master(master: &MemberName) -> Failure {
    Failure::new(
        "not_master",
        format!("Only the master, {master}, distributes builds to the fleet."),
        format!("Run elsecall fleet upgrade on {master}."),
    )
}

/// Distributes a build from this member, which must be the recorded master.
pub async fn upgrade(
    fleet: &Fleet,
    installed: PathBuf,
    request: UpgradeRequest,
) -> Result<UpgradeReport> {
    let roster = fleet.roster()?;
    let master = roster.master();
    if master.key != fleet.public_key() {
        return Err(not_master(&master.name));
    }
    if request.wait_seconds > protocol::MAXIMUM_WAIT_SECONDS {
        return Err(Failure::new(
            "invalid_request",
            "--wait is at most an hour.",
            "Pass a shorter --wait.",
        ));
    }
    let targets: Vec<Member> = match &request.member {
        Some(name) if name == &master.name => {
            return Err(Failure::new(
                "invalid_request",
                "The master runs the build it distributes; it is not upgraded over the fleet.",
                "Update the master with elsecall install, then run elsecall fleet upgrade.",
            ));
        }
        Some(name) => vec![
            roster
                .member(name)
                .cloned()
                .ok_or_else(|| crate::roster::not_a_member(name))?,
        ],
        None => roster
            .members()
            .iter()
            .filter(|m| m.key != master.key)
            .cloned()
            .collect(),
    };
    let source = request.executable.unwrap_or(installed);
    let (bytes, distributed) = tokio::task::spawn_blocking(move || read_build(source))
        .await
        .map_err(|_| {
            Failure::new(
                "build_unreadable",
                "Reading the build failed.",
                "Run elsecall fleet upgrade again.",
            )
        })??;
    let members =
        futures_util::future::join_all(targets.iter().map(|member| {
            upgrade_member(fleet, member, &bytes, &distributed, request.wait_seconds)
        }))
        .await;
    let complete = members.iter().all(|m| m.outcome.complete());
    Ok(UpgradeReport {
        distributed,
        members,
        complete,
    })
}

/// Reads and identifies the build to distribute: a regular file a member would
/// accept, read no further than members' limit.
fn read_build(source: PathBuf) -> Result<(Vec<u8>, Distributed)> {
    use std::io::Read;
    let unreadable = |detail: String| {
        Failure::new(
            "build_unreadable",
            format!(
                "The build at {} could not be read: {detail}",
                source.display()
            ),
            "Run elsecall install on the master, or pass --executable with a built elsecall.",
        )
    };
    let file = std::fs::File::open(&source).map_err(|e| unreadable(e.to_string()))?;
    let metadata = file.metadata().map_err(|e| unreadable(e.to_string()))?;
    if !metadata.is_file() {
        return Err(unreadable("it is not a regular file".into()));
    }
    let too_large = |size: u64| {
        Failure::new(
            "build_too_large",
            format!(
                "The build is {size} bytes; members accept 1 byte to {}.",
                protocol::MAXIMUM_EXECUTABLE
            ),
            "Distribute a release build.",
        )
    };
    if metadata.len() == 0 || metadata.len() > protocol::MAXIMUM_EXECUTABLE {
        return Err(too_large(metadata.len()));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(protocol::MAXIMUM_EXECUTABLE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| unreadable(e.to_string()))?;
    if bytes.len() as u64 > protocol::MAXIMUM_EXECUTABLE {
        return Err(too_large(bytes.len() as u64));
    }
    let architectures = executable_format::architectures(&bytes);
    if architectures.is_empty() {
        return Err(Failure::new(
            "wrong_platform",
            format!("{} is not a macOS executable.", source.display()),
            "Pass --executable with an elsecall build, or omit it to distribute the installed build.",
        ));
    }
    let digest = Digest::of(&bytes);
    let distributed = Distributed {
        build: digest.build_id(),
        digest,
        size: bytes.len() as u64,
        architectures,
        source,
    };
    Ok((bytes, distributed))
}

async fn upgrade_member(
    fleet: &Fleet,
    member: &Member,
    bytes: &[u8],
    distributed: &Distributed,
    wait_seconds: u64,
) -> MemberUpgrade {
    let done = |upgrade_id, outcome| MemberUpgrade {
        name: member.name.clone(),
        upgrade_id,
        outcome,
    };
    // The frozen upgrade status route, which every build with upgrades answers.
    let current = match status(fleet, member).await {
        Ok(status) => status.running,
        Err(error) if error.code == "not_found" => {
            return done(
                None,
                Outcome::Refused {
                    error: Failure::new(
                        "upgrade_unsupported",
                        format!("{} runs a build that predates upgrades.", member.name),
                        format!(
                            "Copy this build to {} and run its install command there once; later builds can be upgraded over the fleet.",
                            member.name
                        ),
                    ),
                },
            );
        }
        Err(error) if error.code == "member_unavailable" => {
            return done(None, Outcome::Unreachable { error });
        }
        Err(error) => return done(None, Outcome::Refused { error }),
    };
    if current.build == distributed.build {
        return done(None, Outcome::AlreadyCurrent { running: current });
    }
    let Some(id) = UpgradeId::generate() else {
        return done(
            None,
            Outcome::Interrupted {
                error: Failure::new(
                    "random_unavailable",
                    "The operating system's random number generator failed.",
                    "Retry; restart the machine if it keeps failing.",
                ),
            },
        );
    };
    let manifest = Manifest {
        id: id.clone(),
        recipient: member.key,
        digest: distributed.digest,
        size: distributed.size,
        os: Os::Macos,
        architectures: distributed.architectures.clone(),
        wait_seconds,
        issued_at: crate::peer_auth::now(),
    };
    let signed = manifest.sign(fleet.member_key());
    let from = match send(fleet, member, signed, bytes).await {
        Ok(Sent::Staged(from)) => from,
        Ok(Sent::AlreadyCurrent(running)) => {
            return done(None, Outcome::AlreadyCurrent { running });
        }
        // The member may have staged the build before its answer was lost, so
        // only its own record can say what happened.
        Ok(Sent::Unanswered) => current,
        Err(error) if error.code == "member_unavailable" => {
            return done(Some(id), Outcome::Unreachable { error });
        }
        Err(error) => return done(Some(id), Outcome::Refused { error }),
    };
    let outcome = follow(fleet, member, &id, from, wait_seconds).await;
    done(Some(id), outcome)
}

async fn status(fleet: &Fleet, member: &Member) -> Result<UpgradeStatus> {
    peer_client::call::<UpgradeStatus>(
        fleet.member_key(),
        member,
        Method::GET,
        protocol::STATUS_ROUTE,
        None::<&()>,
        peer_client::PEER_TIMEOUT,
    )
    .await
}

enum Sent {
    Staged(Build),
    AlreadyCurrent(Build),
    /// The content was sent, but no answer came back.
    Unanswered,
}

/// Offers the build and sends it; returns what the member ran when it staged it.
async fn send(
    fleet: &Fleet,
    member: &Member,
    manifest: protocol::SignedManifest,
    bytes: &[u8],
) -> Result<Sent> {
    let mut channel = peer_client::channel(
        fleet.member_key(),
        member,
        protocol::ROUTE,
        protocol::PURPOSE,
        protocol::PROTOCOL,
    )
    .await?;
    protocol::send(&mut channel, &Message::Offer { manifest }).await?;
    match reply(&mut channel).await? {
        Message::Accepted => (),
        Message::AlreadyCurrent { running } => return Ok(Sent::AlreadyCurrent(running)),
        Message::Refused { failure } => return Err(failure),
        _ => return Err(protocol::protocol_error()),
    }
    let sent = tokio::time::timeout(CONTENT_TIMEOUT, protocol::send_blob(&mut channel, bytes))
        .await
        .map_err(|_| protocol::lost())
        .and_then(|sent| sent);
    match sent.and(Ok(())) {
        Ok(()) => (),
        // Some or all of it may have arrived.
        Err(_) => return Ok(Sent::Unanswered),
    }
    match reply(&mut channel).await {
        Ok(Message::Staged { running }) => Ok(Sent::Staged(running)),
        Ok(Message::Refused { failure }) => Err(failure),
        Ok(_) => Err(protocol::protocol_error()),
        Err(_) => Ok(Sent::Unanswered),
    }
}

async fn reply(channel: &mut peer_client::RunChannel) -> Result<Message> {
    tokio::time::timeout(REPLY_TIMEOUT, protocol::receive(channel))
        .await
        .map_err(|_| protocol::lost())?
}

/// Follows the member's upgrade record, across its restart, until it ends.
async fn follow(
    fleet: &Fleet,
    member: &Member,
    id: &UpgradeId,
    from: Build,
    wait_seconds: u64,
) -> Outcome {
    let deadline = Instant::now() + Duration::from_secs(wait_seconds) + RESTART_ALLOWANCE;
    let mut last: Option<UpgradeState> = None;
    while Instant::now() < deadline {
        // While the member restarts it cannot answer; keep asking.
        let Ok(status) = status(fleet, member).await else {
            tokio::time::sleep(POLL).await;
            continue;
        };
        // A member that neither records this upgrade nor is still receiving or
        // checking it never staged this build.
        let receiving = status.active.as_ref() == Some(id);
        let Some(state) = status.latest.filter(|state| &state.id == id) else {
            if receiving {
                tokio::time::sleep(POLL).await;
                continue;
            }
            return Outcome::Refused {
                error: protocol::lost(),
            };
        };
        {
            if state.phase.terminal() {
                return match state.phase {
                    Phase::Upgraded { running } => Outcome::Upgraded { from, running },
                    Phase::RolledBack { running, error } => Outcome::RolledBack {
                        from,
                        running,
                        error,
                    },
                    Phase::RollbackFailed { error } => Outcome::RollbackFailed { from, error },
                    Phase::Deferred { in_flight } => Outcome::Deferred {
                        from,
                        in_flight,
                        hint: format!(
                            "Run elsecall fleet upgrade --member {} once its work ends, or pass a longer --wait.",
                            member.name
                        ),
                    },
                    Phase::Interrupted { error } => Outcome::Interrupted { error },
                    Phase::Draining { .. } | Phase::Installing => unreachable!("terminal"),
                };
            }
            last = Some(state);
        }
        tokio::time::sleep(POLL).await;
    }
    Outcome::Unknown {
        error: Failure::new(
            "upgrade_unconfirmed",
            format!(
                "{} did not report how the upgrade ended in time.",
                member.name
            ),
            format!(
                "Run elsecall fleet to see the build {} runs, and elsecall doctor there.",
                member.name
            ),
        ),
        last_state: last.map(|s| serde_json::to_value(s).expect("state serializes")),
    }
}
