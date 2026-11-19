//! Bounded, master-authorized rehearsal operations; never arbitrary commands.
use elsecall_contract::scheduling_model::{
    capacity::Cores,
    request::{Placement, Urgency},
    timestamp::Timestamp,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub index: usize,
    pub workspace: String,
    pub label: String,
    pub size: Cores,
    pub placement: Placement,
    pub urgency: Urgency,
    pub hold_ms: Option<u64>,
    pub duration_ms: u64,
    #[serde(default)]
    pub work: Option<super::work::Work>,
    pub demand: BTreeMap<String, f64>,
    pub exit_code: i32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Certify {
        certificate: Box<crate::tuning_trust::Certificate>,
    },
    Trust,
    Inspect {
        since: Timestamp,
    },
    Prepare {
        session: String,
        limit_ms: u64,
        workspaces: Vec<String>,
        seed: u64,
    },
    RecordedLoad {
        session: String,
        since: Timestamp,
        until: Timestamp,
    },
    Launch {
        session: String,
        request: Launch,
    },
    Poll {
        session: String,
    },
    Stop {
        session: String,
    },
    Clean {
        session: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub member: String,
    pub operation: Action,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub index: usize,
    pub run_id: Option<String>,
    pub exit_code: Option<i32>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Reply {
    pub member: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<serde_json::Value>,
    pub cores: Option<elsecall_contract::inspection::CoreCounts>,
    #[serde(default)]
    pub calibration: Option<super::worker::Calibration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<elsecall_scheduling::records::SchedulerSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_load: Option<elsecall_load_log::session::Recorded>,
    #[serde(default)]
    pub observations_ready: Option<bool>,
    pub production_active: usize,
    pub production_arrivals: usize,
    pub jobs: Vec<Job>,
    pub scratch: Option<String>,
    pub clean: bool,
}
pub use elsecall_contract::rehearsal_identity::identity::{
    scratch_of, valid_session, valid_workspace,
};
