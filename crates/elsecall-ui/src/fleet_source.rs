//! The fleet the window shows, and the port it reads that fleet through.
//!
//! Fields stop at what the window draws. Reachability, role and the admission
//! gate are enums so a row cannot be both online and offline, or both open and blocked.

use std::fmt;
use std::num::NonZeroU32;

use elsecall_peers::roster::Readiness;

/// A point-in-time fleet, as the window lists it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FleetSnapshot {
    /// False when this machine has not enrolled in a fleet.
    pub enrolled: bool,
    pub members: Vec<MemberRow>,
    pub standing: Standing,
    /// What needs an agent or a person, as the service derives it.
    pub attention: Vec<elsecall_contract::attention::Item>,
}

impl FleetSnapshot {
    /// Whether this Mac is the master, the one that adds Macs.
    pub(crate) fn this_is_master(&self) -> bool {
        self.standing == Standing::Rostered
            && self
                .members
                .iter()
                .any(|member| member.this_member && member.role == Role::Master)
    }
}

/// Whether this machine has a roster. Without one it is not even the master of its
/// own fleet of one, and an empty member list says nothing.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Standing {
    Rostered,
    /// Tailscale, where Elsecall finds and reaches members, is not running here.
    OffTailnet {
        hint: String,
    },
    /// On the tailnet, but without a roster for another reason the service names.
    Unrostered {
        message: String,
        hint: String,
    },
}

/// One member row.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MemberRow {
    pub name: String,
    /// This Mac's own row.
    pub this_member: bool,
    pub role: Role,
    pub reach: Reachability,
    pub readiness: Readiness,
    pub gate: Gate,
    /// Busy fraction of logical CPUs, from 0 to 1. Absent when the member did not report it.
    pub cpu: Option<f64>,
    pub logical_cpus: Option<NonZeroU32>,
    pub memory: Option<MemoryUse>,
    pub load: Option<LoadAverages>,
    pub release: Release,
    pub contributors: crate::resource_details::Contributors,
    /// The member's latest apply of its tool plan; none when it never applied
    /// one or did not say.
    pub provisioning: Option<elsecall_fleet::provisioning_model::ProvisioningSummary>,
}

/// How a member's release compares with the master's. Work moves between members
/// only when they run the same build, so any other answer leaves the member out of
/// placement until `elsecall fleet upgrade` brings it to the master's build.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Release {
    /// The master's build.
    Matching { version: String },
    /// An older release than the master's.
    Behind { version: String },
    /// A newer release than the master's.
    Ahead { version: String },
    /// Another build of the master's version, such as a local one.
    OtherBuild { version: String },
    /// Unreachable, too old to report its build, or with no comparison from the
    /// service, as while the master is unreachable.
    Unknown { version: Option<String> },
}
impl Release {
    /// Whether the member runs another build than the master.
    pub(crate) fn differs(&self) -> bool {
        matches!(
            self,
            Release::Behind { .. } | Release::Ahead { .. } | Release::OtherBuild { .. }
        )
    }

    /// How a member reporting `version` with this skew flag compares with `master`.
    pub(crate) fn compare(
        version: Option<String>,
        skew: Option<bool>,
        master: Option<&str>,
    ) -> Self {
        let Some(version) = version else {
            return Release::Unknown { version: None };
        };
        match skew {
            Some(false) => Release::Matching { version },
            None => Release::Unknown {
                version: Some(version),
            },
            Some(true) => {
                let parse = |text: &str| semver::Version::parse(text).ok();
                match (parse(&version), master.and_then(parse)) {
                    (Some(own), Some(master)) if own < master => Release::Behind { version },
                    (Some(own), Some(master)) if own > master => Release::Ahead { version },
                    _ => Release::OtherBuild { version },
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Master,
    Member,
}

/// Whether this member answered the fleet view.
///
/// `GET /v1/fleet` sends an error object and no timestamp for an unreachable
/// member, so this stays a unit variant until the payload names a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Reachability {
    Reachable,
    Unreachable,
}

/// Whether outside pressure holds the member's admission gate closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    Open,
    Blocked { leading_process: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MemoryUse {
    pub used_bytes: u64,
    pub total_bytes: u64,
}

/// Load averages over the last 1, 5 and 15 minutes. Always finite and nonnegative.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LoadAverages {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

impl LoadAverages {
    /// Recent runnable work per logical CPU.
    pub(crate) fn demand(self, logical_cpus: NonZeroU32) -> f64 {
        self.one / f64::from(logical_cpus.get())
    }
}

/// Why a fleet read failed. The window shows `message` and `hint` either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SourceFailure {
    /// The local service socket could not be reached.
    ServiceUnavailable { message: String, hint: String },
    /// The service answered with an error, or the body could not be read.
    Rejected { message: String, hint: String },
}

impl SourceFailure {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::ServiceUnavailable { message, .. } | Self::Rejected { message, .. } => message,
        }
    }

    pub(crate) fn hint(&self) -> &str {
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
pub(crate) trait FleetSource: Send + Sync {
    fn fleet(&self) -> Result<FleetSnapshot, SourceFailure>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_member_compares_with_the_master_only_when_the_service_says_they_differ() {
        let version = |text: &str| Some(text.to_owned());
        assert_eq!(
            Release::compare(version("0.2.2"), Some(false), Some("0.2.2")),
            Release::Matching {
                version: "0.2.2".into()
            }
        );
        assert_eq!(
            Release::compare(version("0.2.1"), Some(true), Some("0.2.2")),
            Release::Behind {
                version: "0.2.1".into()
            }
        );
        assert_eq!(
            Release::compare(version("0.10.0"), Some(true), Some("0.9.0")),
            Release::Ahead {
                version: "0.10.0".into()
            }
        );
        // The same version on another build, such as a local one, or a version
        // that is not semver.
        assert_eq!(
            Release::compare(version("0.2.2"), Some(true), Some("0.2.2")),
            Release::OtherBuild {
                version: "0.2.2".into()
            }
        );
        assert_eq!(
            Release::compare(version("local"), Some(true), Some("0.2.2")),
            Release::OtherBuild {
                version: "local".into()
            }
        );
        // A master that reports no build differs from any member that does.
        assert_eq!(
            Release::compare(version("0.2.1"), Some(true), None),
            Release::OtherBuild {
                version: "0.2.1".into()
            }
        );
        assert_eq!(
            Release::compare(version("0.2.1"), None, None),
            Release::Unknown {
                version: Some("0.2.1".into())
            }
        );
        assert_eq!(
            Release::compare(None, None, Some("0.2.2")),
            Release::Unknown { version: None }
        );
    }
}
