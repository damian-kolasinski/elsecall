//! Keeping members alike: syncing configuration between them, and this member's
//! service configuration, which the service relaunches to apply once it is idle.
pub mod config_sync;
pub mod config_sync_api;
pub mod config_sync_cli;
pub mod config_sync_local_api;
pub mod configuration;
pub mod service_configuration;
pub mod service_relaunch;
