//! Installation policy; the service manager is supplied at the system boundary.
use crate::lifecycle_holder;
use elsecall_failure::{Failure, Result};
use elsecall_local_state::{paths::Paths, private_files};

pub mod cli_request;
pub mod ready;

/// The file a service started as a plain process records its PID in, under the
/// data directory; installation removes it with the service.
pub const PID_FILE: &str = "process-service.pid";
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

const INSTALLATION_LOCK: &str = "installation.lock";

/// The lock every lifecycle operation holds, with its holder recorded in it until it
/// is released, so [`operation_running`] can tell without taking the lock.
struct InstallationLock(fs::File);
impl Drop for InstallationLock {
    fn drop(&mut self) {
        lifecycle_holder::clear(&self.0);
    }
}

fn installation_lock(paths: &Paths) -> Result<InstallationLock> {
    let mut lock =
        private_files::try_lock(&paths.data.join(INSTALLATION_LOCK))?.ok_or_else(|| {
            Failure::new(
                "operation_in_progress",
                "Another Elsecall install, restart or uninstall is running.",
                "Wait for it to finish, then retry.",
            )
        })?;
    lifecycle_holder::record(&mut lock);
    Ok(InstallationLock(lock))
}

/// Whether an install, update, upgrade, restart or uninstall is replacing the
/// service in the state directory `data`: from before it stops the service until it
/// has confirmed the new one. Asking never takes the lock, so it cannot make such
/// an operation fail.
pub fn operation_running(data: &Path) -> bool {
    lifecycle_holder::running(&data.join(INSTALLATION_LOCK))
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
    Transaction::begin(paths)?.install(source, manager, requested)
}

/// One installation transaction may also confirm, publish a window or roll back
/// while excluding every other lifecycle operation.
pub struct Transaction {
    paths: Paths,
    _lock: InstallationLock,
}
impl Transaction {
    pub fn begin(paths: &Paths) -> Result<Self> {
        private_files::owned_tree(&paths.home, &paths.data)?;
        private_files::directory(&paths.data)?;
        Ok(Self {
            paths: paths.clone(),
            _lock: installation_lock(paths)?,
        })
    }
    pub fn paths(&self) -> &Paths {
        &self.paths
    }
    pub fn install(
        &self,
        source: &Path,
        manager: &dyn ServiceManager,
        requested: Option<Session>,
    ) -> Result<Installed> {
        install_locked(&self.paths, source, manager, requested)
    }
}
fn install_locked(
    paths: &Paths,
    source: &Path,
    manager: &dyn ServiceManager,
    requested: Option<Session>,
) -> Result<Installed> {
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
    crate::window_release::store_companion(paths, source)?;
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

/// Restarts the installed service. The returned transaction keeps other lifecycle
/// operations out, and tells requests a handover is in progress, until the caller
/// has confirmed the restarted service and drops it.
pub fn restart(paths: &Paths, manager: &dyn ServiceManager) -> Result<Transaction> {
    if !installed(paths)? {
        return Err(Failure::new(
            "not_installed",
            "The installation is absent or incomplete.",
            "Run elsecall install to repair it.",
        ));
    }
    let transaction = Transaction::begin(paths)?;
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
    restart_locked(paths, manager, session)?;
    Ok(transaction)
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

/// What other slices keep in the state directory, removed with the installation.
pub trait InstalledParts {
    /// Whether a release update's helper is running, which an uninstall must not
    /// cut short.
    fn helper_active(&self, paths: &Paths) -> Result<bool>;
    /// Removes the slices' own state. Called with the installation lock held and
    /// the service stopped.
    fn remove(&self, paths: &Paths) -> Result<()>;
}

/// What an uninstall left in the state directory.
#[derive(Debug, Serialize)]
pub struct Kept {
    pub directory: std::path::PathBuf,
    /// The names left directly in `directory`, sorted.
    pub entries: Vec<String>,
    pub hint: &'static str,
}

/// Removes the installation. While this machine is enrolled in a fleet, removal would
/// discard its member key, so it is refused unless forced.
///
/// State a later install still needs, the operator's own fleet configuration and
/// files Elsecall does not recognize stay; the uninstall still succeeds and
/// returns what stayed.
pub fn uninstall(
    paths: &Paths,
    manager: &dyn ServiceManager,
    parts: &dyn InstalledParts,
    force: bool,
) -> Result<Option<Kept>> {
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
            return Ok(None);
        }
        // The links prove ownership of a deleted state directory. Recreate it so
        // removal holds the installation lock like every other lifecycle operation.
        private_files::owned_tree(&paths.home, &paths.data)?;
        private_files::directory(&paths.data)?;
    }
    private_files::check_directory(&paths.data)?;
    if parts.helper_active(paths)? {
        return Err(Failure::new(
            "operation_in_progress",
            "An Elsecall release update is running.",
            "Wait for it to finish before uninstalling.",
        ));
    }
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
        "elsecall-ui",
        "agent.plist",
        "config.toml",
        "workloads.toml",
        "workloads.lock",
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
        PID_FILE,
    ] {
        private_files::remove_file(&paths.data.join(name))?;
    }
    remove_service_state(&paths.data)?;
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
    parts.remove(paths)?;
    match fs::remove_dir(&paths.data) {
        Ok(()) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
            let mut entries = fs::read_dir(&paths.data)
                .and_then(|entries| {
                    entries
                        .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
                        .collect::<std::io::Result<Vec<_>>>()
                })
                .map_err(|e| Failure::io("List the kept files", e))?;
            entries.sort();
            Ok(Some(Kept {
                directory: paths.data.clone(),
                entries,
                hint: "Elsecall keeps these because a later install still needs them, they are your fleet's configuration, or it does not recognize them; README \"Uninstall\" says which is which. Remove them yourself when you no longer need them.",
            }))
        }
        Err(e) => Err(Failure::io("Remove state directory", e)),
    }
}

/// Removes what the service stores beside the installation, except state a later
/// install still needs to look after something that would otherwise be lost:
///
/// - `config-sync/` while `backups/` holds this member's own earlier versions of
///   synced files; `state.json` indexes them, and without it a new service would
///   discard the older ones as orphans.
/// - `copies.sqlite` and `copies/` while copies of other members' workspaces are
///   recorded; the copies live outside this directory and the catalog lets a
///   later install's housekeeping remove them.
/// - `rehearsal-session.json`, which exists only after an interrupted rehearsal
///   and lets the next service clean up after it.
///
/// Uninstall lists whatever is kept.
fn remove_service_state(data: &Path) -> Result<()> {
    for database in [
        "load.sqlite",
        "outside-pressure.sqlite",
        "outside-processes.sqlite",
        "peer-io.sqlite",
        "tuning-evidence.sqlite",
    ] {
        remove_database(&data.join(database))?;
    }
    // Callers' caches belong to the service alone; earlier builds also kept
    // onboarding operations here.
    remove_tree(&data.join("workspaces"))?;
    remove_tree(&data.join("onboarding"))?;
    if !holds_entries(&data.join("copies")) {
        remove_database(&data.join("copies.sqlite"))?;
        remove_tree(&data.join("copies"))?;
    }
    if !holds_entries(&data.join("config-sync/backups")) {
        remove_tree(&data.join("config-sync"))?;
    }
    Ok(())
}

fn remove_database(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        private_files::remove_file(Path::new(&file))?;
    }
    Ok(())
}

/// Whether a directory has anything in it. Anything other than a real directory,
/// such as a symlink in its place, counts as holding entries, so it is kept.
fn holds_entries(directory: &Path) -> bool {
    match fs::symlink_metadata(directory) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Ok(metadata) if metadata.is_dir() => {
            fs::read_dir(directory).map_or(true, |mut entries| entries.next().is_some())
        }
        _ => true,
    }
}

/// Removes a path the service owns; a symlink in its place is removed, not followed.
fn remove_tree(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Failure::io("Inspect service state", e)),
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(path).map_err(|e| Failure::io("Remove service state", e))
        }
        Ok(_) => private_files::remove_file(path),
    }
}
