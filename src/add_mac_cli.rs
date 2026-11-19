//! `elsecall fleet add`: adds a Mac from the master over SSH. It runs in the CLI's
//! own process, which macOS lets reach the local network, unlike the service.
use elsecall_add_mac::{
    add_mac::{self, Approval, Event, Master},
    authorized_keys,
    remote_mac::Build,
};
use elsecall_failure::{Failure, Result};
use elsecall_local_state::paths::Paths;
use elsecall_openssh_mac::OpensshMac;
use elsecall_platform_macos::macos::tailscale::TailscaleCli;
use elsecall_tailnet::tailnet::{Configured, Tailnet};
use serde_json::Value;
use std::{
    io::{BufRead, IsTerminal, Read, Write},
    time::Duration,
};
use tokio::signal::unix::{SignalKind, signal};
use zeroize::Zeroizing;

const PASSWORD_LIMIT: u64 = 4096;
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

pub async fn run(
    paths: &Paths,
    host: String,
    account: String,
    approval_wait: Duration,
    format: Progress,
) -> Result<(Value, bool)> {
    // Watched from the start, so stopping is graceful at any point.
    let mut interrupt =
        signal(SignalKind::interrupt()).map_err(|error| Failure::io("Watch for Ctrl-C", error))?;
    let mut terminate =
        signal(SignalKind::terminate()).map_err(|error| Failure::io("Watch for SIGTERM", error))?;
    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    let password = password(&mut stdin.lock(), terminal)?;
    // A program following the add keeps standard input open: when it closes,
    // whoever started the add is gone, and the add stops.
    let closed = (format == Progress::Json).then(watch_stdin);
    let master = master(paths, approval_wait)?;
    let progress = |event: Event| report_progress(format, &host, event);
    // Stopping drops the flow, and with it the SSH connection.
    let flow = async {
        // Refusals that need no Mac come before the password goes anywhere.
        let fleet = add_mac::preflight(&master.socket).await?;
        let askpass = std::env::current_exe()
            .map_err(|error| Failure::io("Find this executable for SSH", error))?;
        let mut mac = OpensshMac::connect(&host, &account, password, &askpass).await?;
        add_mac::add(&mut mac, &host, &master, &fleet, progress).await
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

/// The account's password: the first line of `input`, read no further, so a
/// program can keep the rest open. A terminal is refused, so the command never
/// waits for typing or echoes the password.
fn password(input: &mut impl BufRead, terminal: bool) -> Result<Zeroizing<String>> {
    let refused = || {
        Failure::new(
            "password_required",
            "The account's password must arrive on standard input.",
            "Pipe it in, for example from your password manager's command-line tool.",
        )
    };
    if terminal {
        return Err(refused());
    }
    let mut bytes = Zeroizing::new(Vec::new());
    input
        .take(PASSWORD_LIMIT)
        .read_until(b'\n', &mut bytes)
        .map_err(|error| Failure::io("Read the password", error))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| refused())?;
    let line = text.split('\n').next().unwrap_or_default();
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.is_empty() {
        return Err(refused());
    }
    Ok(Zeroizing::new(line.to_owned()))
}

/// The master's build, tailnet and SSH key.
fn master(paths: &Paths, approval_wait: Duration) -> Result<Master> {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_is_the_first_line_of_piped_input() {
        let read =
            |input: &[u8], terminal| password(&mut { input }, terminal).map(|p| p.to_string());
        assert_eq!(read(b"s3cret pass\n", false).unwrap(), "s3cret pass");
        assert_eq!(read(b"s3cret\r\nignored\n", false).unwrap(), "s3cret");
        assert_eq!(read(b"no newline", false).unwrap(), "no newline");
        for refused in [&b""[..], b"\n", b"\xff\xfe\n"] {
            assert_eq!(read(refused, false).unwrap_err().code, "password_required");
        }
        assert_eq!(
            read(b"typed\n", true).unwrap_err().code,
            "password_required"
        );
    }
}
