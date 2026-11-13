//! Controlled synthetic fleet execution. Scheduling remains in normal `run`.
mod caller;
pub(crate) mod coverage;
pub(crate) mod inputs;
mod journal;
pub mod member;
pub mod plan;
pub mod protocol;
mod scratch;
pub mod window;
pub mod work;
pub mod worker;
mod workspace;

pub const SESSION_LABEL_EXTRA: usize = 25;
pub const PREFIX: &str = "rehearsal:";
pub fn synthetic(label: Option<&str>) -> bool {
    label.is_some_and(|label| label.starts_with(PREFIX))
}
pub fn label_limit(label: &str) -> usize {
    crate::scheduling::request::MAXIMUM_LABEL
        + if synthetic(Some(label)) {
            PREFIX.len() + SESSION_LABEL_EXTRA
        } else {
            0
        }
}
pub fn invalid(message: impl Into<String>) -> crate::failure::Failure {
    crate::failure::Failure::new(
        "invalid_rehearsal",
        message,
        "Check the trace and rehearsal options; no production settings are changed.",
    )
}

pub fn belongs(session: &str, label: Option<&str>) -> bool {
    label.is_some_and(|l| l.starts_with(&format!("{PREFIX}{session}:")))
}

pub mod cancellation;
pub mod compare;
pub mod load_recording;
pub mod recording;

/// Only known service-owned labels with internal placement are background in
/// synthetic workloads. Never infer ownership from a consumer label prefix.
pub fn internal(request: &crate::workload::trace::Request) -> bool {
    request.fact.internal
        || matches!(
            request.fact.label.as_deref(),
            Some("elsecall housekeeping" | "elsecall fidelity drift")
        ) && (request.placement_request == crate::scheduling::request::Placement::Pinned
            || request.fact.deferrable)
}
