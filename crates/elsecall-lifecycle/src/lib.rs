//! Elsecall's own lifecycle: installing it, its window, updating it and the fleet,
//! starting and stopping the service, and the agent skill it installs.
pub mod agent_skill;
pub mod fleet_upgrade;
pub mod fleet_upgrade_api;
pub mod fleet_upgrade_local_api;
pub mod install_window;
pub mod installation;
mod lifecycle_holder;
pub mod release_update;
pub mod release_update_api;
pub mod release_update_cli;
pub mod service_handover;
pub mod service_relaunch;
pub mod upgrade_drain;
pub mod upgrade_helper_process;
pub mod window_app;
pub mod window_release;
pub mod window_startup;
