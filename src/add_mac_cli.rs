//! `elsecall fleet add`: adds a Mac from the master over SSH. It runs in the CLI's
//! own process, which macOS lets reach the local network, unlike the service.
mod configuration;

use elsecall_contract::failure::{Failure, Result};
use elsecall_fleet::{
    add_mac::{self, Approval, Event, Master, Step, StepReport},
    authorized_keys,
    remote_mac::{Build, RemoteMac},
};
use elsecall_host::paths::Paths;
use elsecall_macos::macos::tailscale::TailscaleCli;
use elsecall_macos::openssh_mac::OpensshMac;
use elsecall_peers::tailnet::{Configured, Tailnet};
use serde_json::Value;
use std::{
    io::{IsTerminal, Write},
    time::Duration,
};
use tokio::signal::unix::{SignalKind, signal};

/// The operator's default SSH keys, in the order OpenSSH offers them.
const SSH_KEYS: [&str; 3] = [
    ".ssh/id_ed25519.pub",
    ".ssh/id_ecdsa.pub",
    ".ssh/id_rsa.pub",
];

/// How `fleet add` reports progress on standard error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Progress {
    /// A line per step, and the approval link, which it also opens.
    Text,
    /// One `add_mac::Event` as JSON per line; the reader opens links.
    Json,
}

/// What the operator chose beside the Mac itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct Extras {
    /// Keep a MacBook awake with its lid closed, once it joined.
    pub keep_awake_with_lid_closed: bool,
    /// Install Elsecall's Xcode helper and its sudoers rule, once it joined.
    pub xcode_privileges: bool,
}

pub async fn run(
    paths: &Paths,
    host: String,
    account: String,
    approval_wait: Duration,
    format: Progress,
    mut ssh_host_key: Option<String>,
    extras: Extras,
) -> Result<(Value, bool)> {
    // Watched from the start, so stopping is graceful at any point.
    let mut interrupt =
        signal(SignalKind::interrupt()).map_err(|error| Failure::io("Watch for Ctrl-C", error))?;
    let mut terminate =
        signal(SignalKind::terminate()).map_err(|error| Failure::io("Watch for SIGTERM", error))?;
    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    let password = crate::piped_password::read(&mut stdin.lock(), terminal)?;
    // A program following the add keeps standard input open: when it closes,
    // whoever started the add is gone, and the add stops.
    let closed = (format == Progress::Json).then(watch_stdin);
    let master = master(paths, approval_wait, extras.keep_awake_with_lid_closed)?;
    let progress = |event: Event| report_progress(format, &host, event);
    // Stopping drops the flow, and with it the SSH connection.
    let flow = async {
        // Refusals that need no Mac come before the password goes anywhere.
        let fleet = add_mac::preflight(&master.socket).await?;
        let askpass = std::env::current_exe()
            .map_err(|error| Failure::io("Find this executable for SSH", error))?;
        let connected =
            OpensshMac::connect(&host, &account, password, &askpass, &mut ssh_host_key).await;
        if let Some(public_key) = ssh_host_key {
            progress(Event::HostKey { public_key });
        }
        let mut mac = connected?;
        let mut report = add_mac::add(&mut mac, &host, &master, &fleet, progress).await?;
        progress(Event::Step {
            step: Step::Configuration,
        });
        let outcome = configuration::synchronize(&master.socket, &report.member).await?;
        report.steps.push(StepReport {
            step: Step::Configuration,
            outcome,
        });
        if master.keep_awake_with_lid_closed {
            mac.keep_awake_with_lid_closed().await.map_err(|failure| {
                Failure::new(
                    "member_lid_closed_setup_failed",
                    format!("{} joined the fleet. {}", report.member, failure.message),
                    failure.hint,
                )
            })?;
        }
        // The person who typed the Mac's password is here now; the provisioning
        // loop copies Xcode there later without asking for it.
        if extras.xcode_privileges {
            progress(Event::Step {
                step: Step::Privileges,
            });
            let script = elsecall_tools::privileges_cli::install_script(&account)?;
            mac.install_xcode_privileges(&script)
                .await
                .map_err(|failure| {
                    let mut joined = Failure::new(
                        "member_xcode_privileges_failed",
                        format!("{} joined the fleet. {}", report.member, failure.message),
                        format!(
                            "Run elsecall tools privileges install on {}, with an administrator's password, to see why; adding the Mac again repeats this step.",
                            report.member
                        ),
                    );
                    if let Some(details) = failure.details {
                        joined = joined.with_details(details);
                    }
                    joined
                })?;
            report.steps.push(StepReport {
                step: Step::Privileges,
                outcome: add_mac::Outcome::Done,
            });
        }
        Ok(report)
    };
    let gone = async {
        match closed {
            Some(closed) => {
                let _ = closed.await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    let report = tokio::select! {
        report = flow => report?,
        _ = interrupt.recv() => return Err(stopped()),
        _ = terminate.recv() => return Err(stopped()),
        _ = gone => return Err(stopped()),
    };
    Ok((
        serde_json::to_value(report).expect("serializable report"),
        true,
    ))
}

/// Resolves when standard input reaches its end.
fn watch_stdin() -> tokio::sync::oneshot::Receiver<()> {
    let (closed, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
        let _ = closed.send(());
    });
    receiver
}

/// Writes progress on standard error; a reader that went away is not an error.
fn report_progress(format: Progress, host: &str, event: Event) {
    let mut stderr = std::io::stderr().lock();
    match (format, event) {
        (Progress::Json, event) => {
            let line = serde_json::to_string(&event).expect("serializable event");
            let _ = writeln!(stderr, "{line}");
        }
        (Progress::Text, Event::Step { step }) => {
            let _ = writeln!(stderr, "elsecall fleet add {host}: {}", step.doing());
        }
        (Progress::Text, Event::HostKey { .. }) => {}
        (Progress::Text, Event::FirewallApproval) => {
            let _ = writeln!(
                stderr,
                "elsecall fleet add {host}: click Allow on the incoming connections prompt on the new Mac. Add Mac continues when the firewall allows the connection."
            );
        }
        (Progress::Text, Event::Approve { link, approval }) => {
            let what = match approval {
                Approval::Login => "approve the Mac in Tailscale",
                Approval::Device => "approve the Mac as a device in the Tailscale admin console",
            };
            let _ = writeln!(
                stderr,
                "elsecall fleet add {host}: {what} at {} (opening it in your browser)",
                link.as_str()
            );
            // Only Tailscale's own links can be made, and opening one must not
            // hold up the flow.
            let _ = std::process::Command::new("/usr/bin/open")
                .arg(link.as_str())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}

fn stopped() -> Failure {
    Failure::new(
        "add_stopped",
        "Adding the Mac was stopped before it finished.",
        "Run elsecall fleet add again; it continues where it stopped.",
    )
}

/// The master's build, tailnet and SSH key.
fn master(
    paths: &Paths,
    approval_wait: Duration,
    keep_awake_with_lid_closed: bool,
) -> Result<Master> {
    let executable = paths.executable();
    if !executable.is_file() {
        return Err(Failure::new(
            "not_installed",
            "Elsecall is not installed on this Mac, so it has no build to copy.",
            "Install it with elsecall install, then add the Mac.",
        ));
    }
    let window = Some(paths.window_executable()).filter(|window| window.is_file());
    let presence = match Configured::from_env() {
        Some(configured) => configured?.discover()?,
        None => TailscaleCli.discover()?,
    };
    let tailnet_domain = presence.domain.ok_or_else(|| {
        Failure::new(
            "tailnet_domain_unknown",
            "Tailscale reports no tailnet domain for this Mac, so the new Mac's tailnet cannot be compared with it.",
            "Check tailscale status on this Mac, then add the Mac again.",
        )
    })?;
    // The first default key that is a single OpenSSH key line.
    let ssh_public_key = SSH_KEYS
        .iter()
        .filter_map(|key| std::fs::read_to_string(paths.home.join(key)).ok())
        .find(|key| authorized_keys::line(key, "check").is_ok());
    Ok(Master {
        socket: paths.socket(),
        build: Build { executable, window },
        tailnet_domain,
        ssh_public_key,
        approval_wait,
        keep_awake_with_lid_closed,
    })
}
