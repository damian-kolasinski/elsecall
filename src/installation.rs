//! Installation policy; the service manager is supplied at the system boundary.
use crate::{
    failure::{Failure, Result},
    paths::Paths,
    private_files,
};
use serde::Serialize;
use std::{fs, os::unix::fs::symlink, path::Path};

/// Where the service runs within the user's account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Session {
    /// Starts at the user's desktop login and stops at logout.
    Desktop,
    /// Runs without a desktop login, across ssh disconnects, for accounts such as
    /// agent accounts that never log in to the desktop.
    Background,
}
impl Session {
    fn starts_at(self) -> &'static str {
        match self {
            Session::Desktop => "user_login",
            Session::Background => "background_session",
        }
    }
}

pub trait ServiceManager {
    fn configuration(&self, paths: &Paths, session: Session) -> Result<Vec<u8>>;
    /// The session an existing configuration runs in; `None` if it is not recognized.
    fn configured_session(&self, configuration: &[u8]) -> Option<Session>;
    /// Whether the user has a desktop session the service could run in.
    fn desktop_available(&self) -> Result<bool>;
    /// Whether the service is registered in any session.
    fn loaded(&self) -> Result<bool>;
    fn start(&self, configuration: &Path, session: Session) -> Result<()>;
    /// Unregisters the service from every session it is registered in.
    fn stop(&self) -> Result<()>;
}

const RECEIPT: &[u8] = b"elsecall-install-v1\n";

#[derive(Debug, Serialize)]
pub struct Installed {
    pub executable: std::path::PathBuf,
    pub launch_agent: std::path::PathBuf,
    pub session: Session,
    pub starts_at: &'static str,
}

fn check_link(link: &Path, target: &Path) -> Result<()> {
    match fs::symlink_metadata(link) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(m)
            if m.file_type().is_symlink()
                && fs::read_link(link).ok().as_deref() == Some(target) =>
        {
            Ok(())
        }
        Err(e) => Err(Failure::io("Inspect installation link", e)),
        _ => Err(Failure::new(
            "installation_conflict",
            format!("Refusing to replace unrelated file {}.", link.display()),
            "Move the conflicting file yourself, then retry.",
        )),
    }
}

/// Whether `link` already points at its owned target. Only an Elsecall installation
/// creates such a link, so it proves ownership even if the receipt was lost.
fn linked(link: &Path, target: &Path) -> bool {
    fs::read_link(link).ok().as_deref() == Some(target)
}

fn owns_links(paths: &Paths) -> bool {
    linked(&paths.bin, &paths.executable()) || linked(&paths.agent, &paths.plist())
}

fn installation_lock(paths: &Paths) -> Result<fs::File> {
    private_files::try_lock(&paths.data.join("installation.lock"))?.ok_or_else(|| {
        Failure::new(
            "operation_in_progress",
            "Another Elsecall install, restart or uninstall is running.",
            "Wait for it to finish, then retry.",
        )
    })
}

fn link(link: &Path, target: &Path) -> Result<()> {
    check_link(link, target)?;
    if fs::symlink_metadata(link).is_err() {
        symlink(target, link).map_err(|e| Failure::io("Create installation link", e))?;
    }
    Ok(())
}

fn check_receipt(paths: &Paths) -> Result<bool> {
    match fs::read(paths.data.join("installation")) {
        Ok(bytes) if bytes == RECEIPT => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        _ => Err(Failure::new(
            "installation_conflict",
            "The installation receipt is not recognized.",
            "Inspect the Elsecall state directory before changing or removing files.",
        )),
    }
}

/// The session of the installed configuration, if there is a recognized one.
pub fn session(paths: &Paths, manager: &dyn ServiceManager) -> Option<Session> {
    fs::read(paths.plist())
        .ok()
        .and_then(|bytes| manager.configured_session(&bytes))
}

/// Whether the installed LaunchAgent is the one this build writes for its session;
/// `None` without a recognized one. A fleet upgrade installs through the build it
/// replaces, so a new build's LaunchAgent applies only from the next install.
pub fn launch_agent_current(paths: &Paths, manager: &dyn ServiceManager) -> Option<bool> {
    let installed = fs::read(paths.plist()).ok()?;
    let session = manager.configured_session(&installed)?;
    let expected = manager.configuration(paths, session).ok()?;
    Some(installed == expected)
}

/// Installs or updates. Without a requested session, an update keeps the session the
/// installation already runs in, and a new installation uses the desktop session when
/// the user has one and the background session otherwise.
pub fn install(
    paths: &Paths,
    source: &Path,
    manager: &dyn ServiceManager,
    requested: Option<Session>,
) -> Result<Installed> {
    private_files::owned_tree(&paths.home, &paths.data)?;
    private_files::directory(&paths.data)?;
    let _lock = installation_lock(paths)?;
    private_files::owned_tree(&paths.home, paths.bin.parent().unwrap())?;
    private_files::owned_tree(&paths.home, paths.agent.parent().unwrap())?;
    check_link(&paths.bin, &paths.executable())?;
    check_link(&paths.agent, &paths.plist())?;
    // Links to owned targets also prove ownership, so a lost receipt or state
    // directory can be repaired. Without either proof, a loaded job may belong to
    // an installation under another HOME and must not be taken over.
    let owned = check_receipt(paths)? || owns_links(paths);
    if !owned
        && (paths.executable().symlink_metadata().is_ok()
            || paths.plist().symlink_metadata().is_ok()
            || manager.loaded()?)
    {
        return Err(Failure::new(
            "installation_conflict",
            "Found an existing installation without an Elsecall receipt or links.",
            "Inspect the existing files and LaunchAgent before retrying.",
        ));
    }
    let binary = fs::read(source).map_err(|e| Failure::io("Read executable to install", e))?;
    let session = match requested.or_else(|| session(paths, manager)) {
        Some(Session::Desktop) if !manager.desktop_available()? => {
            // Refuse before stopping anything, so a remote update never leaves the
            // machine without a running service.
            return Err(Failure::new(
                "desktop_unavailable",
                "The service runs in the desktop session, but this account has no desktop login now.",
                "Run install from the desktop session, or pass --session background to run without one.",
            ));
        }
        Some(session) => session,
        None if manager.desktop_available()? => Session::Desktop,
        None => Session::Background,
    };
    let plist = manager.configuration(paths, session)?;
    private_files::write(&paths.data.join("installation"), RECEIPT, 0o600)?;
    private_files::write(&paths.executable(), &binary, 0o700)?;
    crate::ui_app::store_companion(paths, source)?;
    private_files::write(&paths.plist(), &plist, 0o600)?;
    // Logs are only lifecycle events and allowlisted error fields, never request data.
    if !paths.log().exists() {
        private_files::write(&paths.log(), b"", 0o600)?;
    }
    link(&paths.bin, &paths.executable())?;
    link(&paths.agent, &paths.plist())?;
    restart_locked(paths, manager, session)?;
    Ok(Installed {
        executable: paths.bin.clone(),
        launch_agent: paths.agent.clone(),
        session,
        starts_at: session.starts_at(),
    })
}

pub fn restart(paths: &Paths, manager: &dyn ServiceManager) -> Result<()> {
    if !installed(paths)? {
        return Err(Failure::new(
            "not_installed",
            "The installation is absent or incomplete.",
            "Run elsecall install to repair it.",
        ));
    }
    let _lock = installation_lock(paths)?;
    if !check_receipt(paths)? {
        return Err(Failure::new(
            "not_installed",
            "Elsecall is not installed.",
            "Run elsecall install.",
        ));
    }
    check_link(&paths.agent, &paths.plist())?;
    let session = session(paths, manager).ok_or_else(|| {
        Failure::new(
            "installation_conflict",
            "The installed service configuration is not recognized.",
            "Run elsecall install to rewrite it.",
        )
    })?;
    restart_locked(paths, manager, session)
}

fn restart_locked(paths: &Paths, manager: &dyn ServiceManager, session: Session) -> Result<()> {
    if manager.loaded()? {
        manager.stop()?;
    }
    let service_lock = wait_stopped(paths)?;
    // Release before bootstrap so the new service can take its instance lock.
    drop(service_lock);
    manager.start(&paths.agent, session)
}

fn wait_stopped(paths: &Paths) -> Result<fs::File> {
    for _ in 0..100 {
        if let Some(lock) = private_files::try_lock(&paths.data.join("service.lock"))? {
            return Ok(lock);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Err(Failure::new(
        "service_still_running",
        "A service process still holds the instance lock.",
        "Stop any foreground elsecall serve process and retry; inspect the service log if launchd failed to stop it.",
    ))
}

pub fn installed(paths: &Paths) -> Result<bool> {
    if !paths
        .data
        .try_exists()
        .map_err(|e| Failure::io("Inspect installation", e))?
    {
        return Ok(false);
    }
    private_files::check_directory(&paths.data)?;
    check_link(&paths.bin, &paths.executable())?;
    check_link(&paths.agent, &paths.plist())?;
    Ok(check_receipt(paths)?
        && paths.bin.is_symlink()
        && paths.agent.is_symlink()
        && paths.executable().is_file()
        && paths.plist().is_file())
}

/// Removes the installation. While this machine is enrolled in a fleet, removal would
/// discard its member key, so it is refused unless forced.
pub fn uninstall(paths: &Paths, manager: &dyn ServiceManager, force: bool) -> Result<()> {
    if !force && paths.fleet_membership().symlink_metadata().is_ok() {
        return Err(Failure::new(
            "fleet_member",
            "This machine is enrolled in a fleet; uninstalling deletes its member key, and other members would keep trusting the old one.",
            "Run elsecall fleet remove <name> on another member and elsecall fleet leave here first, or pass --force.",
        ));
    }
    if !paths
        .data
        .try_exists()
        .map_err(|e| Failure::io("Inspect installation", e))?
    {
        if !owns_links(paths) {
            return Ok(());
        }
        // The links prove ownership of a deleted state directory. Recreate it so
        // removal holds the installation lock like every other lifecycle operation.
        private_files::owned_tree(&paths.home, &paths.data)?;
        private_files::directory(&paths.data)?;
    }
    private_files::check_directory(&paths.data)?;
    let _lock = installation_lock(paths)?;
    check_link(&paths.bin, &paths.executable())?;
    check_link(&paths.agent, &paths.plist())?;
    if !(check_receipt(paths)? || owns_links(paths)) {
        return Err(Failure::new(
            "not_installed",
            "No installation receipt or links were found; files were preserved.",
            "Inspect the Elsecall state directory before removing it manually.",
        ));
    }
    if manager.loaded()? {
        manager.stop()?;
    }
    let _service_lock = wait_stopped(paths)?;
    private_files::remove_file(&paths.bin)?;
    private_files::remove_file(&paths.agent)?;
    for name in [
        "elsecall",
        "agent.plist",
        "config.toml",
        "records.sqlite",
        "records.sqlite-wal",
        "records.sqlite-shm",
        "service.sock",
        "service.log",
        "service.log.lock",
        "service.lock",
        "installation",
        "installation.lock",
        "member.key",
        "fleet.json",
        crate::process_service::PID_FILE,
    ] {
        private_files::remove_file(&paths.data.join(name))?;
    }
    for entry in fs::read_dir(&paths.data).map_err(|e| Failure::io("List service logs", e))? {
        let entry = entry.map_err(|e| Failure::io("Inspect service log", e))?;
        if entry
            .file_name()
            .to_str()
            .and_then(|n| n.strip_prefix("service.log."))
            .is_some_and(|n| n.parse::<usize>().is_ok_and(|n| n > 0))
        {
            private_files::remove_file(&entry.path())?;
        }
    }
    crate::fleet_upgrade::journal::Journal::new(&paths.data).remove()?;
    match fs::remove_dir(&paths.data) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => Err(Failure::new(
            "files_preserved",
            "Elsecall was uninstalled; unrecognized files remain in its state directory.",
            "Inspect and remove the remaining files manually if no longer needed.",
        )),
        Err(e) => Err(Failure::io("Remove state directory", e)),
    }
}
