//! The fleet: its members and their authority, adding a Mac to it, and the paths it
//! may mirror between members.
pub mod add_mac;
pub mod askpass;
mod authority;
pub mod authorized_keys;
pub mod config_sync_model;
pub mod fleet;
mod fleet_changes;
pub mod fleet_local_api;
pub mod fleet_placement;
pub mod fleet_view;
pub mod inbound_firewall;
pub mod lan_address;
pub mod nearby_macs;
pub mod remote_mac;
