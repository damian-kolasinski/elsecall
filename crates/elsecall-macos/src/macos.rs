//! macOS boundary adapters.
pub mod application_firewall;
pub mod boot_start;
pub mod core_benchmark;
pub mod dns_sd;
pub mod firewall_guard;
pub mod lan_addresses;
pub mod launch_agent;
mod launch_daemon;
pub mod local_host_name;
pub mod machine;
pub mod process_sampler;
pub mod sudo;
mod sysctl;
pub mod tailscale;
mod thermal;
pub mod window_launcher;
