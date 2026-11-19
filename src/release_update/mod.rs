//! Local upgrades from signed releases: the published ones, or for test builds a
//! local channel (see `channel`). Discovery never installs code.
pub mod catalog;
pub mod channel;
pub mod download;
pub mod helper;
pub mod local_channel;
pub mod rehearsal;
pub mod selection;
pub mod service;
pub mod store;
mod tools;

use crate::build_identity::Digest;
use elsecall_failure::{Failure, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const PROJECT: &str = "github.com/damian-kolasinski/elsecall";
pub const TOOL: &str = "packslip:github.com/damian-kolasinski/elsecall";
pub const REPOSITORY: &str = "damian-kolasinski/elsecall";
pub const ISSUER: &str = "https://token.actions.githubusercontent.com";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ReleaseVersion(semver::Version);
impl ReleaseVersion {
    pub fn running() -> Self {
        Self(semver::Version::parse(crate::build_identity::VERSION).expect("Cargo version"))
    }
    pub fn as_version(&self) -> &semver::Version {
        &self.0
    }
}
impl TryFrom<String> for ReleaseVersion {
    type Error = Failure;
    fn try_from(value: String) -> Result<Self> {
        let version = semver::Version::parse(&value)
            .map_err(|_| invalid("Expected an exact stable release version."))?;
        if !version.pre.is_empty() || !version.build.is_empty() {
            return Err(invalid(
                "Only stable releases without build metadata are supported.",
            ));
        }
        Ok(Self(version))
    }
}
impl From<ReleaseVersion> for String {
    fn from(value: ReleaseVersion) -> String {
        value.to_string()
    }
}
impl std::fmt::Display for ReleaseVersion {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(out)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Release {
    pub version: ReleaseVersion,
}
impl Release {
    pub fn filename(&self) -> String {
        format!("elsecall-v{}-aarch64-apple-darwin.tar.gz", self.version)
    }
    pub fn identity(&self) -> String {
        format!(
            "https://github.com/{REPOSITORY}/.github/workflows/release.yml@refs/tags/v{}",
            self.version
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct JobId(String);
impl JobId {
    pub fn new() -> Result<Self> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|_| invalid("Could not create an update identifier."))?;
        Ok(Self(
            random.iter().map(|byte| format!("{byte:02x}")).collect(),
        ))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for JobId {
    type Error = Failure;
    fn try_from(value: String) -> Result<Self> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid("Invalid update identifier."));
        }
        Ok(Self(value))
    }
}
impl From<JobId> for String {
    fn from(value: JobId) -> String {
        value.0
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Downloading,
    WaitingForWork,
    Installing,
    SelectingCli,
    Reopening,
    Restoring,
    Succeeded,
    Failed {
        error: Failure,
        rolled_back: bool,
    },
    /// Future service phases remain readable by older CLI and window builds.
    #[serde(other)]
    Unknown,
}
impl Phase {
    pub fn terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed { .. })
    }
    pub fn installation_started(&self) -> bool {
        matches!(
            self,
            Self::Installing | Self::SelectingCli | Self::Reopening | Self::Restoring
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub release: Release,
    pub from_build: crate::build_identity::BuildId,
    #[serde(default)]
    pub started_at_ms: u64,
    pub reopen: bool,
    pub wait_seconds: u64,
    #[serde(flatten)]
    pub phase: Phase,
    pub updated_at: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Discovery {
    pub latest: Option<Release>,
    pub checked_at: Option<u64>,
    pub error: Option<Failure>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub current_version: ReleaseVersion,
    pub available: Option<Release>,
    pub checked_at: Option<u64>,
    pub check_error: Option<Failure>,
    pub job: Option<Job>,
    /// On a master, bringing the rest of the fleet to the build the last update
    /// installed. Older services do not report it.
    #[serde(default)]
    pub fleet: Option<FleetFollowUp>,
}

/// Gives the rest of the fleet the build this master runs, after an update
/// installed it here.
pub trait FleetDistribution: Send + Sync {
    /// Whether this machine is the master of a fleet with other members.
    fn leads_others(&self) -> bool;
    /// Distributes the running build to every other member, waiting up to
    /// `wait_seconds` for each one's work.
    fn distribute(
        &self,
        wait_seconds: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<FleetOutcome>> + Send + '_>>;
}

/// How distributing a build to the fleet ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetOutcome {
    /// Whether every member now runs the build.
    pub complete: bool,
    pub members: Vec<MemberOutcome>,
}

/// One member's outcome, named as `elsecall fleet upgrade` reports it, such as
/// `upgraded`, `already_current`, `deferred` or `unreachable`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberOutcome {
    pub name: String,
    pub outcome: String,
}

/// The fleet half of the update job `job`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FleetFollowUp {
    pub job: JobId,
    #[serde(flatten)]
    pub state: FollowUpState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FollowUpState {
    /// The master is giving the members its build.
    Distributing { started_at: u64 },
    /// Members left behind, such as busy or unreachable ones, stay on their build
    /// and are marked in the fleet window until `elsecall fleet upgrade` reaches them.
    Distributed { outcome: FleetOutcome },
    /// Distribution could not start or run; the master's own update stands.
    Failed { error: Failure },
    /// A state from a newer build.
    #[serde(other)]
    Unknown,
}

/// The release channel, independent of where its metadata is hosted.
pub trait ReleaseCatalog: Send + Sync {
    fn latest(&self) -> Result<Option<Release>>;
}
/// Produces verified CLI/UI files without changing the active installation.
pub trait ReleaseDownload: Send + Sync {
    fn stage(&self, release: &Release, directory: &std::path::Path) -> Result<StagedRelease>;
}
#[derive(Clone, Debug)]
pub struct StagedRelease {
    pub executable: PathBuf,
    pub window: PathBuf,
    pub digest: Digest,
    pub window_digest: Digest,
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}
pub fn invalid(message: &str) -> Failure {
    Failure::new(
        "release_update_failed",
        message,
        "Run elsecall update check and retry; inspect elsecall update status for the last result.",
    )
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis() as u64
}
