//! Local inspection, admission, installation and fleet membership, with platform and
//! transport adapters.
pub mod add_mac_cli;
pub mod fleet_cli;
pub mod nearby_cli;
pub mod outside_pressure_api;
pub mod peer_api;
pub mod peer_listener;
pub mod server;
pub mod ui_app;
pub mod uninstall;

pub mod state_sizes;

pub mod simulate_cli;

pub mod rehearse_cli;

pub mod tune_cli;
pub mod tuning;

pub mod tuning_drift;

pub mod tuning_preflight;
pub mod tuning_preflight_cli;

pub mod busy_hour_calibration;

pub mod rehearsal_compare;
pub mod workload_observation_cli;
