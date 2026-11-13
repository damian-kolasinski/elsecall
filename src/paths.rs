use crate::failure::{Failure, Result};
use std::path::{Path, PathBuf};

pub const LABEL: &str = "dev.elsecall.agent";

#[derive(Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub data: PathBuf,
    pub bin: PathBuf,
    pub agent: PathBuf,
}
impl Paths {
    pub fn for_home(home: &Path) -> Result<Self> {
        if !home.is_absolute()
            || home.to_str().is_none()
            || home
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(Failure::new(
                "invalid_home",
                "HOME must be an absolute UTF-8 path without parent traversal.",
                "Set HOME to your user home directory.",
            ));
        }
        let paths = Self {
            home: home.into(),
            data: home.join("Library/Application Support/Elsecall"),
            bin: home.join(".local/bin/elsecall"),
            agent: home.join(format!("Library/LaunchAgents/{LABEL}.plist")),
        };
        if paths.socket().as_os_str().as_encoded_bytes().len() >= 104 {
            return Err(Failure::new(
                "path_too_long",
                "The local socket path exceeds the macOS limit.",
                "This version requires a shorter home directory path (socket path below 104 bytes).",
            ));
        }
        Ok(paths)
    }
    pub fn current() -> Result<Self> {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            Failure::new(
                "invalid_home",
                "HOME is not set.",
                "Run from your user account.",
            )
        })?;
        Self::for_home(Path::new(&home))
    }
    pub fn socket(&self) -> PathBuf {
        self.data.join("service.sock")
    }
    pub fn executable(&self) -> PathBuf {
        self.data.join("elsecall")
    }
    /// The window binary installed with this Elsecall build.
    pub fn window_executable(&self) -> PathBuf {
        self.data.join("elsecall-ui")
    }
    /// The app the user launches from Finder.
    pub fn window_app(&self) -> PathBuf {
        self.home.join("Applications/Elsecall.app")
    }
    pub fn plist(&self) -> PathBuf {
        self.data.join("agent.plist")
    }
    pub fn configuration(&self) -> PathBuf {
        self.data.join("config.toml")
    }
    /// This member's load over time.
    pub fn load_history(&self) -> PathBuf {
        self.data.join("load.sqlite")
    }
    /// Episodes in which outside pressure blocked this member.
    pub fn outside_pressure(&self) -> PathBuf {
        self.data.join("outside-pressure.sqlite")
    }
    /// Trends of outside processes and the early warnings they raised.
    pub fn outside_processes(&self) -> PathBuf {
        self.data.join("outside-processes.sqlite")
    }
    pub fn records(&self) -> PathBuf {
        self.data.join("records.sqlite")
    }
    pub fn log(&self) -> PathBuf {
        self.data.join("service.log")
    }
    /// This member's private signing key; owner-only.
    pub fn member_key(&self) -> PathBuf {
        self.data.join("member.key")
    }
    /// This member's fleet roster; owner-only.
    pub fn fleet_membership(&self) -> PathBuf {
        self.data.join("fleet.json")
    }
}
