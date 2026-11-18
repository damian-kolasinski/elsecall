//! The Mac being added, reached with the system's OpenSSH: the adapter of
//! `remote_mac::RemoteMac`.
//!
//! One authenticated connection carries every command (`ControlMaster`). It ignores
//! the operator's SSH configuration, agent and keys, trusts the host key it first
//! sees for the rest of the session, and gets the password from `askpass` once:
//! later commands never prompt, so a lost connection fails instead of asking.
//! Commands run under `/bin/sh` on the Mac; sudo reads the password on standard
//! input.
use crate::{
    askpass,
    failure::{Failure, Result},
    inbound_firewall::Rule,
    macos::application_firewall,
    remote_mac::{Build, MacFacts, RemoteMac, RemoteTailnet, TailscaleVariant},
};
use serde_json::Value;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use zeroize::Zeroizing;

const SSH: &str = "/usr/bin/ssh";
const SSH_KEYGEN: &str = "/usr/bin/ssh-keygen";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// A command that runs longer than this, such as an install, has failed.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
/// The most a command may print; Elsecall's own documents are far smaller.
const OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;
/// Where `install` puts the executable, relative to the account's home.
const INSTALLED: &str = "$HOME/Library/Application Support/Elsecall/elsecall";
/// Printed before each command's own output, so whatever the account's login
/// shell prints on start is left out.
const MARKER: &str = "--elsecall-output--";

pub struct OpensshMac {
    target: String,
    /// Private to this user; holds the control socket, the trusted host key and
    /// the askpass socket, and is removed when the session ends.
    directory: tempfile::TempDir,
    password: Zeroizing<String>,
    host_key: String,
}

/// A command's result on the Mac: its exit status, its own output and the end of
/// its error output.
struct Ran {
    success: bool,
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

impl OpensshMac {
    /// Connects to `host` as `account` and authenticates with `password`, which
    /// `askpass`, an Elsecall executable, hands to OpenSSH.
    pub async fn connect(
        host: &str,
        account: &str,
        password: Zeroizing<String>,
        askpass: &Path,
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
        let server = askpass::Server::start(&directory.path().join("p"), password.clone())?;
        let target = format!("{account}@{host}");
        let mut command = Command::new(SSH);
        command
            // The first value of an option wins, so these come first.
            .args(["-o", "StrictHostKeyChecking=accept-new"])
            .args(["-o", "BatchMode=no"])
            .args(options(directory.path()))
            .args([
                "-o",
                "ControlMaster=yes",
                "-o",
                // Ends the connection a minute after its last command, should this
                // process die without ending it.
                "ControlPersist=60",
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
            .kill_on_drop(true);
        let output = tokio::time::timeout(CONNECT_TIMEOUT, command.output())
            .await
            .map_err(|_| unreachable(host, "the connection timed out"))?
            .map_err(|error| Failure::io("Start ssh", error))?;
        drop(server);
        if !output.status.success() {
            return Err(connect_failure(
                host,
                account,
                &String::from_utf8_lossy(&output.stderr),
            ));
        }
        // Built first, so a failure from here on ends the connection.
        let mut mac = Self {
            target,
            directory,
            password,
            host_key: String::new(),
        };
        mac.host_key = fingerprint(mac.directory.path()).await?;
        Ok(mac)
    }

    /// Runs `script` with `/bin/sh` on the Mac, with `input` on its standard input.
    async fn run(&self, script: &str, input: Zeroizing<Vec<u8>>) -> Result<Ran> {
        let wrapped = format!("printf '%s\\n' {MARKER}; {script}");
        let mut child = Command::new(SSH)
            .args(["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes"])
            .args(options(self.directory.path()))
            .args(["-o", "ControlMaster=no", "-T", &self.target, "--"])
            .arg(format!("/bin/sh -c {}", quote(&wrapped)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| Failure::io("Start ssh", error))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let writer = tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
            let _ = stdin.shutdown().await;
        });
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let collected = async {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let mut limited_out = (&mut stdout).take(OUTPUT_LIMIT);
            let mut limited_err = (&mut stderr).take(64 * 1024);
            let (read_out, read_err) = tokio::join!(
                limited_out.read_to_end(&mut out),
                limited_err.read_to_end(&mut err),
            );
            read_out.and(read_err)?;
            let status = child.wait().await?;
            std::io::Result::Ok((status, out, err))
        };
        let (status, stdout, stderr) = tokio::time::timeout(COMMAND_TIMEOUT, collected)
            .await
            .map_err(|_| {
                Failure::new(
                    "remote_command_timeout",
                    "A command on the Mac did not finish in time.",
                    "Check the Mac, then run Add Mac again; it continues where it stopped.",
                )
            })?
            .map_err(|error| Failure::io("Run a command over ssh", error))?;
        let _ = writer.await;
        if status.code() == Some(255) {
            return Err(Failure::new(
                "ssh_disconnected",
                "The SSH connection to the Mac was lost.",
                "Check that the Mac is awake and on the network, then run Add Mac again.",
            ));
        }
        Ok(Ran {
            success: status.success(),
            code: status.code(),
            stdout: after_marker(&stdout).to_vec(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }

    /// Runs `script` as root through sudo, which reads the password first; the
    /// script itself gets no standard input.
    async fn sudo(&self, script: &str) -> Result<Ran> {
        let mut input = Zeroizing::new(self.password.as_bytes().to_vec());
        input.push(b'\n');
        let script = format!("exec </dev/null; {script}");
        self.run(
            &format!("sudo -S -k -p '' -- /bin/sh -c {}", quote(&script)),
            input,
        )
        .await
    }

    /// Runs `script` and requires it to succeed; `what` names it in a failure.
    async fn checked(&self, script: &str, input: &[u8], what: &str) -> Result<Vec<u8>> {
        let ran = self.run(script, Zeroizing::new(input.to_vec())).await?;
        if !ran.success {
            return Err(remote_failure(what, &ran));
        }
        Ok(ran.stdout)
    }
}

impl Drop for OpensshMac {
    fn drop(&mut self) {
        // Ends the shared connection; the directory goes with `self.directory`.
        let _ = std::process::Command::new(SSH)
            .args(["-o", "BatchMode=yes"])
            .args(options(self.directory.path()))
            .args(["-O", "exit", &self.target])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl RemoteMac for OpensshMac {
    fn host_key(&self) -> &str {
        &self.host_key
    }

    async fn facts(&mut self) -> Result<MacFacts> {
        let output = self.checked(FACTS, b"", "Reading the Mac's facts").await?;
        parse_facts(&String::from_utf8_lossy(&output))
    }

    async fn sudo_accepted(&mut self) -> Result<bool> {
        let ran = self.sudo("true").await?;
        if ran.success {
            return Ok(true);
        }
        if ran.stderr.contains("incorrect password") || ran.stderr.contains("Sorry, try again") {
            return Ok(false);
        }
        if ran.stderr.contains("not in the sudoers") || ran.stderr.contains("may not run sudo") {
            return Err(Failure::new(
                "not_administrator",
                "That account may not use sudo on the Mac.",
                "Use an administrator account's name and password.",
            ));
        }
        Err(remote_failure("Checking sudo on the Mac", &ran))
    }

    async fn tailnet(&mut self) -> Result<RemoteTailnet> {
        let ran = self.run(TAILNET, Zeroizing::default()).await?;
        Ok(parse_tailnet(&ran.stdout))
    }

    async fn installed_build(&mut self) -> Result<Option<String>> {
        let ran = self
            .run(&format!("\"{INSTALLED}\" status"), Zeroizing::default())
            .await?;
        Ok(serde_json::from_slice::<Value>(&ran.stdout)
            .ok()
            .and_then(|status| status["data"]["build"].as_str().map(str::to_owned)))
    }

    async fn install(&mut self, build: &Build) -> Result<Value> {
        let staged = self
            .checked(
                "umask 077 && mktemp -d",
                b"",
                "Preparing a directory on the Mac",
            )
            .await?;
        let staged = String::from_utf8_lossy(&staged).trim().to_owned();
        if !staged.starts_with('/') || staged.contains('\n') {
            return Err(Failure::new(
                "remote_command_failed",
                "The Mac did not create a directory for the build.",
                "Check the Mac's free space, then run Add Mac again.",
            ));
        }
        let mut files = vec![(&build.executable, "elsecall")];
        if let Some(window) = &build.window {
            files.push((window, "elsecall-ui"));
        }
        for (source, name) in files {
            let bytes = std::fs::read(source)
                .map_err(|error| Failure::io("Read the master's build", error))?;
            let destination = format!("{staged}/{name}");
            self.checked(
                &format!("cat > {0} && chmod 700 {0}", quote(&destination)),
                &bytes,
                "Copying the build to the Mac",
            )
            .await?;
        }
        let ran = self
            .run(
                &format!(
                    "{0}/elsecall install; status=$?; rm -rf {0}; exit $status",
                    quote(&staged)
                ),
                Zeroizing::default(),
            )
            .await?;
        envelope(&ran, "Installing Elsecall on the Mac")
    }

    async fn elsecall(&mut self, args: &[&str]) -> Result<Value> {
        let arguments: Vec<String> = args.iter().map(|a| quote(a)).collect();
        let ran = self
            .run(
                &format!("\"{INSTALLED}\" {}", arguments.join(" ")),
                Zeroizing::default(),
            )
            .await?;
        envelope(&ran, "Running Elsecall on the Mac")
    }

    async fn incoming(&mut self, program: &str) -> Result<Rule> {
        // Each query's output follows a separator line of its own.
        let script: Vec<String> = application_firewall::QUERIES
            .iter()
            .map(|query| format!("printf '%s\\n' {MARKER}; {query} 2>/dev/null"))
            .collect();
        let ran = self.run(&script.join("; "), Zeroizing::default()).await?;
        let text = String::from_utf8_lossy(&ran.stdout).into_owned();
        let parts: Vec<&str> = text.split(&format!("{MARKER}\n")).skip(1).collect();
        Ok(match parts.as_slice() {
            [state, block_all, apps] if !state.trim().is_empty() => {
                application_firewall::rule(state, block_all, apps, program)
            }
            _ => Rule::Unknown,
        })
    }

    async fn allow_incoming(&mut self, programs: &[String]) -> Result<()> {
        let tool = "/usr/libexec/ApplicationFirewall/socketfilterfw";
        let script: Vec<String> = programs
            .iter()
            .map(|program| {
                format!(
                    "{tool} --add {0} >/dev/null && {tool} --unblockapp {0} >/dev/null",
                    quote(program)
                )
            })
            .collect();
        let ran = self.sudo(&script.join(" && ")).await?;
        if !ran.success {
            return Err(remote_failure("Opening the firewall on the Mac", &ran));
        }
        Ok(())
    }

    async fn authorize_key(&mut self, line: &str) -> Result<bool> {
        let output = self
            .checked(
                &authorize_script(line)?,
                b"",
                "Authorizing the master's SSH key",
            )
            .await?;
        Ok(String::from_utf8_lossy(&output).trim() == "added")
    }
}

/// Adds `line` to `$HOME/.ssh/authorized_keys` unless its key is there, printing
/// `added` or `present`.
fn authorize_script(line: &str) -> Result<String> {
    let key = crate::authorized_keys::key_of(line).ok_or_else(|| {
        Failure::new(
            "ssh_key_unreadable",
            "The master's SSH key line has no key.",
            "Check ~/.ssh/id_ed25519.pub on the master.",
        )
    })?;
    Ok(format!(
        r#"umask 077 && mkdir -p "$HOME/.ssh" && f="$HOME/.ssh/authorized_keys" && touch "$f" &&
if grep -qF -- {key} "$f"; then echo present; else
  if [ -s "$f" ] && [ -n "$(tail -c 1 "$f")" ]; then echo >> "$f"; fi
  printf '%s\n' {line} >> "$f" && echo added; fi"#,
        key = quote(key),
        line = quote(line),
    ))
}

/// Options every ssh invocation of the session shares. Options whose first value
/// differs between invocations are given before these.
fn options(directory: &Path) -> Vec<String> {
    let path = |name: &str| directory.join(name).to_string_lossy().into_owned();
    [
        "-F".into(),
        "/dev/null".into(),
        "-o".into(),
        format!("ControlPath={}", path("c")),
        "-o".into(),
        format!("UserKnownHostsFile={}", path("known_hosts")),
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
        _ => Err(Failure::new(
            "host_key_unreadable",
            "The Mac's SSH host key could not be read.",
            "Run Add Mac again.",
        )),
    }
}

/// Host and account names go into the ssh command line: letters, digits and the
/// punctuation of host names, addresses and short account names only.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.len() <= 255
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'%'))
}

/// `text` as one single-quoted shell word.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// What follows the first marker line; everything when there is none.
fn after_marker(output: &[u8]) -> &[u8] {
    let marker = format!("{MARKER}\n");
    output
        .windows(marker.len())
        .position(|window| window == marker.as_bytes())
        .map_or(output, |at| &output[at + marker.len()..])
}

fn connect_failure(host: &str, account: &str, stderr: &str) -> Failure {
    if stderr.contains("Permission denied") {
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
            format!("{host} presented another host key than earlier in this session."),
            "Make sure the host is the Mac you mean, then run Add Mac again.",
        );
    }
    let why = if stderr.contains("Connection refused") {
        "it refused SSH connections"
    } else if stderr.contains("No route to host") {
        "there is no route to it"
    } else if stderr.contains("Could not resolve") || stderr.contains("nodename nor servname") {
        "its name did not resolve"
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

/// A failed remote command, with the end of its error output, which never holds
/// the password: sudo reads it but does not print it.
fn remote_failure(what: &str, ran: &Ran) -> Failure {
    let tail: String = {
        let trimmed = ran.stderr.trim();
        let start = trimmed
            .char_indices()
            .rev()
            .nth(299)
            .map_or(0, |(at, _)| at);
        trimmed[start..].to_owned()
    };
    Failure::new(
        "remote_command_failed",
        format!("{what} failed (exit {:?}): {tail}", ran.code),
        "Fix what the message says on that Mac, then run Add Mac again; it continues where it stopped.",
    )
}

/// An Elsecall JSON document from the Mac: its data, or its error, which is
/// reported as the Mac's rather than taken as the master's own.
fn envelope(ran: &Ran, what: &str) -> Result<Value> {
    let Ok(document) = serde_json::from_slice::<Value>(&ran.stdout) else {
        return Err(remote_failure(what, ran));
    };
    if let Some(error) = document.get("error") {
        let code = error["code"].as_str().unwrap_or("unknown");
        let message = error["message"].as_str().unwrap_or_default();
        return Err(Failure::new(
            "remote_elsecall_failed",
            format!("{what} failed with {code}: {message}"),
            "Run elsecall doctor on that Mac over SSH, fix what it reports, then run Add Mac again.",
        )
        .with_details(serde_json::json!({ "remote_error": error })));
    }
    Ok(document["data"].clone())
}

const FACTS: &str = r#"printf 'version=%s\n' "$(sw_vers -productVersion)"
printf 'architecture=%s\n' "$(uname -m)"
printf 'model=%s\n' "$(sysctl -n hw.model 2>/dev/null)"
printf 'serial=%s\n' "$(ioreg -c IOPlatformExpertDevice -d 2 | awk -F'"' '/IOPlatformSerialNumber/{print $4}')"
printf 'account=%s\n' "$(id -un)"
printf 'home=%s\n' "$HOME"
if id -Gn | tr ' ' '\n' | grep -qx admin; then echo administrator=yes; else echo administrator=no; fi
printf 'local_host_name=%s\n' "$(scutil --get LocalHostName 2>/dev/null)""#;

fn parse_facts(output: &str) -> Result<MacFacts> {
    let field = |key: &str| {
        output
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let missing = || {
        Failure::new(
            "unexpected_response",
            "The Mac did not report its macOS version, architecture and account.",
            "Check that the host is a Mac, then run Add Mac again.",
        )
    };
    Ok(MacFacts {
        macos_version: field("version").ok_or_else(missing)?,
        architecture: field("architecture").ok_or_else(missing)?,
        model: field("model"),
        serial_number: field("serial"),
        account: field("account").ok_or_else(missing)?,
        home: field("home").ok_or_else(missing)?,
        administrator: field("administrator").as_deref() == Some("yes"),
        local_host_name: field("local_host_name"),
    })
}

/// Prints `variant=<path>` for the first Tailscale command found, then its status;
/// `variant=none` when there is none.
const TAILNET: &str = r#"for t in /Applications/Tailscale.app/Contents/MacOS/Tailscale /usr/local/bin/tailscale /opt/homebrew/bin/tailscale; do
  if [ -x "$t" ]; then echo "variant=$t"; TAILSCALE_BE_CLI=1 "$t" status --json --peers=false 2>/dev/null; exit 0; fi
done
echo variant=none"#;

fn parse_tailnet(output: &[u8]) -> RemoteTailnet {
    let text = String::from_utf8_lossy(output);
    let (first, status) = text.split_once('\n').unwrap_or((&text, ""));
    let variant = match first.strip_prefix("variant=") {
        None | Some("none") => return RemoteTailnet::Absent,
        Some(path) if path.contains("Tailscale.app") => TailscaleVariant::App,
        Some(_) => TailscaleVariant::Daemon,
    };
    match crate::macos::tailscale::online_node(status.as_bytes()) {
        Some(node) => RemoteTailnet::Online(node),
        None => RemoteTailnet::Offline { variant },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_are_read_from_their_lines() {
        let facts = parse_facts(
            "version=26.1\narchitecture=arm64\nmodel=Mac16,10\nserial=\naccount=devbox\nhome=/Users/devbox\nadministrator=yes\nlocal_host_name=macbook-pro\n",
        )
        .unwrap();
        assert_eq!(facts.macos_version, "26.1");
        assert_eq!(facts.macos_major(), Some(26));
        assert_eq!(facts.serial_number, None);
        assert!(facts.administrator);
        let user = parse_facts(
            "version=15.6\narchitecture=arm64\naccount=me\nhome=/Users/me\nadministrator=no\n",
        )
        .unwrap();
        assert!(!user.administrator);
        assert_eq!(
            parse_facts("architecture=arm64\n").unwrap_err().code,
            "unexpected_response"
        );
    }

    #[test]
    fn tailscale_is_read_by_variant_and_state() {
        assert_eq!(parse_tailnet(b"variant=none\n"), RemoteTailnet::Absent);
        assert_eq!(
            parse_tailnet(b"variant=/usr/local/bin/tailscale\n"),
            RemoteTailnet::Offline {
                variant: TailscaleVariant::Daemon
            }
        );
        assert_eq!(
            parse_tailnet(
                b"variant=/Applications/Tailscale.app/Contents/MacOS/Tailscale\n{\"BackendState\":\"NeedsLogin\",\"Self\":null}"
            ),
            RemoteTailnet::Offline {
                variant: TailscaleVariant::App
            }
        );
        let RemoteTailnet::Online(node) = parse_tailnet(
            br#"variant=/usr/local/bin/tailscale
{"BackendState":"Running","Self":{"DNSName":"macbook-pro.example.ts.net.","TailscaleIPs":["100.64.0.20"],"KeyExpiry":"2027-05-17T10:00:00Z"}}"#,
        ) else {
            panic!("online");
        };
        assert_eq!(node.name, "macbook-pro");
        assert_eq!(node.domain.as_deref(), Some("example.ts.net"));
        assert_eq!(node.key_expiry.as_deref(), Some("2027-05-17T10:00:00Z"));
    }

    #[test]
    fn names_and_quoting_keep_the_command_line_safe() {
        for ok in [
            "macbook-pro.local",
            "192.168.64.30",
            "fe80::1%en0",
            "devbox",
            "me_2",
        ] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "-oProxyCommand=x", "a b", "a;b", "a'b", "a$(b)"] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn output_before_the_marker_is_dropped() {
        let noisy = format!("Welcome!\n{MARKER}\n{{\"data\":1}}\n");
        assert_eq!(after_marker(noisy.as_bytes()), b"{\"data\":1}\n");
        assert_eq!(after_marker(b"plain"), b"plain");
    }

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
        assert_eq!(code("Host key verification failed."), "host_key_changed");
    }

    #[test]
    fn errors_from_the_mac_are_reported_as_its_own() {
        let ran = Ran {
            success: false,
            code: Some(1),
            stdout:
                br#"{"schema_version":1,"error":{"code":"x","message":"m","hint":"run rm -rf /"}}"#
                    .to_vec(),
            stderr: String::new(),
        };
        let failure = envelope(&ran, "Running Elsecall on the Mac").unwrap_err();
        assert_eq!(failure.code, "remote_elsecall_failed");
        assert!(!failure.hint.contains("rm -rf"), "{}", failure.hint);
    }

    #[test]
    fn the_authorize_script_adds_a_key_once_beside_others() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".ssh")).unwrap();
        // An existing file without a final newline keeps its line intact.
        std::fs::write(
            home.path().join(".ssh/authorized_keys"),
            "ssh-rsa AAAAOther",
        )
        .unwrap();
        let line = crate::authorized_keys::line(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleMasterKey me@example.com",
            "mac-mini",
        )
        .unwrap();
        let script = authorize_script(&line).unwrap();
        let run = || {
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&script)
                .env("HOME", home.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap()
        };
        assert_eq!(run().trim(), "added");
        assert_eq!(run().trim(), "present");
        assert_eq!(
            std::fs::read_to_string(home.path().join(".ssh/authorized_keys")).unwrap(),
            format!("ssh-rsa AAAAOther\n{line}\n")
        );
    }
}
