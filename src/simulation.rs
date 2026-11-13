//! Deterministic adapters around the production scheduler. No scheduling policy
//! lives here: requests, quotes, offers, expiry and learning use `Scheduler`.
mod adapters;
mod channel;
mod config_sync;
mod engine;
mod execution;
pub mod fidelity;
mod initial;
mod lingering;
mod member;
mod metrics;
mod model;
mod objective;
mod observation;
mod processes;
pub mod rehearsal_cpu;
mod request_ids;
mod reserve_ledger;
mod settings;
mod timeline;
mod validation;
mod workspace;
mod world;

pub(crate) use engine::invalid as engine_invalid;
pub use engine::{Options, simulate};

/// Shared untrusted workload boundary for offline simulation and rehearsal.
pub fn validate_trace(trace: &crate::workload::trace::Trace) -> crate::failure::Result<()> {
    validation::trace(trace)
}

pub(crate) use settings::Settings as CandidateSettings;

mod observed;
pub use observed::summarize as observed_metrics;

pub(crate) use model::{freeze_reference, structural_members};

pub(crate) use objective::constrain_failures;
