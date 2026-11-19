//! Local inspection, admission, installation and fleet membership, with platform and
//! transport adapters.
pub mod add_mac_cli;
pub mod admission_api;
pub mod admission_cli;
pub mod fleet_cli;
pub mod fleet_upgrade;
pub mod fleet_upgrade_api;
pub mod fleet_upgrade_local_api;
pub mod nearby_cli;
pub mod openssh_mac;
pub mod outside_pressure_api;
pub mod passthrough;
pub mod peer_api;
pub mod peer_listener;
pub mod placement_api;
pub mod placement_authority;
pub mod placement_config_sync;
pub mod placement_host;
pub mod run_relay;
pub mod server;
pub mod service_handover;
pub mod service_relaunch;
pub mod ui_app;
pub mod uninstall;
pub mod upgrade_drain;
pub mod upgrade_helper_process;

pub mod state_sizes;

pub mod trace_api;
pub mod trace_cli;

pub mod simulate_cli;
pub mod trace_snapshot;

pub mod rehearsal;
pub mod rehearsal_api;
pub mod rehearse_cli;

pub mod tune_cli;
pub mod tuning;

pub mod tuning_drift;
pub mod tuning_trust;
pub mod tuning_trust_api;

pub mod tuning_objective;

pub mod tuning_preflight;
pub mod tuning_preflight_cli;

pub mod busy_hour_calibration;

mod local_run_client;

pub mod workload_observation_cli;
