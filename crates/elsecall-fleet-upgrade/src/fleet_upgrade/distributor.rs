//! Where this master distributes a build to the rest of the fleet from, so that
//! `elsecall fleet upgrade` and the fleet half of a release update never distribute
//! at the same time.
use super::master::{self, Outcome, UpgradeReport, UpgradeRequest};
use elsecall_failure::{Failure, Result};
use elsecall_fleet::fleet::Fleet;
use elsecall_release_update::release_update::{FleetDistribution, FleetOutcome, MemberOutcome};
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

pub struct Distributor {
    fleet: Arc<Fleet>,
    /// The master's installed executable, distributed unless another is named.
    installed: PathBuf,
    running: tokio::sync::Mutex<()>,
}

impl Distributor {
    pub fn new(fleet: Arc<Fleet>, installed: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            fleet,
            installed,
            running: tokio::sync::Mutex::new(()),
        })
    }

    pub async fn distribute(&self, request: UpgradeRequest) -> Result<UpgradeReport> {
        let Ok(_running) = self.running.try_lock() else {
            return Err(Failure::new(
                "upgrade_in_progress",
                "This master is already distributing its build to the fleet.",
                "Wait for it to finish; elsecall fleet then shows each member's build, and elsecall fleet upgrade reaches any left behind.",
            ));
        };
        master::upgrade(&self.fleet, self.installed.clone(), request).await
    }
}

impl FleetDistribution for Distributor {
    fn leads_others(&self) -> bool {
        self.fleet.roster().is_ok_and(|roster| {
            roster.master().key == self.fleet.public_key() && roster.members().len() > 1
        })
    }

    fn distribute(
        &self,
        wait_seconds: u64,
    ) -> Pin<Box<dyn Future<Output = Result<FleetOutcome>> + Send + '_>> {
        Box::pin(async move {
            let report = Distributor::distribute(
                self,
                UpgradeRequest {
                    member: None,
                    wait_seconds,
                    executable: None,
                },
            )
            .await?;
            Ok(FleetOutcome {
                complete: report.complete,
                members: report
                    .members
                    .iter()
                    .map(|member| MemberOutcome {
                        name: member.name.to_string(),
                        outcome: outcome_name(&member.outcome),
                    })
                    .collect(),
            })
        })
    }
}

/// The outcome's name as `elsecall fleet upgrade` reports it, such as `deferred`.
fn outcome_name(outcome: &Outcome) -> String {
    serde_json::to_value(outcome)
        .ok()
        .and_then(|value| value.get("outcome")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_are_named_as_the_fleet_upgrade_report_names_them() {
        let unreachable = Outcome::Unreachable {
            error: Failure::new("member_unavailable", "gone", "check"),
        };
        assert_eq!(outcome_name(&unreachable), "unreachable");
    }
}
