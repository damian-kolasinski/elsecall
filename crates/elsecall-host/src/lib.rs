//! This machine's local plumbing: starting child processes, the state directory and
//! its private files, SQLite retention, the service log, the client for the local
//! service, and scripted machine observations for tests.
pub mod child_start;
pub mod client;
pub mod command_capture;
pub mod local_access;
pub mod machine_script;
pub mod paths;
pub mod private_files;
pub mod run_id_file;
pub mod service_log;
pub mod sqlite_retention;
pub mod state_temps;
