//! Adding a Mac to the fleet from the master: checks it, puts Elsecall on it,
//! opens its firewall, authorizes the master's SSH key, enrolls it awaiting
//! provisioning and verifies it answers as a member. See docs/add-mac.md.
//!
//! Everything that can refuse runs before anything changes, and every step checks
//! before it acts, so running it again continues where an earlier run stopped,
//! and running it on a member repairs it.
use crate::{
    authorized_keys, client,
    failure::{Failure, Result},
    fleet_local_api::EnrollRequest,
    inbound_firewall::Rule,
    member_code,
    paths::Paths,
    remote_mac::{
        Build, Login, LoginLink, MacFacts, RemoteMac, RemoteTailnet, TailnetNode, TailscaleVariant,
    },
    roster::Readiness,
    tailscale_bottle,
};
use hyper::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The oldest macOS the master's build runs on.
const MINIMUM_MACOS: u32 = 14;
/// The architecture of the only build Elsecall ships.
const ARCHITECTURE: &str = "arm64";
const SERVICE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a joined Mac may take to answer the master as a member.
const VERIFY_WITHIN: Duration = Duration::from_secs(30);
/// How often the Mac's tailnet is read while the operator approves it.
const APPROVAL_POLL: Duration = Duration::from_secs(2);

/// A step of adding a Mac, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Check,
    Tailnet,
    Install,
    Firewall,
    SshKey,
    Enroll,
    Verify,
}

impl Step {
    /// What the step does, for progress lines.
    pub fn doing(self) -> &'static str {
        match self {
            Step::Check => "checking the Mac",
            Step::Tailnet => "putting it on the tailnet",
            Step::Install => "installing Elsecall",
            Step::Firewall => "opening the firewall for Elsecall",
            Step::SshKey => "authorizing the master's SSH key",
            Step::Enroll => "enrolling it",
            Step::Verify => "verifying it answers as a member",
        }
    }
}

/// What the operator hears while a Mac is added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A step starts.
    Step { step: Step },
    /// Tailscale on the Mac waits for the operator to approve it at `link`, in a
    /// browser signed in to the master's tailnet.
    Approve { link: LoginLink, approval: Approval },
}

/// What the operator approves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    /// The Mac's login to the tailnet.
    Login,
    /// The Mac as a device, on a tailnet whose admin approves new devices.
    Device,
}

/// The Mac on the tailnet, and whether Add Mac's own daemon runs it, so its
/// firewall should let that daemon through too.
struct Joined {
    node: TailnetNode,
    outcome: Outcome,
    daemon: Option<&'static str>,
}

/// Where Add Mac's `tailscaled` lives on the Mac.
const DAEMON: &str = "/usr/local/bin/tailscaled";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Done,
    /// Nothing needed changing.
    Already,
    Skipped {
        why: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StepReport {
    pub step: Step,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// What the operator may compare with About This Mac on the added Mac.
#[derive(Debug, Serialize, Deserialize)]
pub struct Identification {
    pub model: Option<String>,
    pub serial_number: Option<String>,
    pub macos_version: String,
    pub ssh_host_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AddReport {
    pub member: String,
    pub host: String,
    pub mac: Identification,
    pub readiness: Readiness,
    /// When the Mac's Tailscale node key expires; `null` when expiry is disabled.
    pub tailscale_key_expiry: Option<String>,
    pub steps: Vec<StepReport>,
    pub next_step: String,
}

/// The master's side of adding: its service, its build, its tailnet and its key.
pub struct Master {
    pub socket: PathBuf,
    pub build: Build,
    /// The master's tailnet DNS domain, which the Mac's must equal.
    pub tailnet_domain: String,
    /// The operator's public SSH key on the master, if there is one.
    pub ssh_public_key: Option<String>,
    /// How long to wait for the operator to approve the Mac in Tailscale.
    pub approval_wait: Duration,
}

/// The fleet as the master's service reports it, as far as adding needs it.
#[derive(Debug, Deserialize)]
struct FleetView {
    master: String,
    members: Vec<ViewedMember>,
}
#[derive(Debug, Deserialize)]
struct ViewedMember {
    name: String,
    key: String,
    this_member: bool,
    reachability: String,
    #[serde(default)]
    in_sync: bool,
    readiness: Readiness,
    version_skew: Option<bool>,
    #[serde(default)]
    error: Option<Failure>,
}

/// What the fleet must be before a Mac is touched: this member its master, every
/// other member reachable and on the master's build.
pub struct Preflight {
    master_name: String,
    members: Vec<ViewedMember>,
}

/// Checks the fleet. Runs before connecting to the Mac, so a refusal sends it
/// nothing, not even the password.
pub async fn preflight(socket: &Path) -> Result<Preflight> {
    let fleet = view(socket).await?;
    let own = require_master(&fleet)?;
    require_reachable(&fleet)?;
    require_one_build(&fleet)?;
    Ok(Preflight {
        master_name: own,
        members: fleet.members,
    })
}

/// Adds the Mac reached through `mac` as `host`. `progress` hears each step as it
/// starts.
pub async fn add<M: RemoteMac>(
    mac: &mut M,
    host: &str,
    master: &Master,
    fleet: &Preflight,
    mut progress: impl FnMut(Event),
) -> Result<AddReport> {
    let mut steps = Vec::new();
    let mut record = |step, outcome| steps.push(StepReport { step, outcome });

    // Everything that can refuse, before anything changes.
    progress(Event::Step { step: Step::Check });
    let facts = mac.facts().await?;
    check(&facts)?;
    if !mac.sudo_accepted().await? {
        return Err(Failure::new(
            "sudo_refused",
            format!(
                "sudo on {host} did not accept the password for {}.",
                facts.account
            ),
            "Use the password of an administrator account on that Mac.",
        ));
    }
    let executable = Paths::for_home(Path::new(&facts.home))?
        .executable()
        .to_string_lossy()
        .into_owned();
    let rule = mac.incoming(&executable).await?;
    if rule == Rule::AllBlocked {
        return Err(Failure::new(
            "firewall_blocks_all",
            format!(
                "The firewall on {host} blocks all incoming connections, so other members could never reach Elsecall there."
            ),
            "On that Mac, turn off \"Block all incoming connections\" in System Settings > Network > Firewall > Options, then add it again.",
        ));
    }
    let installed = mac.installed_build().await?;
    if installed.is_some() {
        let identity = mac.elsecall(&["fleet", "identity"]).await?;
        refuse_another_fleet(&identity, &fleet.members, host)?;
    }
    record(Step::Check, Outcome::Done);

    progress(Event::Step {
        step: Step::Tailnet,
    });
    let joined = join_tailnet(mac, &facts, master, host, &mut progress).await?;
    let node = joined.node;
    record(Step::Tailnet, joined.outcome);

    progress(Event::Step {
        step: Step::Install,
    });
    let wanted = crate::build_identity::Digest::of_file(&master.build.executable)
        .map_err(|error| Failure::io("Read the master's build", error))?
        .build_id()
        .to_string();
    let outcome = if installed.as_deref() == Some(wanted.as_str()) {
        Outcome::Already
    } else {
        mac.install(&master.build).await?;
        Outcome::Done
    };
    record(Step::Install, outcome);

    progress(Event::Step {
        step: Step::Firewall,
    });
    // Elsecall's listener, and Add Mac's tailscaled so peers can connect directly
    // rather than through Tailscale's relays.
    let programs: Vec<String> = std::iter::once(executable.as_str())
        .chain(joined.daemon)
        .map(str::to_owned)
        .collect();
    let mut blocked = Vec::new();
    for program in &programs {
        let program = program.as_str();
        let rule = if program == executable {
            rule
        } else {
            mac.incoming(program).await?
        };
        if !matches!(rule, Rule::Unfiltered | Rule::Allowed) {
            blocked.push(program.to_owned());
        }
    }
    let opened = !blocked.is_empty();
    if opened {
        mac.allow_incoming(&blocked).await?;
        for program in &blocked {
            if mac.incoming(program).await? != Rule::Allowed {
                return Err(Failure::new(
                    "firewall_unchanged",
                    format!("The firewall on {host} still does not allow {program}."),
                    "Allow it in System Settings > Network > Firewall > Options on that Mac, then add it again.",
                ));
            }
        }
    }
    // A console user who leaves the firewall's prompt unanswered and logs out
    // turns an allowed program's rule to Block; the guard allows it again.
    let guarded = mac.guard_incoming(&programs).await?;
    let outcome = if opened || guarded {
        Outcome::Done
    } else {
        Outcome::Already
    };
    record(Step::Firewall, outcome);

    progress(Event::Step { step: Step::SshKey });
    let outcome = match &master.ssh_public_key {
        None => Outcome::Skipped {
            why: "The master has no SSH key in ~/.ssh; create one with ssh-keygen -t ed25519 and run Add Mac again.".into(),
        },
        Some(key) => {
            let line = authorized_keys::line(key, &fleet.master_name)?;
            if mac.authorize_key(&line).await? {
                Outcome::Done
            } else {
                Outcome::Already
            }
        }
    };
    record(Step::SshKey, outcome);

    progress(Event::Step { step: Step::Enroll });
    let identity = mac.elsecall(&["fleet", "identity"]).await?;
    let code = text(&identity, "code")?.to_owned();
    // The name that is enrolled is the one in the code, which must be the Mac's
    // tailnet name.
    let member = member_code::decode(&code)?.name.to_string();
    if member != node.name {
        return Err(Failure::new(
            "unexpected_response",
            format!(
                "Elsecall on {host} calls itself {member}, but its tailnet name is {}.",
                node.name
            ),
            "Run elsecall doctor on that Mac, then add it again.",
        ));
    }
    let _: Value = client::send(
        &master.socket,
        Method::POST,
        "/v1/fleet/enroll",
        Some(
            serde_json::to_value(EnrollRequest {
                code,
                force: false,
                readiness: Readiness::AwaitingProvisioning,
            })
            .expect("serializable request"),
        ),
        SERVICE_TIMEOUT,
    )
    .await?;
    let sponsor: Value =
        client::get_within(&master.socket, "/v1/fleet/identity", SERVICE_TIMEOUT).await?;
    mac.elsecall(&["fleet", "join", text(&sponsor, "code")?])
        .await?;
    record(Step::Enroll, Outcome::Done);

    progress(Event::Step { step: Step::Verify });
    let readiness = verify(&master.socket, &member).await?;
    record(Step::Verify, Outcome::Done);

    Ok(AddReport {
        next_step: match readiness {
            Readiness::AwaitingProvisioning => format!(
                "{member} is a member awaiting provisioning: run work there with elsecall run --member {member}, and mark it ready with elsecall fleet ready {member} once its tools are installed."
            ),
            Readiness::Ready => format!("{member} is a member and takes placed work."),
        },
        member,
        host: host.to_owned(),
        mac: Identification {
            model: facts.model,
            serial_number: facts.serial_number,
            macos_version: facts.macos_version,
            ssh_host_key: mac.host_key().to_owned(),
        },
        readiness,
        tailscale_key_expiry: node.key_expiry,
        steps,
    })
}

async fn view(socket: &Path) -> Result<FleetView> {
    client::get_large_within(socket, "/v1/fleet", SERVICE_TIMEOUT).await
}

/// This member's name, when it is the master.
fn require_master(fleet: &FleetView) -> Result<String> {
    match fleet.members.iter().find(|m| m.this_member) {
        Some(own) if own.name == fleet.master => Ok(own.name.clone()),
        _ => Err(Failure::new(
            "not_master",
            "Only the master adds Macs: it copies its own build to them.",
            format!("Run elsecall fleet add on the master ({}).", fleet.master),
        )),
    }
}

/// Enrolling refuses while a member is unreachable, so this refuses first, before
/// the Mac changes.
fn require_reachable(fleet: &FleetView) -> Result<()> {
    let unreachable: Vec<&str> = fleet
        .members
        .iter()
        .filter(|m| !m.this_member && m.reachability != "reachable")
        .map(|m| m.name.as_str())
        .collect();
    if unreachable.is_empty() {
        return Ok(());
    }
    Err(Failure::new(
        "member_unavailable",
        format!(
            "These members cannot be reached, so the fleet cannot change: {}.",
            unreachable.join(", ")
        ),
        "Bring them back, or remove them with elsecall fleet remove, then add the Mac.",
    ))
}

/// A member on another build would drop the new member's readiness from any roster
/// it changes, so every member must run the master's build first.
fn require_one_build(fleet: &FleetView) -> Result<()> {
    let skewed: Vec<&str> = fleet
        .members
        .iter()
        .filter(|m| m.version_skew != Some(false) && !m.this_member)
        .map(|m| m.name.as_str())
        .collect();
    if skewed.is_empty() {
        return Ok(());
    }
    Err(Failure::new(
        "version_skew",
        format!(
            "These members do not run the master's build: {}.",
            skewed.join(", ")
        ),
        "Bring them to the master's build with elsecall fleet upgrade, then add the Mac.",
    ))
}

/// Refuses a Mac enrolled in a fleet that does not list it: joining would fail
/// after the master enrolled it.
fn refuse_another_fleet(identity: &Value, members: &[ViewedMember], host: &str) -> Result<()> {
    let enrolled = identity["enrolled"] == true;
    let key = identity["key"].as_str().unwrap_or_default();
    if enrolled && !members.iter().any(|m| m.key == key) {
        return Err(Failure::new(
            "member_of_another_fleet",
            format!("{host} already belongs to another fleet."),
            "Remove it from that fleet (elsecall fleet remove there, then elsecall fleet leave on it), then add it again.",
        ));
    }
    Ok(())
}

/// Refuses a Mac the master's build cannot run on, or an account that cannot
/// administer it.
fn check(facts: &MacFacts) -> Result<()> {
    if facts.architecture != ARCHITECTURE {
        return Err(Failure::new(
            "unsupported_mac",
            format!(
                "The Mac has a {} processor; Elsecall is built for Apple silicon only.",
                facts.architecture
            ),
            "Add a Mac with Apple silicon.",
        ));
    }
    if facts
        .macos_major()
        .is_none_or(|major| major < MINIMUM_MACOS)
    {
        return Err(Failure::new(
            "unsupported_mac",
            format!(
                "The Mac runs macOS {}; Elsecall needs macOS {MINIMUM_MACOS} or later.",
                facts.macos_version
            ),
            "Update macOS on that Mac, then add it again.",
        ));
    }
    if !facts.administrator {
        return Err(Failure::new(
            "not_administrator",
            format!("{} is not an administrator on that Mac.", facts.account),
            "Use an administrator account's name and password.",
        ));
    }
    Ok(())
}

/// Puts the Mac on the master's tailnet: as it is when it is there already,
/// otherwise installing Add Mac's daemon when it has none (or repairing it) and
/// logging it in, which the operator approves at the link `progress` hears. A Mac
/// that belongs to another tailnet is refused, never switched.
async fn join_tailnet<M: RemoteMac>(
    mac: &mut M,
    facts: &MacFacts,
    master: &Master,
    host: &str,
    progress: &mut impl FnMut(Event),
) -> Result<Joined> {
    let domain = master.tailnet_domain.as_str();
    let (fresh, daemon) = match mac.tailnet().await? {
        RemoteTailnet::Online { variant, node } => {
            return Ok(Joined {
                node: same_tailnet(node, domain, host)?,
                outcome: Outcome::Already,
                // A member Add Mac gave its daemon keeps it allowed, and guarded,
                // when Add Mac runs again.
                daemon: (variant == TailscaleVariant::AddMacDaemon).then_some(DAEMON),
            });
        }
        RemoteTailnet::LoggedOut {
            variant: TailscaleVariant::App,
            ..
        }
        | RemoteTailnet::NotRunning {
            variant: TailscaleVariant::App,
        } => {
            return Err(Failure::new(
                "tailscale_offline",
                format!("Tailscale on {host} is not connected to a tailnet."),
                "Open Tailscale on that Mac and log in to the master's tailnet, then add it again.",
            ));
        }
        RemoteTailnet::LoggedOut {
            domain: Some(other),
            ..
        } if other != domain => return Err(mismatch(host, Some(&other), domain)),
        RemoteTailnet::NotRunning {
            variant: TailscaleVariant::OtherDaemon,
        } => {
            return Err(Failure::new(
                "tailscale_stopped",
                format!("Tailscale's daemon on {host} is installed but not running."),
                "Start it on that Mac (for Homebrew's: sudo brew services start tailscale), then add it again.",
            ));
        }
        RemoteTailnet::Absent
        | RemoteTailnet::NotRunning {
            variant: TailscaleVariant::AddMacDaemon,
        } => {
            let bottle = facts
                .macos_major()
                .and_then(tailscale_bottle::for_macos)
                .ok_or_else(|| {
                    Failure::new(
                        "tailscale_unavailable",
                        format!(
                            "Add Mac has no Tailscale build for macOS {}.",
                            facts.macos_version
                        ),
                        "Install Tailscale on that Mac from tailscale.com and log in to the master's tailnet, then add it again.",
                    )
                })?;
            mac.install_tailscale(&bottle).await?;
            (true, Some(DAEMON))
        }
        RemoteTailnet::LoggedOut { variant, domain } => (
            domain.is_none(),
            (variant == TailscaleVariant::AddMacDaemon).then_some(DAEMON),
        ),
    };
    // Only a daemon that never logged in is named; another keeps its settings.
    let hostname = if fresh { tailnet_hostname(facts) } else { None };
    let (link, approval) = match mac.start_tailscale_login(hostname.as_deref()).await? {
        Login::Online => {
            return match mac.tailnet().await? {
                RemoteTailnet::Online { node, .. } => Ok(Joined {
                    node: same_tailnet(node, domain, host)?,
                    outcome: Outcome::Done,
                    daemon,
                }),
                _ => Err(Failure::new(
                    "tailscale_offline",
                    format!("Tailscale on {host} did not come online."),
                    "Check tailscale status on that Mac over SSH, then add it again.",
                )),
            };
        }
        Login::Approve(link) => (link, Approval::Login),
        Login::DeviceApproval => (LoginLink::device_approval(), Approval::Device),
    };
    progress(Event::Approve {
        link: link.clone(),
        approval,
    });
    let deadline = tokio::time::Instant::now() + master.approval_wait;
    loop {
        // A read that hangs ends at the deadline rather than after its own bound.
        if let Ok(read) = tokio::time::timeout_at(deadline, mac.tailnet()).await
            && let RemoteTailnet::Online { node, .. } = read?
        {
            return Ok(Joined {
                node: same_tailnet(node, domain, host)?,
                outcome: Outcome::Done,
                daemon,
            });
        }
        if tokio::time::Instant::now() >= deadline {
            let (what, hint) = match approval {
                Approval::Login => (
                    "its login",
                    "Open the link in a browser signed in to the master's tailnet, approve the Mac, then run elsecall fleet add again.",
                ),
                Approval::Device => (
                    "device approval",
                    "Approve the Mac in the Tailscale admin console, then run elsecall fleet add again.",
                ),
            };
            return Err(Failure::new(
                "tailscale_login_pending",
                format!("Tailscale on {host} waits for {what} at {}", link.as_str()),
                hint,
            )
            .with_details(serde_json::json!({ "url": link, "approval": approval })));
        }
        tokio::time::sleep(APPROVAL_POLL).await;
    }
}

/// The tailnet name to give the Mac: its Bonjour name, which is also how the
/// operator saw it on the local network. `None` leaves Tailscale's own choice.
fn tailnet_hostname(facts: &MacFacts) -> Option<String> {
    let name: String = facts
        .local_host_name
        .as_deref()?
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let name = name.trim_matches('-').to_owned();
    (!name.is_empty() && name.len() <= 63).then_some(name)
}

/// `node`, when it is on the master's tailnet.
fn same_tailnet(node: TailnetNode, domain: &str, host: &str) -> Result<TailnetNode> {
    if node.domain.as_deref() == Some(domain) {
        Ok(node)
    } else {
        Err(mismatch(host, node.domain.as_deref(), domain))
    }
}

fn mismatch(host: &str, theirs: Option<&str>, domain: &str) -> Failure {
    Failure::new(
        "tailnet_mismatch",
        format!(
            "{host} belongs to another tailnet ({}) than the master ({domain}).",
            theirs.unwrap_or("unknown")
        ),
        "Log that Mac out of its tailnet and into the master's, then add it again. Add Mac never switches a tailnet.",
    )
}

fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field].as_str().ok_or_else(|| {
        Failure::new(
            "unexpected_response",
            format!("The response had no {field}."),
            "Check that the Mac runs the master's Elsecall build.",
        )
    })
}

/// Waits until the master reaches `member` with the same roster, and returns its
/// readiness.
async fn verify(socket: &Path, member: &str) -> Result<Readiness> {
    let deadline = Instant::now() + VERIFY_WITHIN;
    loop {
        let fleet = view(socket).await?;
        let entry = fleet.members.into_iter().find(|m| m.name == member);
        if let Some(entry) = &entry
            && entry.reachability == "reachable"
            && entry.in_sync
        {
            return Ok(entry.readiness);
        }
        if Instant::now() > deadline {
            let why = entry.and_then(|e| e.error).map_or_else(
                || "it does not record the master's roster".into(),
                |e| e.message,
            );
            return Err(Failure::new(
                "member_unverified",
                format!("{member} joined, but the master cannot reach it as a member: {why}"),
                "Run elsecall doctor on that Mac (over SSH), then run Add Mac again.",
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> MacFacts {
        MacFacts {
            macos_version: "26.1".into(),
            architecture: "arm64".into(),
            model: Some("Mac16,10".into()),
            serial_number: None,
            account: "devbox".into(),
            home: "/Users/devbox".into(),
            administrator: true,
            local_host_name: Some("macbook-pro".into()),
        }
    }

    #[test]
    fn only_an_administrator_on_a_supported_mac_passes() {
        assert!(check(&facts()).is_ok());
        let intel = MacFacts {
            architecture: "x86_64".into(),
            ..facts()
        };
        assert_eq!(check(&intel).unwrap_err().code, "unsupported_mac");
        for old in ["13.6.1", "", "garbage"] {
            let old = MacFacts {
                macos_version: old.into(),
                ..facts()
            };
            assert_eq!(check(&old).unwrap_err().code, "unsupported_mac");
        }
        let user = MacFacts {
            administrator: false,
            ..facts()
        };
        assert_eq!(check(&user).unwrap_err().code, "not_administrator");
    }

    fn node(domain: Option<&str>) -> TailnetNode {
        TailnetNode {
            name: "macbook-pro".into(),
            domain: domain.map(Into::into),
            addresses: vec!["100.64.0.20".parse().unwrap()],
            key_expiry: None,
        }
    }

    #[test]
    fn only_the_masters_tailnet_is_accepted() {
        let master = "example.ts.net";
        assert!(same_tailnet(node(Some(master)), master, "h").is_ok());
        for other in [Some("example.beta.tailscale.net"), None] {
            assert_eq!(
                same_tailnet(node(other), master, "h").unwrap_err().code,
                "tailnet_mismatch"
            );
        }
    }

    #[test]
    fn progress_events_are_the_json_lines_the_window_reads() {
        let step = Event::Step {
            step: Step::Tailnet,
        };
        assert_eq!(
            serde_json::to_string(&step).unwrap(),
            r#"{"event":"step","step":"tailnet"}"#
        );
        let approve = Event::Approve {
            link: LoginLink::parse("https://login.tailscale.com/a/example").unwrap(),
            approval: Approval::Login,
        };
        let line = serde_json::to_string(&approve).unwrap();
        assert_eq!(
            line,
            r#"{"event":"approve","link":"https://login.tailscale.com/a/example","approval":"login"}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), approve);
        // A link that is not Tailscale's is refused when read, too.
        assert!(
            serde_json::from_str::<Event>(
                r#"{"event":"approve","link":"https://example.com/x","approval":"login"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn the_tailnet_name_is_the_bonjour_name_as_a_dns_label() {
        let named = |name: Option<&str>| {
            tailnet_hostname(&MacFacts {
                local_host_name: name.map(Into::into),
                ..facts()
            })
        };
        assert_eq!(named(Some("MacBook-Pro")).as_deref(), Some("macbook-pro"));
        assert_eq!(
            named(Some("Studio_2 (old)")).as_deref(),
            Some("studio-2--old")
        );
        assert_eq!(named(Some("---")), None);
        assert_eq!(named(None), None);
    }

    fn fleet(members: serde_json::Value) -> FleetView {
        serde_json::from_value(serde_json::json!({"master": "mac-mini", "members": members}))
            .unwrap()
    }

    fn member(name: &str, own: bool, reachability: &str, skew: Option<bool>) -> serde_json::Value {
        serde_json::json!({
            "name": name, "key": format!("key-{name}"), "this_member": own,
            "reachability": reachability, "readiness": "ready", "version_skew": skew,
        })
    }

    #[test]
    fn the_master_alone_adds_to_a_reachable_fleet_on_one_build() {
        let good = fleet(serde_json::json!([
            member("mac-mini", true, "reachable", Some(false)),
            member("macbook-pro", false, "reachable", Some(false)),
        ]));
        assert_eq!(require_master(&good).unwrap(), "mac-mini");
        assert!(require_reachable(&good).is_ok());
        assert!(require_one_build(&good).is_ok());

        let skewed = fleet(serde_json::json!([
            member("mac-mini", true, "reachable", Some(false)),
            member("macbook-pro", false, "reachable", Some(true)),
        ]));
        assert_eq!(require_one_build(&skewed).unwrap_err().code, "version_skew");
        let down = fleet(serde_json::json!([
            member("mac-mini", true, "reachable", Some(false)),
            member("macbook-pro", false, "unreachable", None),
        ]));
        assert_eq!(
            require_reachable(&down).unwrap_err().code,
            "member_unavailable"
        );
        let elsewhere = fleet(serde_json::json!([member(
            "macbook-pro",
            true,
            "reachable",
            Some(false)
        ),]));
        assert_eq!(require_master(&elsewhere).unwrap_err().code, "not_master");
    }

    #[test]
    fn a_mac_in_another_fleet_is_refused_and_a_member_is_repaired() {
        let members = fleet(serde_json::json!([member(
            "mac-mini",
            true,
            "reachable",
            Some(false)
        )]))
        .members;
        let solo = serde_json::json!({"enrolled": false, "key": "key-new"});
        assert!(refuse_another_fleet(&solo, &members, "h").is_ok());
        let ours = serde_json::json!({"enrolled": true, "key": "key-mac-mini"});
        assert!(refuse_another_fleet(&ours, &members, "h").is_ok());
        let theirs = serde_json::json!({"enrolled": true, "key": "key-other"});
        assert_eq!(
            refuse_another_fleet(&theirs, &members, "h")
                .unwrap_err()
                .code,
            "member_of_another_fleet"
        );
    }
}
