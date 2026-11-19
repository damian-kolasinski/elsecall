//! The vocabulary every crate shares: failures, the wire forms and API envelope,
//! guidance, labels, build identity, the scheduler's request model and this
//! machine's facts.
pub mod api;
pub mod build_identity;
pub mod executable_format;
pub mod failure;
pub mod guidance;
pub mod idle_load;
pub mod inspection;
pub mod labels;
pub mod rehearsal_identity;
pub mod run_admission;
pub mod scheduling_model;
pub mod version_skew;
pub mod wire;
