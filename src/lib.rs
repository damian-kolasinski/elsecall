//! Local inspection, admission, installation and fleet membership, with platform and
//! transport adapters.
pub mod add_mac;
pub mod add_mac_cli;
pub mod admission_api;
pub mod admission_cli;
pub mod askpass;
pub mod authorized_keys;
pub mod config_sync;
pub mod config_sync_api;
pub mod config_sync_cli;
pub mod config_sync_local_api;
pub mod configuration;
pub mod copies_api;
pub mod copies_cli;
pub mod fleet;
pub mod fleet_callers;
pub mod fleet_changes;
pub mod fleet_cli;
pub mod fleet_copies;
pub mod fleet_local_api;
pub mod fleet_placement;
pub mod fleet_tool_inventory;
pub mod fleet_upgrade;
pub mod fleet_upgrade_api;
pub mod fleet_upgrade_local_api;
pub mod fleet_view;
pub mod housekeeping;
pub mod inbound_firewall;
pub mod install_window;
pub mod installation;
pub mod inventory_reader;
pub mod lan_address;
pub mod lifecycle_holder;
pub mod load_log;
pub mod machine_script;
pub mod macos;
pub mod mise_cli;
pub mod nearby_cli;
pub mod nearby_macs;
pub mod openssh_mac;
pub mod outside_pressure;
pub mod outside_pressure_api;
pub mod outside_pressure_gate;
pub mod own_history;
pub mod passthrough;
pub mod peer_api;
pub mod peer_client;
pub mod peer_history;
pub mod peer_listener;
pub mod placement_api;
pub mod placement_authority;
pub mod placement_config_sync;
pub mod placement_host;
pub mod process_service;
pub mod remote_mac;
pub mod remote_run;
pub mod replay_api;
pub mod replay_cli;
pub mod report_api;
pub mod report_cli;
pub mod report_protocol;
pub mod reporting;
pub mod run_relay;
pub mod scheduling;
pub mod server;
pub mod service_configuration;
pub mod service_handover;
pub mod service_relaunch;
pub mod tool_inventory;
pub mod tools_api;
pub mod tools_cli;
pub mod toolset;
pub mod toolset_api;
pub mod ui_app;
pub mod uninstall;
pub mod upgrade_drain;
pub mod upgrade_helper_process;
pub mod window_app;
pub mod window_release;
pub mod window_startup;
pub mod workload_settings;
pub mod workspace;
pub mod workspace_check_api;

pub mod service_log;

pub mod state_sizes;

pub mod trace_api;
pub mod trace_cli;
pub mod workload;

pub mod simulate_cli;
pub mod simulation;
pub mod trace_snapshot;

pub mod rehearsal;
pub mod rehearsal_api;
pub mod rehearse_cli;

pub mod tune_cli;
pub mod tuning;

mod evaluation_day;

pub mod tuning_drift;
pub mod tuning_trust;
pub mod tuning_trust_api;

pub mod tuning_objective;

pub mod tuning_preflight;
pub mod tuning_preflight_cli;

pub mod replay_load;

pub mod busy_hour_calibration;

pub mod replay_placement;

pub mod peer_diagnostics;

mod peer_transport;

mod local_run_client;
mod receive_inactivity;
mod sealed_run;
mod socket_io;

mod send_inactivity;

pub mod workload_metadata;
pub mod workload_observation_cli;

pub mod release_update;
pub mod release_update_api;
pub mod release_update_cli;
pub mod workload_configuration;
pub mod workload_migration;
