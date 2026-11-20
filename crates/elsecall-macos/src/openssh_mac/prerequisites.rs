//! Preparing the new Mac's power and Apple developer tools over SSH.

use super::{OpensshMac, quote, remote_failure};
use elsecall_contract::failure::{Failure, Result};
use std::time::Duration;
use zeroize::Zeroizing;

const TOOL_INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const CLT_HINT: &str = "On the new Mac, run xcode-select --install in Terminal, finish the installation, then try again.";

/// A MacBook running on AC can use lid-closed mode. Desktops skip it.
pub(super) async fn check_lid_closed_power(mac: &OpensshMac) -> Result<bool> {
    let report = mac
        .run("/usr/bin/pmset -g batt", Zeroizing::default())
        .await?;
    if !report.success {
        return Err(remote_failure(
            "Checking the new Mac's power source",
            &report,
        ));
    }
    let power = String::from_utf8_lossy(&report.stdout);
    match laptop_power(&power) {
        None => Ok(false),
        Some(false) => Err(Failure::new(
            "power_adapter_required",
            "Plug the new MacBook into power for lid-closed setup.",
            "Leave the lid open and click Try again once it is plugged in.",
        )),
        Some(true) => Ok(true),
    }
}

pub(super) async fn keep_awake_with_lid_closed(mac: &OpensshMac) -> Result<()> {
    if !check_lid_closed_power(mac).await? {
        return Ok(());
    }
    let changed = mac
        .sudo("/usr/bin/pmset -c sleep 0 disablesleep 1 && /usr/bin/pmset -g")
        .await?;
    let settings = String::from_utf8_lossy(&changed.stdout);
    if !changed.success
        || !settings
            .lines()
            .any(|line| line.split_whitespace().eq(["SleepDisabled", "1"]))
    {
        return Err(Failure::new(
            "closed_lid_setup_failed",
            "The new MacBook did not apply the lid-closed sleep setting.",
            "Check the sleep settings on the new Mac before closing its lid.",
        ));
    }
    Ok(())
}

/// A desktop has no internal battery, even if it is connected to a UPS.
fn laptop_power(report: &str) -> Option<bool> {
    report.contains("InternalBattery").then(|| {
        report
            .lines()
            .next()
            .is_some_and(|line| line.trim() == "Now drawing from 'AC Power'")
    })
}

pub(super) async fn ensure_command_line_tools(mac: &OpensshMac) -> Result<bool> {
    if git_available(mac).await? {
        return Ok(false);
    }
    let catalog = mac
        .run(
            "LC_ALL=C /usr/sbin/softwareupdate --list",
            Zeroizing::default(),
        )
        .await?;
    if !catalog.success {
        return Err(remote_failure(
            "Listing Apple Software Updates on the new Mac",
            &catalog,
        ));
    }
    let updates = String::from_utf8_lossy(&catalog.stdout);
    let label = command_line_tools_label(&updates).ok_or_else(|| {
        Failure::new(
            "command_line_tools_unavailable",
            "Apple Software Update offered no Command Line Tools package for the new Mac.",
            CLT_HINT,
        )
    })?;
    let installed = mac
        .sudo_within(
            &format!("/usr/sbin/softwareupdate --install {}", quote(label)),
            TOOL_INSTALL_TIMEOUT,
        )
        .await?;
    if !git_available(mac).await? {
        let message = if installed.success {
            "Apple installed Command Line Tools, but the new Mac still cannot find Git."
        } else {
            "Apple Software Update could not install Command Line Tools on the new Mac."
        };
        return Err(Failure::new("command_line_tools_failed", message, CLT_HINT));
    }
    Ok(true)
}

async fn git_available(mac: &OpensshMac) -> Result<bool> {
    // Invoking the Git shim directly on a fresh Mac can open an installer prompt.
    let probe = mac
        .run(
        "/usr/bin/xcode-select -p >/dev/null 2>&1 && git=$(/usr/bin/xcrun --find git 2>/dev/null) && \"$git\" --version >/dev/null 2>&1",
            Zeroizing::default(),
        )
        .await?;
    Ok(probe.success)
}

/// Software Update can offer several compatible Command Line Tools releases.
fn command_line_tools_label(catalog: &str) -> Option<&str> {
    catalog
        .lines()
        .filter_map(|line| {
            let label = line.trim().strip_prefix("* Label: ")?;
            let rest = label.strip_prefix("Command Line Tools for Xcode")?;
            let (_, version) = rest.rsplit_once('-')?;
            let version = version
                .split('.')
                .map(str::parse::<u32>)
                .collect::<std::result::Result<Vec<_>, _>>()
                .ok()?;
            Some((version, label))
        })
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, label)| label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_the_latest_listed_command_line_tools() {
        let available = "\
            * Label: macOS Tahoe 26.9-26.9\n\
            * Label: Command Line Tools for Xcode 26.5-26.5\n\
            * Label: Command Line Tools for Xcode 26.6-26.6\n\
            * Label: Command Line Tools for Xcode-26.10\n";
        assert_eq!(
            command_line_tools_label(available),
            Some("Command Line Tools for Xcode-26.10")
        );
        assert_eq!(
            command_line_tools_label("* Label: macOS Tahoe 26.9-26.9"),
            None
        );
    }

    #[test]
    fn detects_ac_power_only_for_a_laptop() {
        assert_eq!(
            laptop_power("Now drawing from 'AC Power'\n -InternalBattery-0"),
            Some(true)
        );
        assert_eq!(
            laptop_power("Now drawing from 'Battery Power'\n -InternalBattery-0"),
            Some(false)
        );
        assert_eq!(laptop_power("Now drawing from 'AC Power'\n -UPS-0"), None);
    }
}
