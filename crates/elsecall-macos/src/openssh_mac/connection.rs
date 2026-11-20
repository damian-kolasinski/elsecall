//! Authentication and host identity for the SSH connection used by Add Mac.

use super::{OpensshMac, SSH};
use elsecall_contract::failure::{Failure, Result};
use elsecall_fleet::askpass;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::process::Command;
use zeroize::Zeroizing;

const SSH_KEYGEN: &str = "/usr/bin/ssh-keygen";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Each session's private known_hosts file is used for a single target.
const HOST_KEY_ALIAS: &str = "elsecall-add-target";

impl OpensshMac {
    /// Connects to `host` as `account` and authenticates with `password`, which
    /// `askpass`, an Elsecall executable, hands to OpenSSH. If a host public key
    /// was observed on a previous attempt, SSH requires it before sending the
    /// password. The first observed key is recorded for further attempts.
    pub async fn connect(
        host: &str,
        account: &str,
        password: Zeroizing<String>,
        askpass: &Path,
        host_public_key: &mut Option<String>,
    ) -> Result<Self> {
        if !valid_name(host) || !valid_name(account) {
            return Err(Failure::new(
                "invalid_request",
                "The host and account must be plain names or addresses.",
                "Pass the host as elsecall fleet nearby lists it, and the account's short name.",
            ));
        }
        // Short, so the control socket's path stays under the macOS limit.
        let directory = tempfile::Builder::new()
            .prefix("ec-add-")
            .tempdir_in("/tmp")
            .map_err(|error| Failure::io("Prepare the SSH session", error))?;
        let known_hosts = directory.path().join("known_hosts");
        if let Some(key) = host_public_key {
            let key = public_key(key).ok_or_else(unreadable_key)?;
            std::fs::write(&known_hosts, format!("{HOST_KEY_ALIAS} {key}\n"))
                .map_err(|error| Failure::io("Pin the Mac's SSH host key", error))?;
        }
        let server = askpass::Server::start(&directory.path().join("p"), password.clone())?;
        let target = format!("{account}@{host}");
        let connect = || async {
            let strict = if known_hosts.is_file() {
                "StrictHostKeyChecking=yes"
            } else {
                "StrictHostKeyChecking=accept-new"
            };
            let output = Command::new(SSH)
                // The first value of an option wins, so these come first.
                .args(["-o", strict])
                .args(["-o", "BatchMode=no"])
                .args(options(directory.path()))
                .args([
                    "-o",
                    "ControlMaster=yes",
                    "-o",
                    // Outlasts the post-enrollment configuration sync, including an
                    // idle service restart, before the final power change.
                    "ControlPersist=180",
                    "-o",
                    "PreferredAuthentications=keyboard-interactive,password",
                    "-o",
                    "NumberOfPasswordPrompts=1",
                    "-o",
                    "ConnectTimeout=15",
                    "-N",
                    "-f",
                    &target,
                ])
                .env("SSH_ASKPASS", askpass)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env(askpass::SOCKET_VARIABLE, server.socket())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|error| Failure::io("Start ssh", error))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(connect_failure(
                    host,
                    account,
                    &String::from_utf8_lossy(&output.stderr),
                ))
            }
        };
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, retry_connect(connect)).await;
        drop(server);
        if host_public_key.is_none() {
            *host_public_key = std::fs::read_to_string(&known_hosts)
                .ok()
                .and_then(|known| {
                    let key = known
                        .trim_end()
                        .strip_prefix(HOST_KEY_ALIAS)?
                        .strip_prefix(' ')?;
                    public_key(key).map(str::to_owned)
                });
        }
        connected.map_err(|_| unreachable(host, "the connection timed out"))??;
        // Built first, so a failure from here on ends the connection.
        let mut mac = Self {
            target,
            directory,
            password,
            host_key: String::new(),
        };
        if host_public_key.is_none() {
            return Err(unreadable_key());
        }
        mac.host_key = fingerprint(mac.directory.path()).await?;
        Ok(mac)
    }
}

/// Bonjour and Remote Login may still be settling when the operator gets here.
async fn retry_connect(mut connect: impl AsyncFnMut() -> Result<()>) -> Result<()> {
    for wait in [500, 1000, 2000] {
        let result = connect().await;
        if !result
            .as_ref()
            .is_err_and(|failure| failure.code == "ssh_unreachable")
        {
            return result;
        }
        tokio::time::sleep(Duration::from_millis(wait)).await;
    }
    connect().await
}

/// Options every ssh invocation of the session shares. Options whose first value
/// differs between invocations are given before these.
pub(super) fn options(directory: &Path) -> Vec<String> {
    let path = |name: &str| directory.join(name).to_string_lossy().into_owned();
    [
        "-F".into(),
        "/dev/null".into(),
        "-o".into(),
        format!("ControlPath={}", path("c")),
        "-o".into(),
        format!("UserKnownHostsFile={}", path("known_hosts")),
        "-o".into(),
        format!("HostKeyAlias={HOST_KEY_ALIAS}"),
        "-o".into(),
        "UpdateHostKeys=no".into(),
        "-o".into(),
        "GlobalKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "IdentityAgent=none".into(),
        "-o".into(),
        "PubkeyAuthentication=no".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
    ]
    .into()
}

fn public_key(key: &str) -> Option<&str> {
    let (kind, blob) = key.split_once(' ')?;
    (!kind.is_empty()
        && kind
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'@' | b'.'))
        && !blob.is_empty()
        && blob
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')))
    .then_some(key)
}

fn unreadable_key() -> Failure {
    Failure::new(
        "host_key_unreadable",
        "The Mac's SSH host key could not be read.",
        "Go back to choose the Mac and sign in again.",
    )
}

/// The fingerprint of the host key the session trusted, such as
/// `SHA256:… (ED25519)`.
async fn fingerprint(directory: &Path) -> Result<String> {
    let output = Command::new(SSH_KEYGEN)
        .arg("-l")
        .arg("-f")
        .arg(directory.join("known_hosts"))
        .output()
        .await
        .map_err(|error| Failure::io("Read the Mac's host key", error))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = text
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    match fields.as_slice() {
        [_, hash, .., kind] if hash.starts_with("SHA256:") => Ok(format!("{hash} {kind}")),
        _ => Err(unreadable_key()),
    }
}

/// Host and account names go into the ssh command line: letters, digits and the
/// punctuation of host names, addresses and short account names only.
pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.len() <= 255
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'%'))
}

fn connect_failure(host: &str, account: &str, stderr: &str) -> Failure {
    if stderr.contains("Permission denied (")
        || stderr.contains("Permission denied, please try again")
    {
        return Failure::new(
            "ssh_password_refused",
            format!("{host} refused the password for {account}."),
            "Check the account's short name and password on that Mac, and that Remote Login allows that account.",
        );
    }
    if stderr.contains("Host key verification failed")
        || stderr.contains("REMOTE HOST IDENTIFICATION")
    {
        return Failure::new(
            "host_key_changed",
            format!("{host} presented a different SSH host key."),
            "Confirm the new Mac's address before choosing it again.",
        );
    }
    let why = if stderr.contains("Connection refused") {
        "it refused SSH connections"
    } else if stderr.contains("No route to host") {
        "there is no route to it"
    } else if stderr.contains("Could not resolve") || stderr.contains("nodename nor servname") {
        "its name did not resolve"
    } else if stderr.contains("Operation not permitted") || stderr.contains("Permission denied") {
        "macOS denied the network connection"
    } else {
        "the connection failed"
    };
    unreachable(host, why)
}

fn unreachable(host: &str, why: &str) -> Failure {
    Failure::new(
        "ssh_unreachable",
        format!("{host} could not be reached over SSH: {why}."),
        "Turn on Remote Login on that Mac (System Settings > General > Sharing), check it is on this network, and run the command from Terminal or over SSH if macOS Local Network privacy blocks this process.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_failures_name_their_cause() {
        let code = |stderr: &str| connect_failure("h", "a", stderr).code;
        assert_eq!(
            code("a@h: Permission denied (publickey,password)."),
            "ssh_password_refused"
        );
        assert_eq!(
            code("ssh: connect to host h port 22: Connection refused"),
            "ssh_unreachable"
        );
        assert_eq!(
            code("ssh: connect to host h port 22: Permission denied"),
            "ssh_unreachable"
        );
        assert_eq!(code("Host key verification failed."), "host_key_changed");
    }

    #[tokio::test(start_paused = true)]
    async fn connection_retries_unreachable_but_not_rejected_authentication_or_keys() {
        let mut tries = 0;
        retry_connect(async || {
            tries += 1;
            if tries < 3 {
                Err(unreachable("h", "the connection failed"))
            } else {
                Ok(())
            }
        })
        .await
        .unwrap();
        assert_eq!(tries, 3);
        for error in [
            "Permission denied (password).",
            "Host key verification failed",
        ] {
            let mut tries = 0;
            let result = retry_connect(async || {
                tries += 1;
                Err(connect_failure("h", "a", error))
            })
            .await;
            assert!(result.is_err());
            assert_eq!(tries, 1);
        }
    }

    #[test]
    fn a_pin_can_only_supply_one_key() {
        assert!(public_key("ssh-ed25519 AAa+/=").is_some());
        for key in [
            "",
            "ssh-ed25519 ",
            "ssh-ed25519 a b",
            "ssh-ed25519 a\n* ssh-ed25519 b",
        ] {
            assert!(public_key(key).is_none());
        }
    }
}
