//! What a member reports about installing its tools: whether it is carrying out
//! a plan now, and which steps of the latest one failed.
//!
//! `elsecall tools apply` on the master sends each member its steps, and the
//! member installs them as its own admitted work. The member keeps the latest
//! apply and reports it in its status, from which every member's fleet view
//! derives one `provisioning_step_failed` attention item per failed step. A
//! failed step stands until the next apply on that member, which retries it or
//! no longer needs it.
//!
//! A step's failure is a code, never the output of what failed, so nothing a
//! command printed (a token in a URL, say) reaches a status, an item or a log.
//! Another member's report is read leniently: one this build cannot read is no
//! report, and a failed step it cannot read is left out.
use elsecall_contract::scheduling_model::timestamp::Timestamp;
use serde::{Deserialize, Deserializer, Serialize};

/// The longest tool name, version, step or code a report may hold.
const MAXIMUM_TEXT: usize = 160;
/// More failed steps than one apply sends; a bound on another member's report.
pub const MAXIMUM_STEPS: usize = 256;

/// Where the fleet learns this member's latest apply.
pub trait ProvisioningReport: Send + Sync {
    /// The latest apply, or none when this member never applied a plan.
    fn summary(&self) -> Option<ProvisioningSummary>;
}

/// The latest apply on a member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisioningSummary {
    pub state: ApplyState,
    pub started_at: Timestamp,
    /// Null while it runs.
    #[serde(default)]
    pub finished_at: Option<Timestamp>,
    /// How many steps it was sent, and how many of them are installed.
    pub steps: usize,
    pub installed: usize,
    /// Its failed steps, in the order they ran.
    #[serde(default, deserialize_with = "failed_steps")]
    pub failed: Vec<FailedStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyState {
    Running,
    Finished,
}

/// One step that failed: the tool and version it installs, the stage of
/// installing that failed, and why, as a code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedStep {
    /// `mise`, `xcodes` or `simulators`.
    pub manager: String,
    pub tool: String,
    pub version: String,
    /// The stage of the step that failed, such as `install` or `bootstrap`.
    pub stage: String,
    /// Why, in the vocabulary of error codes, such as `tool_install_failed`.
    pub code: String,
    pub failed_at: Timestamp,
}

impl FailedStep {
    /// What the attention item is about: `<manager>:<tool>@<version>`.
    pub fn subject(&self) -> String {
        format!("{}:{}@{}", self.manager, self.tool, self.version)
    }

    fn readable(&self) -> bool {
        matches!(self.manager.as_str(), "mise" | "xcodes" | "simulators")
            && [&self.tool, &self.version].iter().all(|text| {
                !text.is_empty()
                    && text.len() <= MAXIMUM_TEXT
                    && text.chars().all(|c| c.is_ascii_graphic())
            })
            && [&self.stage, &self.code].iter().all(|text| {
                !text.is_empty()
                    && text.len() <= 64
                    && text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            })
    }
}

/// A member's report, or none when this build cannot read it.
pub fn lenient<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<ProvisioningSummary>, D::Error> {
    Ok(Option::<serde_json::Value>::deserialize(deserializer)?
        .and_then(|value| serde_json::from_value(value).ok()))
}

fn failed_steps<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<FailedStep>, D::Error> {
    let values = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .take(MAXIMUM_STEPS)
        .filter_map(|value| serde_json::from_value::<FailedStep>(value).ok())
        .filter(FailedStep::readable)
        .collect())
}

#[cfg(test)]
mod tests {
    use crate::provisioning_model::*;
    use serde_json::json;

    #[derive(Deserialize)]
    struct Status {
        #[serde(default, deserialize_with = "lenient")]
        provisioning: Option<ProvisioningSummary>,
    }

    fn step(tool: &str, code: &str) -> serde_json::Value {
        json!({"manager": "mise", "tool": tool, "version": "22.19.0", "stage": "install",
               "code": code, "failed_at": "2026-11-20T10:00:00.000Z"})
    }

    #[test]
    fn a_report_reads_leniently_and_drops_steps_it_cannot_read() {
        let status: Status = serde_json::from_value(json!({"provisioning": {
            "state": "finished", "started_at": "2026-11-20T09:00:00.000Z", "finished_at": "2026-11-20T10:00:00.000Z", "steps": 3, "installed": 1,
            "failed": [step("node", "tool_install_failed"), step("no de", "x"),
                       step("python", "Not A Code"), {"tool": "partial"}],
        }}))
        .unwrap();
        let summary = status.provisioning.unwrap();
        assert_eq!(summary.failed.len(), 1);
        assert_eq!(summary.failed[0].subject(), "mise:node@22.19.0");
        let status: Status =
            serde_json::from_value(json!({"provisioning": {"state": "paused"}})).unwrap();
        assert!(status.provisioning.is_none());
        let status: Status = serde_json::from_value(json!({})).unwrap();
        assert!(status.provisioning.is_none());
    }
}
