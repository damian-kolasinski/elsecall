//! The fleet the window shows, and the port it reads that fleet through.
//!
//! Fields stop at what the window draws. Reachability, role and the admission
//! gate are enums so a row cannot be both online and offline, or both open and blocked.

use std::fmt;

/// A point-in-time fleet, as the window lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct FleetSnapshot {
    /// False when this machine has not enrolled in a fleet.
    pub enrolled: bool,
    pub members: Vec<MemberRow>,
}

/// One member row.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberRow {
    pub name: String,
    pub role: Role,
    pub reach: Reachability,
    pub gate: Gate,
    /// Busy fraction of logical CPUs, from 0 to 1. Absent when the member did not report it.
    pub cpu: Option<f64>,
    pub memory: Option<MemoryUse>,
    pub load: Option<LoadAverages>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Master,
    Member,
}

/// Whether this member answered the fleet view.
///
/// `GET /v1/fleet` sends an error object and no timestamp for an unreachable
/// member, so this stays a unit variant until the payload names a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reachability {
    Reachable,
    Unreachable,
}

/// Whether outside pressure holds the member's admission gate closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    Open,
    Blocked { leading_process: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryUse {
    pub used_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoadAverages {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

/// Why a fleet read failed. The window shows `message` and `hint` either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceFailure {
    /// The local service socket could not be reached.
    ServiceUnavailable { message: String, hint: String },
    /// The service answered with an error, or the body could not be read.
    Rejected { message: String, hint: String },
}

impl SourceFailure {
    pub fn message(&self) -> &str {
        match self {
            Self::ServiceUnavailable { message, .. } | Self::Rejected { message, .. } => message,
        }
    }

    pub fn hint(&self) -> &str {
        match self {
            Self::ServiceUnavailable { hint, .. } | Self::Rejected { hint, .. } => hint,
        }
    }
}

impl fmt::Display for SourceFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message())
    }
}

/// Where the window reads the fleet. The UI thread never calls this.
pub trait FleetSource: Send + Sync {
    fn fleet(&self) -> Result<FleetSnapshot, SourceFailure>;

    /// Build id of the window installed with the local service.
    /// Absent when this service does not report one.
    fn installed_window(&self) -> Option<String> {
        None
    }
}
