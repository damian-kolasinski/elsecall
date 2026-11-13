//! The fleet as this member sees it: every member's reachability, role and state,
//! each fetched from that member over the authenticated channel.
use crate::{
    build_identity::{Build, BuildId},
    failure::Failure,
    fleet::{Fleet, PeerStatus},
    member_key::PublicKey,
    membership_store::Membership,
    peer_client,
    roster::MemberName,
};
use hyper::Method;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Master,
    Member,
}

#[derive(Debug, Serialize)]
#[serde(tag = "reachability", rename_all = "snake_case")]
pub enum Reach {
    Reachable {
        /// Whether the member records exactly this member's roster.
        in_sync: bool,
        status: Box<PeerStatus>,
    },
    Unreachable {
        error: Failure,
    },
}

#[derive(Debug, Serialize)]
pub struct MemberView {
    pub name: MemberName,
    pub key: PublicKey,
    pub endpoints: Vec<SocketAddr>,
    pub role: Role,
    pub this_member: bool,
    #[serde(flatten)]
    pub reach: Reach,
    /// The latest run placed between this member and that one, with its
    /// consistency warnings; absent when there was none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_placed_run: Option<PlacedRunSummary>,
    /// The member's configuration sync state against the master's; its details
    /// are `status.config_sync`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_sync: Option<crate::config_sync::report::SyncState>,
    /// The member's version and build, as it reports them; null when unreachable
    /// or when its build predates build IDs.
    pub build: Option<Build>,
    /// Whether the member runs another build than the master; null when unknown.
    pub version_skew: Option<bool>,
    /// Set while outside pressure holds the member's admission gate closed: why,
    /// since when, and the outside processes behind it. Null when it admits, when
    /// unreachable, and for members whose build predates the report.
    pub blocked: Option<crate::outside_pressure::Blocked>,
    /// Outside processes that have spun or grown for hours, the
    /// longest-standing first. A warning, not a block: the member still admits
    /// work. Empty when there are none, when unreachable, and for members whose
    /// build predates them.
    pub outside_warnings: Vec<crate::outside_pressure::Warning>,
}

/// Members that run another build than the master, and the repair.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionSkew {
    pub master_build: Option<BuildId>,
    pub members: Vec<MemberName>,
    /// Work between members on different builds is refused with this code.
    pub refused_with: String,
    pub hint: String,
}

/// A recent placed run between two members, as this member recorded it.
#[derive(Debug, Clone, Serialize)]
pub struct PlacedRunSummary {
    pub run_id: String,
    pub started_at: Option<crate::scheduling::timestamp::Timestamp>,
    pub warnings: Vec<String>,
}

/// Where the fleet view learns about placed runs.
pub trait PlacedRuns: Send + Sync {
    fn latest_with(&self, member: &str) -> Option<PlacedRunSummary>;
}

#[derive(Debug, Serialize)]
pub struct FleetView {
    pub enrolled: bool,
    pub epoch: u64,
    pub master: Option<MemberName>,
    pub members: Vec<MemberView>,
    /// Present while reachable members run different builds.
    pub version_skew: Option<VersionSkew>,
}

/// Each member's build and the fleet's skew, and the members outside pressure
/// blocks, as `doctor` reports them.
#[derive(Debug, Serialize, Deserialize)]
pub struct FleetBuilds {
    pub members: Vec<MemberBuild>,
    pub version_skew: Option<VersionSkew>,
    /// Absent from services that predate the report; an entry this build
    /// cannot read is dropped.
    #[serde(
        default,
        deserialize_with = "crate::outside_pressure::blocked::lenient_list"
    )]
    pub blocked: Vec<BlockedMember>,
    /// Early warnings per member; absent from services that predate them, and
    /// an entry this build cannot read is dropped.
    #[serde(
        default,
        deserialize_with = "crate::outside_pressure::blocked::lenient_list"
    )]
    pub outside_warnings: Vec<MemberWarning>,
}

/// A member whose admission gate outside pressure holds closed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockedMember {
    pub member: String,
    #[serde(flatten)]
    pub blocked: crate::outside_pressure::Blocked,
}

/// An early warning about an outside process on a member.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemberWarning {
    pub member: String,
    #[serde(flatten)]
    pub warning: crate::outside_pressure::Warning,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MemberBuild {
    pub name: MemberName,
    pub build: Option<Build>,
    pub version_skew: Option<bool>,
}

impl FleetView {
    pub fn builds(self) -> FleetBuilds {
        FleetBuilds {
            blocked: self
                .members
                .iter()
                .filter_map(|member| {
                    Some(BlockedMember {
                        member: member.name.to_string(),
                        blocked: member.blocked.clone()?,
                    })
                })
                .collect(),
            outside_warnings: self
                .members
                .iter()
                .flat_map(|member| {
                    member.outside_warnings.iter().map(|warning| MemberWarning {
                        member: member.name.to_string(),
                        warning: warning.clone(),
                    })
                })
                .collect(),
            members: self
                .members
                .into_iter()
                .map(|member| MemberBuild {
                    name: member.name,
                    build: member.build,
                    version_skew: member.version_skew,
                })
                .collect(),
            version_skew: self.version_skew,
        }
    }
}

/// Compares each reachable member's build with the master's.
fn flag_skew(view: &mut FleetView) {
    let builds: Vec<Option<Build>> = view
        .members
        .iter()
        .map(|member| match &member.reach {
            Reach::Reachable { status, .. } => Some(status.build.clone().map(|build| Build {
                version: status.version.clone(),
                build,
            })),
            Reach::Unreachable { .. } => None,
        })
        .map(Option::flatten)
        .collect();
    let master = view
        .members
        .iter()
        .zip(&builds)
        .find(|(m, _)| matches!(m.role, Role::Master))
        .and_then(|(m, b)| match m.reach {
            Reach::Reachable { .. } => Some(b.as_ref().map(|b| b.build.clone())),
            Reach::Unreachable { .. } => None,
        });
    let mut skewed = Vec::new();
    for (member, build) in view.members.iter_mut().zip(builds) {
        member.version_skew = match (&member.reach, &master) {
            (Reach::Reachable { .. }, Some(master)) => {
                let differs = build.as_ref().map(|b| &b.build) != master.as_ref();
                if differs {
                    skewed.push(member.name.clone());
                }
                Some(differs)
            }
            _ => None,
        };
        member.build = build;
    }
    if !skewed.is_empty() {
        let master_name = view.master.clone();
        view.version_skew = Some(VersionSkew {
            master_build: master.flatten(),
            members: skewed,
            refused_with: "version_skew".into(),
            hint: match master_name {
                Some(name) => format!(
                    "Run elsecall fleet upgrade on {name} so every member runs its build. Until then placement, configuration sync, workspace checks, reports and tool inventories between different builds are refused."
                ),
                None => "Run elsecall fleet upgrade on the master.".into(),
            },
        });
    }
}

pub async fn view(fleet: &Fleet, placed: &dyn PlacedRuns) -> FleetView {
    let mut view = members(fleet).await;
    for member in &mut view.members {
        if !member.this_member {
            member.latest_placed_run = placed.latest_with(member.name.as_str());
        }
    }
    let master_sync = view
        .members
        .iter()
        .find(|m| matches!(m.role, Role::Master))
        .and_then(|m| match &m.reach {
            Reach::Reachable { status, .. } => status.config_sync.clone(),
            Reach::Unreachable { .. } => None,
        });
    for member in &mut view.members {
        if let Reach::Reachable { status, .. } = &mut member.reach {
            if let Some(summary) = &mut status.config_sync {
                summary.service_against(master_sync.as_ref());
            }
            member.config_sync = status
                .config_sync
                .as_ref()
                .map(|summary| summary.state_against(master_sync.as_ref()));
            member.blocked = status.blocked.clone();
            member.outside_warnings = status.outside_warnings.clone();
        }
    }
    flag_skew(&mut view);
    view
}

async fn members(fleet: &Fleet) -> FleetView {
    let enrolled = matches!(fleet.membership(), Membership::Enrolled(_));
    let roster = match fleet.roster() {
        Ok(roster) => roster,
        // Without a tailnet presence a solo member cannot even reach itself.
        Err(error) => {
            return FleetView {
                enrolled,
                epoch: 0,
                master: None,
                version_skew: None,
                members: fleet
                    .presence()
                    .into_iter()
                    .map(|(name, endpoints)| MemberView {
                        name,
                        key: fleet.public_key(),
                        endpoints,
                        role: Role::Master,
                        this_member: true,
                        reach: Reach::Unreachable {
                            error: error.clone(),
                        },
                        latest_placed_run: None,
                        config_sync: None,
                        build: None,
                        version_skew: None,
                        blocked: None,
                        outside_warnings: Vec::new(),
                    })
                    .collect(),
            };
        }
    };
    let statuses = peer_client::call_each::<PeerStatus>(
        fleet.key(),
        roster.members(),
        Method::GET,
        "/peer/v1/status",
        None::<&()>,
    )
    .await;
    let members = roster
        .members()
        .iter()
        .zip(statuses)
        .map(|(member, status)| MemberView {
            name: member.name.clone(),
            key: member.key,
            endpoints: member.endpoints.clone(),
            role: if member.name == roster.master().name {
                Role::Master
            } else {
                Role::Member
            },
            this_member: member.key == fleet.public_key(),
            reach: match status {
                Ok(status) => Reach::Reachable {
                    in_sync: status.roster == roster,
                    status: Box::new(status),
                },
                Err(error) => Reach::Unreachable { error },
            },
            latest_placed_run: None,
            config_sync: None,
            build: None,
            version_skew: None,
            blocked: None,
            outside_warnings: Vec::new(),
        })
        .collect();
    FleetView {
        enrolled,
        epoch: roster.epoch(),
        master: Some(roster.master().name.clone()),
        members,
        version_skew: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        outside_pressure::{
            sampling::Owner,
            warning::{Concern, Warning},
        },
        scheduling::timestamp::Timestamp,
    };
    use serde_json::json;

    fn fseventsd() -> Warning {
        Warning {
            concern: Concern::Spinning { cpu_cores: 1.0 },
            process: "fseventsd".into(),
            pid: 412,
            started: Timestamp::parse_rfc3339("2026-09-09T08:00:00Z").unwrap(),
            owner: Owner::Root,
            since: Timestamp::parse_rfc3339("2026-11-08T21:00:00Z").unwrap(),
            observed_at: Timestamp::parse_rfc3339("2026-11-09T03:05:00Z").unwrap(),
        }
    }

    fn member(name: &str, warnings: Vec<Warning>) -> MemberView {
        MemberView {
            name: name.parse().unwrap(),
            key: crate::member_key::MemberKey::generate().unwrap().public(),
            endpoints: Vec::new(),
            role: Role::Member,
            this_member: false,
            reach: Reach::Unreachable {
                error: Failure::new("member_unavailable", "gone", "check"),
            },
            latest_placed_run: None,
            config_sync: None,
            build: None,
            version_skew: None,
            blocked: None,
            outside_warnings: warnings,
        }
    }

    #[test]
    fn members_and_doctor_show_outside_warnings_per_member() {
        let view = FleetView {
            enrolled: true,
            epoch: 3,
            master: Some("mac-mini".parse().unwrap()),
            members: vec![
                member("mac-mini", vec![]),
                member("macbook-pro", vec![fseventsd()]),
            ],
            version_skew: None,
        };
        let rendered = serde_json::to_value(&view).unwrap();
        assert_eq!(rendered["members"][0]["outside_warnings"], json!([]));
        assert_eq!(
            rendered["members"][1]["outside_warnings"],
            json!([{
                "concern": "spinning", "cpu_cores": 1.0, "process": "fseventsd", "pid": 412,
                "started": "2026-09-09T08:00:00.000Z", "owner": "root",
                "since": "2026-11-08T21:00:00.000Z", "observed_at": "2026-11-09T03:05:00.000Z"
            }])
        );
        let builds = serde_json::to_value(view.builds()).unwrap();
        assert_eq!(
            builds["outside_warnings"],
            json!([{
                "member": "macbook-pro",
                "concern": "spinning", "cpu_cores": 1.0, "process": "fseventsd", "pid": 412,
                "started": "2026-09-09T08:00:00.000Z", "owner": "root",
                "since": "2026-11-08T21:00:00.000Z", "observed_at": "2026-11-09T03:05:00.000Z"
            }])
        );
    }

    #[test]
    fn doctor_reads_services_that_predate_or_postdate_warnings() {
        let earlier: FleetBuilds =
            serde_json::from_value(json!({"members": [], "version_skew": null})).unwrap();
        assert!(earlier.outside_warnings.is_empty());
        let mut known = serde_json::to_value(MemberWarning {
            member: "macbook-pro".into(),
            warning: fseventsd(),
        })
        .unwrap();
        let mut later = known.clone();
        later["concern"] = json!("disk_thrashing");
        let mixed: FleetBuilds = serde_json::from_value(json!({
            "members": [], "version_skew": null, "outside_warnings": [known.take(), later]
        }))
        .unwrap();
        assert_eq!(
            mixed.outside_warnings.len(),
            1,
            "a later concern is dropped"
        );
        assert_eq!(mixed.outside_warnings[0].warning, fseventsd());
    }
}
