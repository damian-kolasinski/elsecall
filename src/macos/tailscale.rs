//! Tailnet discovery through the Tailscale command-line interface.
use crate::{
    failure::Result,
    tailnet::{Presence, Tailnet, is_tailnet_address, unavailable},
};
use serde::Deserialize;
use std::{
    io::Read,
    net::IpAddr,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Fixed install locations; the service never searches PATH for the binary.
const CANDIDATES: [&str; 3] = [
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/opt/homebrew/bin/tailscale",
    "/usr/local/bin/tailscale",
];
const OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;

pub struct TailscaleCli;
impl Tailnet for TailscaleCli {
    fn discover(&self) -> Result<Presence> {
        let binary = CANDIDATES
            .iter()
            .find(|path| Path::new(path).is_file())
            .ok_or_else(|| unavailable("Tailscale is not installed"))?;
        let mut command = Command::new(binary);
        command
            .args(["status", "--json", "--peers=false"])
            // Without a terminal, the app bundle binary would otherwise try to start its GUI.
            .env("TAILSCALE_BE_CLI", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = crate::child_start::spawn(&mut command)
            .map_err(|_| unavailable("the Tailscale command could not be started"))?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = (&mut stdout).take(OUTPUT_LIMIT).read_to_end(&mut bytes);
            bytes
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(unavailable("the Tailscale command did not finish"));
                }
            }
        }
        let bytes = reader.join().unwrap_or_default();
        parse(&bytes)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Status {
    backend_state: String,
    #[serde(rename = "Self")]
    this: Option<Node>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Node {
    #[serde(rename = "DNSName")]
    dns_name: String,
    #[serde(default, rename = "TailscaleIPs")]
    tailscale_ips: Vec<IpAddr>,
}

fn parse(bytes: &[u8]) -> Result<Presence> {
    let status: Status = serde_json::from_slice(bytes)
        .map_err(|_| unavailable("Tailscale status was not readable"))?;
    if status.backend_state != "Running" {
        return Err(unavailable("Tailscale is not running"));
    }
    let node = status
        .this
        .ok_or_else(|| unavailable("Tailscale reported no node"))?;
    let name = node
        .dns_name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    // Command output is external data: keep only addresses inside tailnet ranges.
    let addresses: Vec<IpAddr> = node
        .tailscale_ips
        .into_iter()
        .filter(|address| is_tailnet_address(*address))
        .collect();
    if name.is_empty() || addresses.is_empty() {
        return Err(unavailable("Tailscale reported no name or addresses"));
    }
    Ok(Presence { name, addresses })
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn reads_node_name_and_only_tailnet_addresses() {
        let presence = parse(
            br#"{"BackendState":"Running","Self":{"DNSName":"Mac-Mini.example.ts.net.","TailscaleIPs":["100.64.0.10","fd7a:115c:a1e0::a","192.168.1.10"]}}"#,
        )
        .unwrap();
        assert_eq!(presence.name, "mac-mini");
        assert_eq!(presence.addresses.len(), 2);
        assert_eq!(
            parse(br#"{"BackendState":"Stopped","Self":null}"#)
                .unwrap_err()
                .code,
            "tailnet_unavailable"
        );
        assert!(parse(br#"{"BackendState":"Running","Self":{"DNSName":"a.b.","TailscaleIPs":["10.0.0.1"]}}"#).is_err());
    }
}
