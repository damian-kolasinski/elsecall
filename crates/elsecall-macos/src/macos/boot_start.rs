//! A root LaunchDaemon that starts an account's background-session service at
//! boot, before anyone logs in.
//!
//! launchd loads `~/Library/LaunchAgents` only into a desktop (Aqua) login, which
//! skips an agent limited to the `Background` session, and nothing loads that
//! agent into the account's `user/<uid>` domain at boot. Without this daemon, a
//! member running in the background session stays down after a restart until
//! someone bootstraps its service again, and stays down even after a desktop
//! login.
//!
//! The daemon, one per account, runs once at boot as that account (`UserName`)
//! and runs only `/bin/launchctl bootstrap user/<uid> <agent>`: what the account
//! could do itself, so root grants it nothing and never executes anything the
//! account controls. The agent is the installation's own LaunchAgent link, read
//! as data. That domain refuses an agent for the desktop session, so the daemon
//! does nothing for one: the desktop login loads it instead. After an uninstall
//! it finds no agent and does nothing.
use elsecall_contract::failure::{Failure, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

const DIRECTORY: &str = "/Library/LaunchDaemons";
const LABEL_PREFIX: &str = "dev.elsecall.boot.";

/// The boot starter for one account's installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootStart {
    account: String,
    uid: u32,
    agent: PathBuf,
}

/// Whether the account's boot starter is in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Installed as this build writes it.
    Installed,
    /// Installed for this installation, but not as this build writes it.
    Outdated,
    /// Installed for another installation of the account, under another home;
    /// it stays that installation's.
    Foreign,
    Missing,
}

impl BootStart {
    /// The starter for `account` (with user ID `uid`), whose installation's
    /// LaunchAgent link is `agent`.
    pub(crate) fn new(account: &str, uid: u32, agent: &Path) -> Result<Self> {
        // A macOS short name; it becomes part of the label and the file name.
        let plain = !account.is_empty()
            && !account.starts_with(['.', '-'])
            && account
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
        if !plain || uid == 0 || !agent.is_absolute() {
            return Err(Failure::new(
                "invalid_request",
                "The account must be a regular account with a plain short name.",
                "Install Elsecall from a regular macOS account, not root.",
            ));
        }
        Ok(Self {
            account: account.to_owned(),
            uid,
            agent: agent.to_owned(),
        })
    }

    /// The starter for the account running this process.
    pub fn for_this_account(agent: &Path) -> Result<Self> {
        let uid = elsecall_host::local_access::user_id();
        Self::new(&account_name(uid)?, uid, agent)
    }

    pub fn label(&self) -> String {
        format!("{LABEL_PREFIX}{}", self.account)
    }

    pub fn plist(&self) -> PathBuf {
        Path::new(DIRECTORY).join(format!("{}.plist", self.label()))
    }

    fn dictionary(&self) -> plist::Dictionary {
        let mut values = plist::Dictionary::new();
        values.insert("Label".into(), self.label().into());
        values.insert("UserName".into(), self.account.clone().into());
        values.insert(
            "ProgramArguments".into(),
            plist::Value::Array(vec![
                "/bin/launchctl".into(),
                "bootstrap".into(),
                format!("user/{}", self.uid).into(),
                self.agent.to_string_lossy().into_owned().into(),
            ]),
        );
        values.insert("RunAtLoad".into(), true.into());
        values.insert("StandardOutPath".into(), "/dev/null".into());
        values.insert("StandardErrorPath".into(), "/dev/null".into());
        values
    }

    /// The LaunchDaemon's configuration.
    fn configuration(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        plist::Value::Dictionary(self.dictionary())
            .to_writer_xml(&mut bytes)
            .map_err(|_| {
                Failure::new(
                    "configuration_error",
                    "The boot starter's configuration could not be written.",
                    "Report this; the LaunchAgent path could not be encoded.",
                )
            })?;
        Ok(bytes)
    }

    /// A root shell script that installs the starter and loads it, which also
    /// starts a background-session service that is not running. It prints
    /// `installed` when it changed the starter and `unchanged` otherwise.
    pub fn install_script(&self) -> Result<String> {
        Ok(super::launch_daemon::install_script(
            &self.label(),
            &self.plist().to_string_lossy(),
            &self.configuration()?,
        ))
    }

    /// Whether `install_script`'s output says it changed the starter.
    pub fn installed(stdout: &[u8]) -> Option<bool> {
        super::launch_daemon::installed(stdout)
    }

    pub fn state(&self) -> State {
        self.state_in(Path::new(DIRECTORY))
    }

    fn state_in(&self, directory: &Path) -> State {
        let path = directory.join(format!("{}.plist", self.label()));
        let Ok(bytes) = std::fs::read(path) else {
            return State::Missing;
        };
        // Compared as values, so a change in how the plist is written never
        // asks for root again.
        let installed = plist::Value::from_reader_xml(bytes.as_slice()).ok();
        let installed = installed.as_ref().and_then(plist::Value::as_dictionary);
        if installed == Some(&self.dictionary()) {
            return State::Installed;
        }
        let agent = installed.and_then(|values| {
            values
                .get("ProgramArguments")?
                .as_array()?
                .last()?
                .as_string()
        });
        match agent {
            Some(agent) if Path::new(agent) != self.agent => State::Foreign,
            _ => State::Outdated,
        }
    }

    /// The commands an administrator runs to remove the starter.
    pub fn removal(&self) -> [String; 2] {
        super::launch_daemon::removal(&self.label(), &self.plist().to_string_lossy())
    }
}

/// The short name of the account with user ID `uid`.
fn account_name(uid: u32) -> Result<String> {
    let mut buffer = vec![0 as libc::c_char; 4096];
    // SAFETY: passwd is plain data; getpwuid_r writes only into `entry` and
    // `buffer`, within the length given, and sets `found` to `entry` or null.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() || entry.pw_name.is_null() {
        return Err(Failure::new(
            "account_unknown",
            "This account's short name could not be read.",
            "Run elsecall doctor; check the account with id -un.",
        ));
    }
    // SAFETY: getpwuid_r succeeded, so pw_name points at a NUL-terminated string
    // inside `buffer`, which is still alive.
    let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
    Ok(name.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devbox() -> BootStart {
        BootStart::new(
            "devbox",
            502,
            Path::new("/Users/devbox/Library/LaunchAgents/dev.elsecall.agent.plist"),
        )
        .unwrap()
    }

    #[test]
    fn the_starter_bootstraps_the_agent_as_the_account() {
        let start = devbox();
        assert_eq!(start.label(), "dev.elsecall.boot.devbox");
        assert_eq!(
            start.plist(),
            Path::new("/Library/LaunchDaemons/dev.elsecall.boot.devbox.plist")
        );
        let bytes = start.configuration().unwrap();
        let value = plist::Value::from_reader_xml(bytes.as_slice()).unwrap();
        let config = value.as_dictionary().unwrap();
        assert_eq!(
            config["Label"].as_string(),
            Some("dev.elsecall.boot.devbox")
        );
        assert_eq!(config["UserName"].as_string(), Some("devbox"));
        assert_eq!(config["RunAtLoad"].as_boolean(), Some(true));
        assert!(!config.contains_key("KeepAlive"), "it runs once per boot");
        let arguments: Vec<&str> = config["ProgramArguments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_string().unwrap())
            .collect();
        assert_eq!(
            arguments,
            [
                "/bin/launchctl",
                "bootstrap",
                "user/502",
                "/Users/devbox/Library/LaunchAgents/dev.elsecall.agent.plist"
            ]
        );
        let script = start.install_script().unwrap();
        assert!(script.contains("plist='/Library/LaunchDaemons/dev.elsecall.boot.devbox.plist'"));
        assert!(script.contains("system/dev.elsecall.boot.devbox"));
        assert_eq!(
            start.removal(),
            [
                "sudo launchctl bootout system/dev.elsecall.boot.devbox",
                "sudo rm /Library/LaunchDaemons/dev.elsecall.boot.devbox.plist"
            ]
        );
    }

    #[test]
    fn only_plain_regular_accounts_get_a_starter() {
        let agent = Path::new("/Users/me/Library/LaunchAgents/dev.elsecall.agent.plist");
        assert!(BootStart::new("me", 501, agent).is_ok());
        assert!(BootStart::new("first.last", 501, agent).is_ok());
        for account in ["", "-x", ".x", "a b", "a/b", "a'b", "ü"] {
            assert!(BootStart::new(account, 501, agent).is_err(), "{account:?}");
        }
        assert!(BootStart::new("root", 0, agent).is_err());
        assert!(BootStart::new("me", 501, Path::new("relative.plist")).is_err());
    }

    #[test]
    fn the_state_compares_the_installed_starter_with_this_builds() {
        let directory = tempfile::tempdir().unwrap();
        let start = devbox();
        assert_eq!(start.state_in(directory.path()), State::Missing);
        let file = directory.path().join("dev.elsecall.boot.devbox.plist");
        std::fs::write(&file, start.configuration().unwrap()).unwrap();
        assert_eq!(start.state_in(directory.path()), State::Installed);
        // The same values, written differently, are still this build's.
        let mut reordered = Vec::new();
        let mut values: Vec<_> = start.dictionary().into_iter().collect();
        values.reverse();
        plist::Value::Dictionary(values.into_iter().collect())
            .to_writer_xml(&mut reordered)
            .unwrap();
        std::fs::write(&file, reordered).unwrap();
        assert_eq!(start.state_in(directory.path()), State::Installed);
        // Another installation's starter is not this one's to replace or remove.
        let other = BootStart::new(
            "devbox",
            502,
            Path::new("/Users/me/Library/LaunchAgents/dev.elsecall.agent.plist"),
        )
        .unwrap();
        assert_eq!(other.state_in(directory.path()), State::Foreign);
        // One this installation's agent runs differently is outdated.
        let mut changed = start.dictionary();
        changed.insert("RunAtLoad".into(), false.into());
        let mut bytes = Vec::new();
        plist::Value::Dictionary(changed)
            .to_writer_xml(&mut bytes)
            .unwrap();
        std::fs::write(&file, bytes).unwrap();
        assert_eq!(start.state_in(directory.path()), State::Outdated);
        std::fs::write(&file, "not a plist").unwrap();
        assert_eq!(start.state_in(directory.path()), State::Outdated);
    }

    #[test]
    fn this_account_has_a_name() {
        let start = BootStart::for_this_account(Path::new("/Users/me/agent.plist")).unwrap();
        assert!(start.label().starts_with(LABEL_PREFIX));
    }
}
