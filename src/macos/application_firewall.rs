//! The macOS Application Firewall, read through `socketfilterfw` without privileges.
//!
//! The firewall keeps a rule per executable. Elsecall's binary has no Developer ID
//! signature, so it is not allowed automatically; loopback is never filtered.
use crate::inbound_firewall::{Inbound, InboundFirewall};
use std::{path::Path, process::Command};

const TOOL: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";

pub struct ApplicationFirewall;
impl ApplicationFirewall {
    fn query(&self, arguments: &[&str]) -> Option<String> {
        let output = crate::child_start::output(Command::new(TOOL).args(arguments)).ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }
}
impl InboundFirewall for ApplicationFirewall {
    fn inbound(&self, executable: &Path) -> Inbound {
        let (Some(state), Some(block_all), Some(apps)) = (
            self.query(&["--getglobalstate"]),
            self.query(&["--getblockall"]),
            self.query(&["--listapps"]),
        ) else {
            return Inbound::Unknown;
        };
        verdict(&state, &block_all, &apps, executable)
    }
}

fn verdict(state: &str, block_all: &str, apps: &str, executable: &Path) -> Inbound {
    if state.contains("disabled") || state.contains("State = 0") {
        return Inbound::Unfiltered;
    }
    let path = executable.to_string_lossy();
    if !block_all.contains("enabled") && allowed(apps, &path) {
        return Inbound::Allowed;
    }
    let quoted = format!("'{}'", path.replace('\'', r"'\''"));
    let mut commands = vec![
        format!("sudo {TOOL} --add {quoted}"),
        format!("sudo {TOOL} --unblockapp {quoted}"),
    ];
    if block_all.contains("enabled") {
        commands.insert(0, format!("sudo {TOOL} --setblockall off"));
    }
    Inbound::Blocked { commands }
}

/// Whether `--listapps` shows an allow rule for exactly this path. Each rule is a
/// numbered line with the path, followed by a line with its verdict.
fn allowed(apps: &str, path: &str) -> bool {
    let mut lines = apps.lines();
    while let Some(line) = lines.next() {
        let listed = line.split_once(" : ").map(|(_, rest)| rest.trim());
        if listed == Some(path) {
            return lines
                .next()
                .is_some_and(|verdict| verdict.contains("Allow incoming connections"));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPS: &str = "ALF: total number of apps = 2 \n\n1 :  /usr/bin/python3 \n \t (Allow incoming connections) \n2 : /Users/a b/Library/Application Support/Elsecall/elsecall \n \t (Block incoming connections) \n";

    #[test]
    fn reads_rules_for_the_exact_executable() {
        let enabled = "Firewall is enabled. (State = 1)";
        let off = "Firewall has block all state set to disabled.";
        let python = Path::new("/usr/bin/python3");
        assert_eq!(verdict(enabled, off, APPS, python), Inbound::Allowed);
        let elsecall = Path::new("/Users/a b/Library/Application Support/Elsecall/elsecall");
        let Inbound::Blocked { commands } = verdict(enabled, off, APPS, elsecall) else {
            panic!("a block rule blocks");
        };
        assert_eq!(
            commands[0],
            format!("sudo {TOOL} --add '/Users/a b/Library/Application Support/Elsecall/elsecall'")
        );
        assert!(matches!(
            verdict(enabled, off, APPS, Path::new("/usr/bin/python")),
            Inbound::Blocked { .. }
        ));
        assert_eq!(
            verdict("Firewall is disabled. (State = 0)", off, APPS, elsecall),
            Inbound::Unfiltered
        );
        let Inbound::Blocked { commands } = verdict(
            enabled,
            "Firewall has block all state set to enabled.",
            APPS,
            python,
        ) else {
            panic!("block all blocks everything");
        };
        assert!(commands[0].ends_with("--setblockall off"));
    }
}
