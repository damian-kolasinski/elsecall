//! Starting a background-session service at boot: `elsecall service
//! start-at-boot`, which installs the account's root boot starter through sudo,
//! and the starter's state as install, doctor and uninstall report it.
use elsecall_contract::failure::{Failure, Result};
use elsecall_host::paths::Paths;
use elsecall_lifecycle::installation::{self, ServiceManager, Session};
use elsecall_macos::macos::{
    boot_start::{BootStart, State},
    sudo::{self, Password},
};
use serde_json::{Value, json};
use std::io::IsTerminal;

/// What a background-session installation without its starter needs.
pub const NEXT_STEP: &str = "launchd does not start a background-session service at boot: after a restart it stays down, even after a desktop login, until someone runs elsecall service restart. Run elsecall service start-at-boot here once (it asks for an administrator's password), or Add Mac for this Mac again from the master.";

/// The account's starter, or `None` when the account cannot have one.
fn starter(paths: &Paths) -> Option<BootStart> {
    BootStart::for_this_account(&paths.agent).ok()
}

/// The starter's state; `None` when the account cannot have one.
pub fn state(paths: &Paths) -> Option<State> {
    starter(paths).map(|start| start.state())
}

/// Whether an installation in `session` stays down after a restart for want of
/// a starter `start-at-boot` can install. Another installation's starter is not
/// this one's to replace.
pub fn missing(session: Option<Session>, state: Option<State>) -> bool {
    session == Some(Session::Background) && matches!(state, Some(State::Missing | State::Outdated))
}

/// A starter an uninstall leaves behind: it needs root to remove, and does
/// nothing without an installation.
pub fn kept(paths: &Paths) -> Option<Value> {
    let start = starter(paths)?;
    matches!(start.state(), State::Installed | State::Outdated).then(|| {
        json!({
            "plist": start.plist(),
            "commands": start.removal(),
            "hint": "The boot starter stays: removing it needs an administrator. Without an installation it does nothing at boot; remove it with these commands, or keep it for a later install.",
        })
    })
}

/// Installs the starter, through sudo unless it is already in place. sudo asks
/// on a terminal; a program pipes the password in on standard input.
pub fn run(paths: &Paths, manager: &dyn ServiceManager) -> Result<Value> {
    if !installation::installed(paths)? {
        return Err(Failure::new(
            "not_installed",
            "Elsecall is not installed in this account, so there is no service to start at boot.",
            "Run elsecall install first.",
        ));
    }
    let start = BootStart::for_this_account(&paths.agent)?;
    let session = installation::session(paths, manager);
    // Refused before asking for a password the starter would not use.
    if session == Some(Session::Desktop) {
        return Err(Failure::new(
            "desktop_session",
            "The service runs in the desktop session, which starts at desktop login; the boot starter does nothing for it.",
            "To run without a login, run elsecall install --session background first, then this.",
        ));
    }
    let state = start.state();
    if state == State::Foreign {
        return Err(Failure::new(
            "boot_start_foreign",
            "This account's boot starter belongs to another Elsecall installation, under another home.",
            "Leave it to that installation, or remove it with the commands in details.commands and run this again.",
        )
        .with_details(json!({ "plist": start.plist(), "commands": start.removal() })));
    }
    let changed = if state == State::Installed {
        false
    } else {
        let stdin = std::io::stdin();
        let password = if stdin.is_terminal() {
            Password::Terminal {
                why: "sudo asks for an administrator's password to install the boot starter.",
            }
        } else {
            Password::Given(crate::piped_password::read(&mut stdin.lock(), false)?)
        };
        let stdout = sudo::run(&start.install_script()?, password)?;
        BootStart::installed(&stdout).ok_or_else(|| {
            Failure::new(
                "sudo_failed",
                "Installing the boot starter did not report its result.",
                "Run elsecall doctor and check boot_start.",
            )
        })?
    };
    Ok(json!({
        "label": start.label(),
        "plist": start.plist(),
        "changed": changed,
        "boot_start": start.state(),
        "session": session,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_background_session_without_its_starter_misses_boot() {
        let background = Some(Session::Background);
        assert!(missing(background, Some(State::Missing)));
        assert!(missing(background, Some(State::Outdated)));
        assert!(!missing(background, Some(State::Foreign)));
        assert!(!missing(background, None));
        assert!(!missing(background, Some(State::Installed)));
        assert!(!missing(Some(Session::Desktop), Some(State::Missing)));
        assert!(!missing(None, Some(State::Missing)));
    }
}
