//! Removing copies of other members' workspaces, asking their owners first over the
//! fleet, and the routes and CLI that serve and run it.
pub mod copies_api;
pub mod copies_cli;
pub mod fleet_callers;
pub mod fleet_copies;
pub mod housekeeping;
pub mod workspace_check_api;
