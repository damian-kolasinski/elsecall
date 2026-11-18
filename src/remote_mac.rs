//! The Mac being added, as the master reaches it: what it is, its tailnet, and the
//! changes Add Mac makes there. The production adapter is SSH (`openssh_mac`).
use crate::{failure::Result, inbound_firewall::Rule};
use serde::Serialize;
use std::path::PathBuf;

/// What the Mac is, read once before anything changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MacFacts {
    pub macos_version: String,
    /// `arm64` or `x86_64`, as `uname -m` reports it.
    pub architecture: String,
    pub model: Option<String>,
    pub serial_number: Option<String>,
    pub account: String,
    #[serde(skip)]
    pub home: String,
    pub administrator: bool,
    /// The Mac's Bonjour name, which becomes its tailnet name when Add Mac
    /// puts it on the tailnet.
    pub local_host_name: Option<String>,
}

impl MacFacts {
    /// The macOS major version, such as 26.
    pub fn macos_major(&self) -> Option<u32> {
        self.macos_version.split('.').next()?.parse().ok()
    }
}

/// Which Tailscale the Mac has, and whether it is on a tailnet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteTailnet {
    /// No Tailscale is installed.
    Absent,
    /// Tailscale is installed but not on a tailnet: stopped, logged out or
    /// waiting for a login.
    Offline {
        variant: TailscaleVariant,
    },
    Online(TailnetNode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TailscaleVariant {
    /// The Tailscale app, from the App Store or Tailscale's own download.
    App,
    /// The open-source `tailscaled`, as Homebrew or Add Mac installs it.
    Daemon,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailnetNode {
    /// The node's name on the tailnet, which becomes its member name.
    pub name: String,
    /// The tailnet's DNS domain, such as `example.ts.net`.
    pub domain: Option<String>,
    pub addresses: Vec<std::net::IpAddr>,
    /// When the node's key expires, as Tailscale reports it (RFC 3339); `None`
    /// when expiry is disabled.
    pub key_expiry: Option<String>,
}

/// The master's Elsecall build: its executable and, when the release has one, the
/// fleet window beside it.
#[derive(Debug, Clone)]
pub struct Build {
    pub executable: PathBuf,
    pub window: Option<PathBuf>,
}

/// The Mac being added. Every change is idempotent: it checks before it acts.
///
/// Used through generics within one task, never as a trait object.
#[allow(async_fn_in_trait)]
pub trait RemoteMac {
    /// The fingerprint of the SSH host key this session trusts.
    fn host_key(&self) -> &str;
    async fn facts(&mut self) -> Result<MacFacts>;
    /// Whether sudo accepts the account's password.
    async fn sudo_accepted(&mut self) -> Result<bool>;
    async fn tailnet(&mut self) -> Result<RemoteTailnet>;
    /// The installed Elsecall's build ID, or `None` when it is not installed or
    /// not running.
    async fn installed_build(&mut self) -> Result<Option<String>>;
    /// Copies `build` to the Mac and runs its `install`, returning its data.
    async fn install(&mut self, build: &Build) -> Result<serde_json::Value>;
    /// Runs the installed `elsecall` with `args`, returning its data, or its
    /// error as a failure.
    async fn elsecall(&mut self, args: &[&str]) -> Result<serde_json::Value>;
    /// What the Application Firewall does with incoming connections to `program`.
    async fn incoming(&mut self, program: &str) -> Result<Rule>;
    /// Lets the Application Firewall pass incoming connections to `programs`.
    async fn allow_incoming(&mut self, programs: &[String]) -> Result<()>;
    /// Adds `line` to the account's `authorized_keys` unless its key is there;
    /// true when it was added.
    async fn authorize_key(&mut self, line: &str) -> Result<bool>;
}
