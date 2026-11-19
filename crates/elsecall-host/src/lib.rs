//! This machine's local plumbing: starting child processes and knowing the operating
//! system's, the state directory and its private files, the lock lifecycle
//! operations hold, SQLite retention, the service log, the client for the local
//! service, and scripted machine observations for tests.
pub mod child_start;
pub mod client;
pub mod command_capture;
pub mod lifecycle_holder;
pub mod local_access;
pub mod machine_script;
pub mod os_processes;
pub mod paths;
pub mod private_files;
pub mod processes;
pub mod run_id_file;
pub mod service_log;
pub mod sqlite_retention;
pub mod state_temps;
