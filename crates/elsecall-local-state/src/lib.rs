//! Where Elsecall keeps its state on this machine, files only this user may read,
//! and access to the local service.
pub mod local_access;
pub mod paths;
pub mod private_files;
pub mod run_id_file;
pub mod state_temps;
