//! Work requests: what a caller asks for, and the rules that make one valid.
use crate::{
    failure::{Failure, Result},
    scheduling::{capacity::Cores, placed::WorkspaceRef, settings::Settings, timestamp::Timestamp},
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A command Elsecall runs to completion.
    Run,
    /// Capacity held by a process for a section of its own work.
    Lease,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    Pinned,
    Placeable,
    /// A placeable request that must run on the named member.
    Member(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtDeadline {
    Start,
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    /// A caller is waiting. Without a hold limit, the default applies.
    Interactive { hold_limit_ms: Option<u64> },
    Deferrable {
        deadline: Timestamp,
        at_deadline: AtDeadline,
    },
}

/// A request as it crosses the local API.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionRequest {
    pub kind: Kind,
    /// The process a lease belongs to: the requesting process or one of its
    /// ancestors. Runs belong to the requesting process and leave it empty.
    #[serde(default)]
    pub holder_pid: Option<u32>,
    #[serde(default)]
    pub size: Option<Cores>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<crate::workload_metadata::Workload>,
    pub placement: Placement,
    pub urgency: Urgency,
    /// The working tree a placeable request was made in, which the member it is
    /// placed on copies. `None` outside a working tree.
    #[serde(default)]
    pub workspace: Option<WorkspaceRef>,
    /// Where the properties came from, for the record.
    #[serde(default)]
    pub settings: Option<Settings>,
}

/// A request that passed validation, with defaults applied.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkRequest {
    pub kind: Kind,
    pub holder_pid: Option<u32>,
    pub size: Cores,
    /// Whether the caller declared the size; otherwise it may be learned.
    pub size_declared: bool,
    pub label: Option<String>,
    pub workload: Option<crate::workload_metadata::Workload>,
    pub placement: Placement,
    pub urgency: Urgency,
    pub workspace: Option<WorkspaceRef>,
    pub settings: Option<Settings>,
}

pub const DEFAULT_SIZE: Cores = Cores::ONE;
pub const MAXIMUM_HOLD_LIMIT: Duration = Duration::from_secs(24 * 60 * 60);
pub const MAXIMUM_DEADLINE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub const MAXIMUM_LABEL: usize = 200;
pub const MAXIMUM_MEMBER: usize = 64;

pub fn invalid(message: impl Into<String>) -> Failure {
    Failure::new(
        "invalid_request",
        message,
        "Run elsecall run --help or elsecall lease acquire --help for valid options.",
    )
}

impl AdmissionRequest {
    pub fn validate(self, now: Timestamp) -> Result<WorkRequest> {
        let size = self.size.unwrap_or(DEFAULT_SIZE);
        if size == Cores::ZERO {
            return Err(invalid("The size must be greater than zero."));
        }
        if let Some(label) = &self.label
            && (label.is_empty()
                || label.chars().count() > crate::rehearsal::label_limit(label)
                || label.chars().any(char::is_control))
        {
            return Err(invalid(format!(
                "A label must be 1 to {MAXIMUM_LABEL} characters without control characters."
            )));
        }
        match (self.kind, &self.placement, self.holder_pid) {
            (Kind::Lease, Placement::Pinned, Some(_)) | (Kind::Run, _, None) => (),
            (Kind::Lease, Placement::Pinned, None) => {
                return Err(invalid("A lease needs the process that holds it."));
            }
            (Kind::Lease, _, _) => return Err(invalid("A lease is always pinned.")),
            (Kind::Run, _, Some(_)) => {
                return Err(invalid("A run belongs to the process that requests it."));
            }
        }
        if let Placement::Member(member) = &self.placement
            && (member.is_empty()
                || member.len() > MAXIMUM_MEMBER
                || !member
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')))
        {
            return Err(invalid(
                "A member name has 1 to 64 letters, digits, dots, underscores or dashes.",
            ));
        }
        match self.urgency {
            Urgency::Interactive {
                hold_limit_ms: Some(limit),
            } if Duration::from_millis(limit) > MAXIMUM_HOLD_LIMIT => {
                return Err(invalid("The hold limit may be at most 24 hours."));
            }
            Urgency::Deferrable { .. } if self.placement != Placement::Pinned => {
                return Err(invalid(
                    "Deferrable work is pinned only; drop --placeable or --member.",
                ));
            }
            Urgency::Deferrable { deadline, .. } if deadline.since(now) > MAXIMUM_DEADLINE => {
                return Err(invalid("The deadline may be at most 7 days ahead."));
            }
            _ => (),
        }
        if self.workload.is_some() && self.kind != Kind::Run {
            return Err(invalid(
                "Workload metadata belongs to commands, not leases.",
            ));
        }
        if let Some(workspace) = &self.workspace {
            workspace.validate()?;
        }
        if let Some(settings) = &self.settings {
            settings.validate(&self.urgency)?;
        }
        Ok(WorkRequest {
            kind: self.kind,
            holder_pid: self.holder_pid,
            size,
            size_declared: self.size.is_some(),
            label: self.label,
            workload: self.workload,
            placement: self.placement,
            urgency: self.urgency,
            workspace: self.workspace,
            settings: self.settings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> AdmissionRequest {
        AdmissionRequest {
            workload: None,
            kind: Kind::Run,
            holder_pid: None,
            size: None,
            label: Some("test FeatureModule".into()),
            placement: Placement::Pinned,
            urgency: Urgency::Interactive {
                hold_limit_ms: None,
            },
            workspace: None,
            settings: None,
        }
    }
    fn rejects(change: impl FnOnce(&mut AdmissionRequest)) -> String {
        let mut request = request();
        change(&mut request);
        let failure = request.validate(Timestamp::from_millis(0)).unwrap_err();
        assert_eq!(failure.code, "invalid_request");
        failure.message
    }

    #[test]
    fn applies_defaults() {
        let valid = request().validate(Timestamp::from_millis(0)).unwrap();
        assert_eq!(valid.size, Cores::ONE);
    }

    #[test]
    fn rejects_contradictory_or_out_of_range_properties() {
        let deferrable = Urgency::Deferrable {
            deadline: Timestamp::from_millis(1000),
            at_deadline: AtDeadline::Skip,
        };
        rejects(|r| r.size = Some(Cores::ZERO));
        rejects(|r| r.label = Some(String::new()));
        rejects(|r| r.label = Some("a\nb".into()));
        rejects(|r| r.label = Some("x".repeat(201)));
        rejects(|r| r.holder_pid = Some(1));
        rejects(|r| r.kind = Kind::Lease);
        rejects(|r| {
            r.kind = Kind::Lease;
            r.holder_pid = Some(1);
            r.placement = Placement::Placeable;
        });
        rejects(|r| r.placement = Placement::Member("../x".into()));
        assert!(
            rejects(|r| {
                r.placement = Placement::Placeable;
                r.urgency = deferrable;
            })
            .contains("pinned only")
        );
        rejects(|r| {
            r.urgency = Urgency::Interactive {
                hold_limit_ms: Some(MAXIMUM_HOLD_LIMIT.as_millis() as u64 + 1),
            }
        });
        rejects(|r| {
            r.urgency = Urgency::Deferrable {
                deadline: Timestamp::from_millis(0).after(MAXIMUM_DEADLINE * 2),
                at_deadline: AtDeadline::Start,
            }
        });
    }
}
