//! The vocabulary every crate shares: failures, the wire forms and API envelope,
//! guidance, labels, build identity, the scheduler's request model, this
//! machine's facts and what in the fleet needs attention.
pub mod api;
pub mod attention;
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
