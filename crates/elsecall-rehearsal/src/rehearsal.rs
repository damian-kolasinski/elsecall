//! Controlled synthetic fleet execution. Scheduling remains in normal `run`.
mod caller;
pub mod coverage;
pub mod inputs;
mod journal;
pub mod member;
pub mod plan;
pub mod protocol;
mod scratch;
pub mod window;
pub mod work;
pub mod worker;
mod workspace;

pub use elsecall_scheduling::service::invalid_rehearsal as invalid;

pub mod cancellation;
pub use elsecall_workload::recording;

/// Only known service-owned labels with internal placement are background in
/// synthetic workloads. Never infer ownership from a consumer label prefix.
pub use elsecall_workload::trace::internal;
