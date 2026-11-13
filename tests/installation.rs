#![cfg(target_os = "macos")]
use elsecall::{
    failure::{Failure, Result},
    installation::{self, ServiceManager, Session},
    paths::Paths,
};
use std::{cell::RefCell, fs, path::Path};

#[derive(Default)]
struct Manager {
    loaded: RefCell<bool>,
    events: RefCell<Vec<&'static str>>,
    sessions: RefCell<Vec<Session>>,
    fail_start: bool,
    no_desktop: bool,
}
impl ServiceManager for Manager {
    fn configuration(&self, _: &Paths, session: Session) -> Result<Vec<u8>> {
        Ok(match session {
            Session::Desktop => b"test registration".to_vec(),
            Session::Background => b"test background registration".to_vec(),
        })
    }
    fn configured_session(&self, configuration: &[u8]) -> Option<Session> {
        match configuration {
            b"test registration" => Some(Session::Desktop),
            b"test background registration" => Some(Session::Background),
            _ => None,
        }
    }
    fn desktop_available(&self) -> Result<bool> {
        Ok(!self.no_desktop)
    }
    fn loaded(&self) -> Result<bool> {
        Ok(*self.loaded.borrow())
    }
    fn start(&self, config: &Path, session: Session) -> Result<()> {
        assert!(config.exists());
        self.events.borrow_mut().push("start");
        self.sessions.borrow_mut().push(session);
        if self.fail_start {
            return Err(Failure::new(
                "service_manager_error",
                "Cannot start",
                "Retry install",
            ));
        }
        *self.loaded.borrow_mut() = true;
        Ok(())
    }
    fn stop(&self) -> Result<()> {
        self.events.borrow_mut().push("stop");
        *self.loaded.borrow_mut() = false;
        Ok(())
    }
}
fn fixture() -> (tempfile::TempDir, Paths, std::path::PathBuf) {
    let home = tempfile::Builder::new()
        .prefix("ec-")
        .tempdir_in("/tmp")
        .unwrap();
    let paths = Paths::for_home(home.path()).unwrap();
    let source = home.path().join("source");
    fs::write(&source, "binary one").unwrap();
    (home, paths, source)
}

#[test]
fn install_update_restart_and_clean_uninstall_using_real_files_and_fake_lifecycle() {
    let (_home, paths, source) = fixture();
    let manager = Manager::default();
    installation::install(&paths, &source, &manager, None).unwrap();
    assert_eq!(fs::read_to_string(&paths.bin).unwrap(), "binary one");
    assert_eq!(
        fs::read_to_string(&paths.agent).unwrap(),
        "test registration"
    );
    fs::write(&source, "binary two").unwrap();
    installation::install(&paths, &source, &manager, None).unwrap();
    assert_eq!(fs::read_to_string(&paths.bin).unwrap(), "binary two");
    installation::restart(&paths, &manager).unwrap();
    installation::uninstall(&paths, &manager, false).unwrap();
    assert!(!paths.data.exists());
    assert!(!paths.bin.is_symlink());
    assert!(!paths.agent.is_symlink());
    assert_eq!(fs::read_to_string(&source).unwrap(), "binary two");
    assert_eq!(
        *manager.events.borrow(),
        vec!["start", "stop", "start", "stop", "start", "stop"]
    );
    installation::uninstall(&paths, &manager, false).unwrap();
}

#[test]
fn conflict_preserves_existing_executable_and_never_starts_service() {
    let (_home, paths, source) = fixture();
    fs::create_dir_all(paths.bin.parent().unwrap()).unwrap();
    fs::write(&paths.bin, "unrelated").unwrap();
    let manager = Manager::default();
    assert_eq!(
        installation::install(&paths, &source, &manager, None)
            .unwrap_err()
            .code,
        "installation_conflict"
    );
    assert_eq!(fs::read_to_string(paths.bin).unwrap(), "unrelated");
    assert!(manager.events.borrow().is_empty());
}

#[test]
fn failed_start_leaves_diagnosable_installation_that_can_be_removed() {
    let (_home, paths, source) = fixture();
    let manager = Manager {
        fail_start: true,
        ..Default::default()
    };
    assert_eq!(
        installation::install(&paths, &source, &manager, None)
            .unwrap_err()
            .code,
        "service_manager_error"
    );
    assert!(paths.bin.exists());
    assert!(paths.agent.exists());
    installation::uninstall(&paths, &manager, false).unwrap();
    assert!(!paths.data.exists());
}

#[test]
fn uninstall_preserves_unknown_files_and_conflicting_links() {
    let (_home, paths, source) = fixture();
    let manager = Manager::default();
    installation::install(&paths, &source, &manager, None).unwrap();
    fs::remove_file(&paths.bin).unwrap();
    fs::write(&paths.bin, "replacement").unwrap();
    assert_eq!(
        installation::uninstall(&paths, &manager, false)
            .unwrap_err()
            .code,
        "installation_conflict"
    );
    assert!(*manager.loaded.borrow());
    assert_eq!(fs::read_to_string(&paths.bin).unwrap(), "replacement");
    fs::remove_file(&paths.bin).unwrap();
    fs::write(paths.data.join("unrelated"), "keep me").unwrap();
    assert_eq!(
        installation::uninstall(&paths, &manager, false)
            .unwrap_err()
            .code,
        "files_preserved"
    );
    assert_eq!(
        fs::read_to_string(paths.data.join("unrelated")).unwrap(),
        "keep me"
    );
}

#[test]
fn installer_refuses_symlinked_parent_directories() {
    let (home, paths, source) = fixture();
    let elsewhere = home.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, home.path().join("Library")).unwrap();
    assert_eq!(
        installation::install(&paths, &source, &Manager::default(), None)
            .unwrap_err()
            .code,
        "unsafe_directory"
    );
    assert_eq!(fs::read_dir(elsewhere).unwrap().count(), 0);
}

#[test]
fn inspection_reports_broken_links_and_missing_receipt() {
    let (_home, paths, source) = fixture();
    let manager = Manager::default();
    assert!(!installation::installed(&paths).unwrap());
    installation::install(&paths, &source, &manager, None).unwrap();
    assert!(installation::installed(&paths).unwrap());
    fs::remove_file(paths.data.join("installation")).unwrap();
    assert!(!installation::installed(&paths).unwrap());
    fs::remove_file(&paths.bin).unwrap();
    std::os::unix::fs::symlink("/nonexistent", &paths.bin).unwrap();
    assert_eq!(
        installation::installed(&paths).unwrap_err().code,
        "installation_conflict"
    );
}

#[test]
fn restart_waits_for_previous_process_to_release_the_socket() {
    let (_home, paths, source) = fixture();
    let manager = Manager::default();
    installation::install(&paths, &source, &manager, None).unwrap();
    let lock = elsecall::private_files::try_lock(&paths.data.join("service.lock"))
        .unwrap()
        .unwrap();
    let started = std::time::Instant::now();
    let releasing = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        drop(lock);
    });
    installation::restart(&paths, &manager).unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_millis(150));
    releasing.join().unwrap();
}

#[test]
fn install_repairs_lost_receipt_and_deleted_state_directory() {
    let (_home, paths, source) = fixture();
    let manager = Manager::default();
    installation::install(&paths, &source, &manager, None).unwrap();
    fs::remove_file(paths.data.join("installation")).unwrap();
    installation::install(&paths, &source, &manager, None).unwrap();
    assert!(installation::installed(&paths).unwrap());
    fs::remove_dir_all(&paths.data).unwrap();
    assert!(*manager.loaded.borrow());
    installation::install(&paths, &source, &manager, None).unwrap();
    assert!(installation::installed(&paths).unwrap());
    assert_eq!(fs::read_to_string(&paths.bin).unwrap(), "binary one");
}

#[test]
fn uninstall_after_deleted_state_directory_stops_service_and_removes_links() {
    let (_home, paths, source) = fixture();
    let manager = Manager::default();
    installation::install(&paths, &source, &manager, None).unwrap();
    fs::remove_dir_all(&paths.data).unwrap();
    installation::uninstall(&paths, &manager, false).unwrap();
    assert!(!*manager.loaded.borrow());
    assert!(!paths.bin.is_symlink());
    assert!(!paths.agent.is_symlink());
    assert!(!paths.data.exists());
    installation::uninstall(&paths, &manager, false).unwrap();
}

#[test]
fn files_without_receipt_or_links_are_a_conflict() {
    let (_home, paths, source) = fixture();
    let manager = Manager::default();
    elsecall::private_files::directory(&paths.data).unwrap();
    fs::write(paths.executable(), "unknown").unwrap();
    assert_eq!(
        installation::install(&paths, &source, &manager, None)
            .unwrap_err()
            .code,
        "installation_conflict"
    );
    assert_eq!(fs::read_to_string(paths.executable()).unwrap(), "unknown");
    assert!(manager.events.borrow().is_empty());
}

#[test]
fn concurrent_lifecycle_operations_are_refused() {
    let (_home, paths, source) = fixture();
    let manager = Manager::default();
    installation::install(&paths, &source, &manager, None).unwrap();
    let _held = elsecall::private_files::try_lock(&paths.data.join("installation.lock"))
        .unwrap()
        .unwrap();
    for error in [
        installation::install(&paths, &source, &manager, None).unwrap_err(),
        installation::restart(&paths, &manager).unwrap_err(),
        installation::uninstall(&paths, &manager, false).unwrap_err(),
    ] {
        assert_eq!(error.code, "operation_in_progress");
    }
}

#[test]
fn loaded_service_without_receipt_or_links_is_not_taken_over() {
    let (_home, paths, source) = fixture();
    let manager = Manager::default();
    *manager.loaded.borrow_mut() = true;
    assert_eq!(
        installation::install(&paths, &source, &manager, None)
            .unwrap_err()
            .code,
        "installation_conflict"
    );
    assert!(manager.events.borrow().is_empty());
    assert!(!paths.bin.is_symlink());
}

#[test]
fn uninstall_after_lost_receipt_removes_linked_installation() {
    let (_home, paths, source) = fixture();
    let manager = Manager::default();
    installation::install(&paths, &source, &manager, None).unwrap();
    fs::remove_file(paths.data.join("installation")).unwrap();
    fs::remove_file(&paths.bin).unwrap();
    installation::uninstall(&paths, &manager, false).unwrap();
    assert!(!*manager.loaded.borrow());
    assert!(!paths.agent.is_symlink());
    assert!(!paths.data.exists());
}

#[test]
fn uninstall_keeps_an_enrolled_member_key_unless_forced() {
    let (_home, paths, source) = fixture();
    let manager = Manager::default();
    installation::install(&paths, &source, &manager, None).unwrap();
    fs::write(paths.member_key(), "key").unwrap();
    fs::write(paths.fleet_membership(), "roster").unwrap();
    assert_eq!(
        installation::uninstall(&paths, &manager, false)
            .unwrap_err()
            .code,
        "fleet_member"
    );
    assert!(paths.member_key().exists());
    installation::uninstall(&paths, &manager, true).unwrap();
    assert!(!paths.data.exists());
}

#[test]
fn session_follows_desktop_availability_then_sticks_unless_requested() {
    // An account without a desktop login gets the background session.
    let (_home, paths, source) = fixture();
    let headless = Manager {
        no_desktop: true,
        ..Default::default()
    };
    let installed = installation::install(&paths, &source, &headless, None).unwrap();
    assert_eq!(
        (installed.session, installed.starts_at),
        (Session::Background, "background_session")
    );
    // Updates and restarts keep it, even where a desktop session exists.
    let desktop = Manager {
        loaded: RefCell::new(true),
        ..Default::default()
    };
    installation::install(&paths, &source, &desktop, None).unwrap();
    installation::restart(&paths, &desktop).unwrap();
    assert_eq!(
        *desktop.sessions.borrow(),
        [Session::Background, Session::Background]
    );
    assert_eq!(
        installation::session(&paths, &desktop),
        Some(Session::Background)
    );

    // An existing desktop installation is updated in place as a desktop installation.
    let (_home, paths, source) = fixture();
    let manager = Manager::default();
    installation::install(&paths, &source, &manager, None).unwrap();
    let updated = installation::install(&paths, &source, &manager, None).unwrap();
    assert_eq!(updated.session, Session::Desktop);

    // Without a desktop login (e.g. over ssh), a desktop installation is not stopped
    // only to fail to start: the update is refused before anything changes.
    let headless = Manager {
        loaded: RefCell::new(true),
        no_desktop: true,
        ..Default::default()
    };
    assert_eq!(
        installation::install(&paths, &source, &headless, None)
            .unwrap_err()
            .code,
        "desktop_unavailable"
    );
    assert_eq!(
        installation::install(&paths, &source, &headless, Some(Session::Desktop))
            .unwrap_err()
            .code,
        "desktop_unavailable"
    );
    assert!(headless.events.borrow().is_empty());
    assert_eq!(
        installation::session(&paths, &headless),
        Some(Session::Desktop)
    );

    // An explicit request moves the service: stop where it was, start in the new session.
    let moved =
        installation::install(&paths, &source, &headless, Some(Session::Background)).unwrap();
    assert_eq!(moved.session, Session::Background);
    assert_eq!(*headless.events.borrow(), ["stop", "start"]);
    assert_eq!(*headless.sessions.borrow(), [Session::Background]);
}

#[test]
fn launch_agent_leaves_logging_to_the_service() {
    use elsecall::installation::{ServiceManager, Session};
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let paths = Paths::for_home(home.path()).unwrap();
    let bytes = elsecall::macos::launch_agent::LaunchAgent
        .configuration(&paths, Session::Desktop)
        .unwrap();
    let plist = plist::Value::from_reader(std::io::Cursor::new(bytes)).unwrap();
    let fields = plist.as_dictionary().unwrap();
    assert_eq!(fields["StandardOutPath"].as_string(), Some("/dev/null"));
    assert_eq!(fields["StandardErrorPath"].as_string(), Some("/dev/null"));
}

#[test]
fn launch_agent_written_by_an_earlier_build_is_reported_stale() {
    use elsecall::macos::launch_agent::LaunchAgent;
    let (_home, paths, _) = fixture();
    assert_eq!(
        installation::launch_agent_current(&paths, &LaunchAgent),
        None
    );
    fs::create_dir_all(&paths.data).unwrap();
    for session in [Session::Desktop, Session::Background] {
        let current = LaunchAgent.configuration(&paths, session).unwrap();
        fs::write(paths.plist(), &current).unwrap();
        assert_eq!(
            installation::launch_agent_current(&paths, &LaunchAgent),
            Some(true)
        );
        // The same session's LaunchAgent from a build before `ProcessType`.
        let mut earlier = plist::Value::from_reader_xml(current.as_slice()).unwrap();
        earlier.as_dictionary_mut().unwrap().remove("ProcessType");
        let mut bytes = Vec::new();
        earlier.to_writer_xml(&mut bytes).unwrap();
        fs::write(paths.plist(), &bytes).unwrap();
        assert_eq!(
            installation::launch_agent_current(&paths, &LaunchAgent),
            Some(false)
        );
    }
}
