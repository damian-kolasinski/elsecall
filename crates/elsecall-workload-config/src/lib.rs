//! The workload configuration a consumer project writes: its label rules, with
//! their urgency, placement and outputs, the file they live in, and migrating it
//! from older forms. Runs read it, tuning simulates it and the desktop app edits
//! it, so it stays apart from all three.
pub mod workload_configuration;
pub mod workload_migration;
pub mod workload_settings;
