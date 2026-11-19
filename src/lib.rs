//! Local inspection, admission, installation and fleet membership, with platform and
//! transport adapters.
pub mod add_mac_cli;
mod fleet_callers;
pub mod fleet_cli;
mod fleet_copies;
pub mod nearby_cli;
pub mod outside_pressure_api;
pub mod peer_api;
pub mod peer_listener;
pub mod server;
pub mod ui_app;
pub mod uninstall;
mod workspace_check_api;

pub mod state_sizes;

pub mod simulate_cli;

pub mod rehearse_cli;

pub mod rehearsal_compare;
pub mod workload_observation_cli;
