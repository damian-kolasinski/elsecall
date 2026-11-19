//! Adding a Mac to the fleet: finding nearby Macs, reaching one over Remote Login,
//! trusting its key and opening its firewall for the service.
pub mod add_mac;
pub mod askpass;
pub mod authorized_keys;
pub mod inbound_firewall;
pub mod lan_address;
pub mod nearby_macs;
pub mod remote_mac;
