//! Local socket adapter for reviewing and approving file sync.

use elsecall_fleet::config_sync_model::{consent::SecretFile, status::FileReport};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
pub(crate) struct Status {
    pub(crate) master: String,
    pub(crate) local_member: String,
    pub(crate) declared_here: Vec<String>,
    pub(crate) members: Vec<MemberStatus>,
}

#[derive(Deserialize)]
pub(crate) struct MemberStatus {
    pub(crate) member: String,
    pub(crate) sync: Option<Summary>,
}

#[derive(Deserialize)]
pub(crate) struct Summary {
    pub(crate) paths: Vec<PathStatus>,
    pub(crate) applying: bool,
    #[serde(default)]
    pub(crate) paused: Vec<String>,
    pub(crate) service: ServiceStatus,
}

#[derive(Deserialize)]
pub(crate) struct PathStatus {
    pub(crate) path: String,
    pub(crate) differences: Vec<FileReport>,
}

#[derive(Deserialize)]
pub(crate) struct ServiceStatus {
    pub(crate) pending: bool,
}

#[derive(Deserialize)]
pub(crate) struct Plan {
    pub(crate) plan_id: String,
    pub(crate) paths: Vec<PlannedPath>,
    pub(crate) members: Vec<MemberPlan>,
}

#[derive(Deserialize)]
pub(crate) struct PlannedPath {
    pub(crate) path: String,
    pub(crate) enabled: bool,
    pub(crate) present: bool,
    pub(crate) files: usize,
    #[serde(default)]
    pub(crate) secret_looking: Vec<SecretFile>,
    #[serde(default)]
    pub(crate) skipped: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct MemberPlan {
    pub(crate) member: String,
    pub(crate) error: Option<elsecall_contract::failure::Failure>,
    pub(crate) paths: Vec<PathPlan>,
}

#[derive(Deserialize)]
pub(crate) struct PathPlan {
    pub(crate) files: Vec<FileReport>,
    pub(crate) unchanged: usize,
    pub(crate) error: Option<elsecall_contract::failure::Failure>,
}

#[derive(Deserialize)]
struct Enabled {
    round: Round,
}

#[derive(Deserialize)]
struct Round {
    complete: bool,
}

pub(crate) fn status() -> Result<Status, String> {
    post("/v1/sync", &json!({}))
}

pub(crate) fn review(path: &str) -> Result<Plan, String> {
    post("/v1/sync/plan", &json!({ "paths": [path] }))
}

pub(crate) fn declare(path: &str) -> Result<(), String> {
    post("/v1/sync/declare", &json!(path))
}

pub(crate) fn enable(path: &str, plan_id: &str) -> Result<bool, String> {
    post::<Enabled>(
        "/v1/sync/enable",
        &json!({ "path": path, "confirm": plan_id }),
    )
    .map(|enabled| enabled.round.complete)
}

fn post<T: serde::de::DeserializeOwned>(route: &str, body: &Value) -> Result<T, String> {
    let message = |failure: elsecall_contract::failure::Failure| {
        format!("{} {}", failure.message, failure.hint)
    };
    let socket = elsecall_host::paths::Paths::current()
        .map_err(message)?
        .socket();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "The configuration sync connection could not start.".to_owned())?;
    runtime.block_on(async {
        tokio::time::timeout(
            std::time::Duration::from_secs(150),
            elsecall_host::client::post_large(&socket, route, body),
        )
        .await
        .map_err(|_| "The sync request timed out. Reload to check its status.".to_owned())?
        .map_err(message)
    })
}
