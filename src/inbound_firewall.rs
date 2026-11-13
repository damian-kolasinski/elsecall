//! Whether the operating system's firewall lets tailnet members reach the installed
//! service. Elsecall never changes firewall rules: that needs administrator rights, so
//! it reports the commands for the operator to run.
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Inbound {
    /// No firewall filters incoming connections.
    Unfiltered,
    /// The firewall has a rule that allows the executable.
    Allowed,
    /// Incoming connections from other members will be dropped until the operator
    /// runs these commands (they need administrator rights).
    Blocked { commands: Vec<String> },
    /// The firewall state could not be read.
    Unknown,
}

pub trait InboundFirewall {
    fn inbound(&self, executable: &Path) -> Inbound;
}
