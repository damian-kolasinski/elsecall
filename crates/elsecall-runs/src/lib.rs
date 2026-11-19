//! Running commands on the fleet: admitting a run, placing it on a member, running
//! it remotely.
pub mod admission_api;
pub mod admission_cli;
mod local_run_client;
pub mod passthrough;
pub mod placement_api;
pub mod placement_authority;
pub mod placement_config_sync;
pub mod placement_host;
pub mod remote_run;
pub mod run_relay;
mod sealed_run;
mod service_handover;
