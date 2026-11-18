//! Adding a Mac from a real installed master. SSH is the boundary: a fake Mac
//! runs the real Elsecall CLI in its own temporary home on this machine, with
//! loopback standing in for the tailnet, as the other fleet tests do.
#![cfg(target_os = "macos")]
mod diagnostics;
mod members;
mod processes;

use elsecall::{
    add_mac::{self, Approval, Event, Master, Outcome, Step},
    failure::{Failure, Result},
    inbound_firewall::Rule,
    paths::Paths,
    remote_mac::{
        Build, Login, LoginLink, MacFacts, RemoteMac, RemoteTailnet, TailnetNode, TailscaleVariant,
    },
    tailscale_bottle::Bottle,
};
use members::{TestMember, free_port};
use serde_json::Value;
use std::{path::Path, process::Output, time::Duration};

const DOMAIN: &str = "example.ts.net";
const LINK: &str = "https://login.tailscale.com/a/example";
const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleMasterKey me@example.com\n";

/// The fake Mac's Tailscale.
#[derive(Debug, Clone, PartialEq)]
enum Tailscale {
    Online,
    Absent,
    /// Add Mac's daemon, logged out of `domain` (or never logged in). Once a
    /// login starts, the operator approves it after this many more reads of its
    /// state, or never.
    Waiting {
        approved_after: Option<u32>,
        domain: Option<String>,
    },
}

/// A Mac reached as if over SSH: commands run here, in its own home.
struct FakeMac {
    name: &'static str,
    home: tempfile::TempDir,
    port: u16,
    facts: MacFacts,
    domain: Option<String>,
    tailscale: Tailscale,
    /// After installing Tailscale, how many reads until the operator approves.
    approves_after: Option<u32>,
    /// The bottle Add Mac installed, and the name it logged in with.
    installed_tailscale: Option<Bottle>,
    logged_in_as: Option<Option<String>>,
    /// What the Mac's firewall does with incoming connections to Elsecall.
    firewall: Rule,
    opened: Vec<String>,
}

impl FakeMac {
    fn new(name: &'static str) -> Self {
        let home = tempfile::Builder::new()
            .prefix("ec-mac-")
            .tempdir_in("/tmp")
            .unwrap();
        Self {
            name,
            port: free_port(),
            facts: MacFacts {
                macos_version: "26.1".into(),
                architecture: "arm64".into(),
                model: Some("Mac16,10".into()),
                serial_number: Some("EXAMPLE0001".into()),
                account: "devbox".into(),
                home: home.path().to_string_lossy().into_owned(),
                administrator: true,
                local_host_name: Some(name.into()),
            },
            home,
            domain: Some(DOMAIN.into()),
            tailscale: Tailscale::Online,
            approves_after: Some(2),
            installed_tailscale: None,
            logged_in_as: None,
            firewall: Rule::NotAllowed,
            opened: Vec::new(),
        }
    }

    fn paths(&self) -> Paths {
        Paths::for_home(self.home.path()).unwrap()
    }

    /// Runs `program` as the Mac's account would, with the environment an installed
    /// test member's service and CLI share.
    fn command(&self, program: &Path, args: &[&str]) -> Output {
        processes::command(program)
            .env("HOME", self.home.path())
            .current_dir(self.home.path())
            .env("ELSECALL_SERVICE_MANAGER", "process")
            .env("ELSECALL_TAILNET_NAME", self.name)
            .env("ELSECALL_TAILNET_ADDRESSES", "127.0.0.1")
            .env("ELSECALL_FLEET_PORT", self.port.to_string())
            .env("ELSECALL_MACHINE_LOAD", "idle")
            .env("ELSECALL_MACHINE_SPEED", "1")
            .args(args)
            .output()
            .unwrap()
    }

    fn data(output: &Output) -> Result<Value> {
        let document: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("not JSON: {output:?}"));
        match document.get("error") {
            Some(error) => Err(serde_json::from_value::<Failure>(error.clone()).unwrap()),
            None => Ok(document["data"].clone()),
        }
    }

    fn authorized_keys(&self) -> String {
        std::fs::read_to_string(self.home.path().join(".ssh/authorized_keys")).unwrap_or_default()
    }
}

impl Drop for FakeMac {
    fn drop(&mut self) {
        let executable = self.paths().executable();
        if executable.is_file() {
            let _ = self.command(&executable, &["uninstall", "--force"]);
        }
    }
}

impl RemoteMac for FakeMac {
    fn host_key(&self) -> &str {
        "SHA256:exampleHostKeyFingerprint (ED25519)"
    }
    async fn facts(&mut self) -> Result<MacFacts> {
        Ok(self.facts.clone())
    }
    async fn sudo_accepted(&mut self) -> Result<bool> {
        Ok(true)
    }
    async fn tailnet(&mut self) -> Result<RemoteTailnet> {
        match self.tailscale.clone() {
            Tailscale::Online => (),
            Tailscale::Absent => return Ok(RemoteTailnet::Absent),
            Tailscale::Waiting {
                approved_after: Some(0),
                ..
            } if self.logged_in_as.is_some() => self.tailscale = Tailscale::Online,
            Tailscale::Waiting {
                approved_after,
                domain,
            } => {
                if self.logged_in_as.is_some() {
                    self.tailscale = Tailscale::Waiting {
                        approved_after: approved_after.map(|n| n - 1),
                        domain: domain.clone(),
                    };
                }
                return Ok(RemoteTailnet::LoggedOut {
                    variant: TailscaleVariant::AddMacDaemon,
                    domain,
                });
            }
        }
        Ok(RemoteTailnet::Online(TailnetNode {
            name: self.name.into(),
            domain: self.domain.clone(),
            addresses: vec!["127.0.0.1".parse().unwrap()],
            key_expiry: Some("2027-05-17T10:00:00Z".into()),
        }))
    }
    async fn install_tailscale(&mut self, bottle: &Bottle) -> Result<()> {
        assert_eq!(
            self.tailscale,
            Tailscale::Absent,
            "installed only when absent"
        );
        self.installed_tailscale = Some(*bottle);
        self.tailscale = Tailscale::Waiting {
            approved_after: self.approves_after,
            domain: None,
        };
        Ok(())
    }
    async fn start_tailscale_login(&mut self, hostname: Option<&str>) -> Result<Login> {
        if self.tailscale == Tailscale::Online {
            return Ok(Login::Online);
        }
        self.logged_in_as = Some(hostname.map(Into::into));
        Ok(Login::Approve(LoginLink::parse(LINK).unwrap()))
    }
    async fn installed_build(&mut self) -> Result<Option<String>> {
        let executable = self.paths().executable();
        if !executable.is_file() {
            return Ok(None);
        }
        Ok(Self::data(&self.command(&executable, &["status"]))
            .ok()
            .and_then(|status| status["build"].as_str().map(str::to_owned)))
    }
    async fn install(&mut self, build: &Build) -> Result<Value> {
        let staged = tempfile::tempdir().unwrap();
        let copy = staged.path().join("elsecall");
        std::fs::copy(&build.executable, &copy).unwrap();
        // A loaded test machine can start a debug build slower than install waits;
        // the other installed members retry the same way.
        let mut installed = Self::data(&self.command(&copy, &["install"]));
        for _ in 0..3 {
            match &installed {
                Err(failure) if failure.code == "startup_failed" => {
                    installed = Self::data(&self.command(&copy, &["install"]));
                }
                _ => break,
            }
        }
        installed
    }
    async fn elsecall(&mut self, args: &[&str]) -> Result<Value> {
        Self::data(&self.command(&self.paths().executable(), args))
    }
    async fn incoming(&mut self, _: &str) -> Result<Rule> {
        Ok(self.firewall)
    }
    async fn allow_incoming(&mut self, programs: &[String]) -> Result<()> {
        self.opened.extend(programs.iter().cloned());
        if self.firewall == Rule::NotAllowed {
            self.firewall = Rule::Allowed;
        }
        Ok(())
    }
    async fn authorize_key(&mut self, line: &str) -> Result<bool> {
        let existing = self.authorized_keys();
        let key = elsecall::authorized_keys::key_of(line).unwrap();
        if existing.contains(key) {
            return Ok(false);
        }
        std::fs::create_dir_all(self.home.path().join(".ssh")).unwrap();
        std::fs::write(
            self.home.path().join(".ssh/authorized_keys"),
            format!("{existing}{line}\n"),
        )
        .unwrap();
        Ok(true)
    }
}

fn master(alpha: &TestMember, ssh_public_key: Option<&str>) -> Master {
    let paths = Paths::for_home(alpha.home.path()).unwrap();
    Master {
        socket: paths.socket(),
        build: Build {
            executable: paths.executable(),
            window: None,
        },
        tailnet_domain: DOMAIN.into(),
        approval_wait: Duration::from_secs(60),
        ssh_public_key: ssh_public_key.map(Into::into),
    }
}

/// Adds `mac` as the CLI does: the fleet's checks first, then the Mac.
async fn add(
    mac: &mut FakeMac,
    master: &Master,
    progress: impl FnMut(Event),
) -> Result<add_mac::AddReport> {
    let fleet = add_mac::preflight(&master.socket).await?;
    let host = format!("{}.local", mac.name);
    add_mac::add(mac, &host, master, &fleet, progress).await
}

fn outcomes(report: &add_mac::AddReport) -> Vec<(Step, Outcome)> {
    report
        .steps
        .iter()
        .map(|s| (s.step, s.outcome.clone()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mac_on_the_tailnet_joins_awaiting_provisioning_and_again_is_repaired() {
    let alpha = TestMember::install("alpha", Path::new(env!("CARGO_BIN_EXE_elsecall")));
    let mut mac = FakeMac::new("bravo");
    let master = master(&alpha, Some(KEY));
    let mut heard = Vec::new();

    let report = add(&mut mac, &master, |event| heard.push(event))
        .await
        .unwrap();
    assert_eq!(report.member, "bravo");
    assert_eq!(
        outcomes(&report),
        [
            (Step::Check, Outcome::Done),
            (Step::Tailnet, Outcome::Already),
            (Step::Install, Outcome::Done),
            (Step::Firewall, Outcome::Done),
            (Step::SshKey, Outcome::Done),
            (Step::Enroll, Outcome::Done),
            (Step::Verify, Outcome::Done),
        ]
    );
    assert_eq!(heard.len(), 7, "a step each, and no approval: {heard:?}");
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["readiness"], "awaiting_provisioning");
    assert_eq!(json["mac"]["model"], "Mac16,10");
    assert_eq!(json["tailscale_key_expiry"], "2027-05-17T10:00:00Z");
    assert!(
        json["next_step"]
            .as_str()
            .unwrap()
            .contains("fleet ready bravo")
    );

    // The master reaches it as a member that takes no placed work.
    let entry = alpha.view_of("bravo");
    assert_eq!(entry["reachability"], "reachable", "{entry}");
    assert_eq!(entry["in_sync"], true);
    assert_eq!(entry["readiness"], "awaiting_provisioning");
    // Its firewall was opened for the installed executable, and the master's key
    // is accepted only from the tailnet.
    assert_eq!(
        mac.opened,
        [mac.paths().executable().to_string_lossy().into_owned()]
    );
    let keys = mac.authorized_keys();
    assert!(
        keys.starts_with("from=\"100.64.0.0/10,fd7a:115c:a1e0::/48\" ssh-ed25519 "),
        "{keys}"
    );
    assert!(keys.trim_end().ends_with("elsecall-master:alpha"), "{keys}");

    // Running it again changes nothing that is already right, and keeps the
    // member's readiness even after it was marked ready.
    alpha.ok(&["fleet", "ready", "bravo"]);
    let again = add(&mut mac, &master, |_| ()).await.unwrap();
    let steps = outcomes(&again);
    assert_eq!(steps[2], (Step::Install, Outcome::Already));
    assert_eq!(steps[3], (Step::Firewall, Outcome::Already));
    assert_eq!(steps[4], (Step::SshKey, Outcome::Already));
    assert_eq!(mac.opened.len(), 1, "allowed once");
    assert_eq!(serde_json::to_value(&again).unwrap()["readiness"], "ready");
    assert_eq!(alpha.view_of("bravo")["readiness"], "ready");
    assert_eq!(mac.authorized_keys().lines().count(), 1);

    // Leaving the fleet removes the master's key.
    let executable = mac.paths().executable();
    alpha.ok(&["fleet", "remove", "bravo"]);
    FakeMac::data(&mac.command(&executable, &["fleet", "leave"])).unwrap();
    assert_eq!(mac.authorized_keys(), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mac_that_cannot_join_is_refused_before_anything_changes() {
    let alpha = TestMember::install("alpha", Path::new(env!("CARGO_BIN_EXE_elsecall")));
    let master = master(&alpha, None);

    let mut other = FakeMac::new("bravo");
    other.domain = Some("example.beta.tailscale.net".into());
    let refused = add(&mut other, &master, |_| ()).await.unwrap_err();
    assert_eq!(refused.code, "tailnet_mismatch");

    let mut intel = FakeMac::new("charlie");
    intel.facts.architecture = "x86_64".into();
    let refused = add(&mut intel, &master, |_| ()).await.unwrap_err();
    assert_eq!(refused.code, "unsupported_mac");
    for mac in [&other, &intel] {
        assert!(!mac.paths().executable().exists(), "nothing was installed");
    }
    assert_eq!(alpha.fleet()["members"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_master_key_the_mac_still_joins() {
    let alpha = TestMember::install("alpha", Path::new(env!("CARGO_BIN_EXE_elsecall")));
    let mut mac = FakeMac::new("bravo");
    mac.firewall = Rule::Unfiltered;
    let report = add(&mut mac, &master(&alpha, None), |_| ()).await.unwrap();
    let steps = outcomes(&report);
    assert_eq!(steps[3], (Step::Firewall, Outcome::Already));
    assert!(matches!(steps[4], (Step::SshKey, Outcome::Skipped { .. })));
    assert!(mac.opened.is_empty());
    assert_eq!(alpha.view_of("bravo")["reachability"], "reachable");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_firewall_that_blocks_everything_is_refused_before_anything_changes() {
    let alpha = TestMember::install("alpha", Path::new(env!("CARGO_BIN_EXE_elsecall")));
    let mut mac = FakeMac::new("bravo");
    mac.firewall = Rule::AllBlocked;
    let refused = add(&mut mac, &master(&alpha, Some(KEY)), |_| ())
        .await
        .unwrap_err();
    assert_eq!(refused.code, "firewall_blocks_all");
    assert!(!mac.paths().executable().exists());
    assert_eq!(mac.authorized_keys(), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_member_stops_the_add_before_the_mac_is_reached() {
    let alpha = TestMember::install("alpha", Path::new(env!("CARGO_BIN_EXE_elsecall")));
    let mut charlie = TestMember::start("charlie");
    members::enrol(&alpha, &charlie);
    charlie.stop();
    let master = master(&alpha, Some(KEY));
    let refused = add_mac::preflight(&master.socket).await.err().unwrap();
    assert_eq!(refused.code, "member_unavailable");
    assert!(refused.message.contains("charlie"), "{}", refused.message);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mac_without_tailscale_gets_it_and_joins_once_approved() {
    let alpha = TestMember::install("alpha", Path::new(env!("CARGO_BIN_EXE_elsecall")));
    let mut mac = FakeMac::new("bravo");
    mac.tailscale = Tailscale::Absent;
    mac.facts.local_host_name = Some("Bravo".into());
    let mut heard = Vec::new();
    let report = add(&mut mac, &master(&alpha, Some(KEY)), |event| {
        heard.push(event)
    })
    .await
    .unwrap();
    assert_eq!(outcomes(&report)[1], (Step::Tailnet, Outcome::Done));
    // The bottle for its macOS, logged in under its Bonjour name.
    assert_eq!(mac.installed_tailscale.unwrap().macos, 26);
    assert_eq!(mac.logged_in_as, Some(Some("bravo".into())));
    assert!(heard.contains(&Event::Approve {
        link: LoginLink::parse(LINK).unwrap(),
        approval: Approval::Login,
    }));
    let approve = heard
        .iter()
        .position(|e| matches!(e, Event::Approve { .. }))
        .unwrap();
    assert_eq!(
        heard[approve - 1],
        Event::Step {
            step: Step::Tailnet
        }
    );
    assert_eq!(alpha.view_of("bravo")["reachability"], "reachable");
    // The firewall lets Add Mac's tailscaled through as well as Elsecall.
    assert_eq!(
        mac.opened,
        [
            mac.paths().executable().to_string_lossy().into_owned(),
            "/usr/local/bin/tailscaled".to_owned()
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_logged_out_daemon_logs_in_again_without_reinstalling() {
    let alpha = TestMember::install("alpha", Path::new(env!("CARGO_BIN_EXE_elsecall")));
    let mut mac = FakeMac::new("bravo");
    mac.tailscale = Tailscale::Waiting {
        approved_after: Some(1),
        domain: Some(DOMAIN.into()),
    };
    let report = add(&mut mac, &master(&alpha, None), |_| ()).await.unwrap();
    assert_eq!(outcomes(&report)[1], (Step::Tailnet, Outcome::Done));
    assert!(mac.installed_tailscale.is_none());
    // It logged in before, so it keeps its own name and settings.
    assert_eq!(mac.logged_in_as, Some(None));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_of_another_tailnet_is_refused_not_switched() {
    let alpha = TestMember::install("alpha", Path::new(env!("CARGO_BIN_EXE_elsecall")));
    let mut mac = FakeMac::new("bravo");
    mac.tailscale = Tailscale::Waiting {
        approved_after: Some(0),
        domain: Some("example.beta.tailscale.net".into()),
    };
    let refused = add(&mut mac, &master(&alpha, None), |_| ())
        .await
        .unwrap_err();
    assert_eq!(refused.code, "tailnet_mismatch");
    assert_eq!(mac.logged_in_as, None, "no login was started");
    assert!(!mac.paths().executable().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unapproved_mac_stops_before_elsecall_is_installed() {
    let alpha = TestMember::install("alpha", Path::new(env!("CARGO_BIN_EXE_elsecall")));
    let mut mac = FakeMac::new("bravo");
    mac.tailscale = Tailscale::Absent;
    mac.approves_after = None;
    let mut master = master(&alpha, Some(KEY));
    master.approval_wait = Duration::from_secs(3);
    let refused = add(&mut mac, &master, |_| ()).await.unwrap_err();
    assert_eq!(refused.code, "tailscale_login_pending");
    assert_eq!(
        refused.details.unwrap()["url"],
        "https://login.tailscale.com/a/example"
    );
    assert!(!mac.paths().executable().exists());

    // Approved later: running it again finds the Mac on the tailnet.
    mac.tailscale = Tailscale::Waiting {
        approved_after: Some(0),
        domain: None,
    };
    let report = add(&mut mac, &master, |_| ()).await.unwrap();
    assert_eq!(outcomes(&report)[1], (Step::Tailnet, Outcome::Already));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mac_on_macos_14_without_tailscale_is_told_to_install_it() {
    let alpha = TestMember::install("alpha", Path::new(env!("CARGO_BIN_EXE_elsecall")));
    let mut mac = FakeMac::new("bravo");
    mac.tailscale = Tailscale::Absent;
    mac.facts.macos_version = "14.7".into();
    let refused = add(&mut mac, &master(&alpha, None), |_| ())
        .await
        .unwrap_err();
    assert_eq!(refused.code, "tailscale_unavailable");
    assert!(mac.installed_tailscale.is_none());
}
