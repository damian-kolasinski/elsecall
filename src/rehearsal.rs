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

pub use crate::scheduling::service::invalid_rehearsal as invalid;

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
