//! `elsecall install` puts the release's window in place as the app, and opens it on
//! request, with real files and only launchd, signing and launching replaced.
#![cfg(target_os = "macos")]
use elsecall_build_identity::build_identity::Digest;
use elsecall_failure::{Failure, Result};
use elsecall_installation::installation::{self, ServiceManager, Session, Transaction};
use elsecall_local_state::paths::Paths;
use elsecall_window::{
    install_window::{self, Window},
    window_app::{self, WindowLauncher},
};
use std::{cell::RefCell, fs, path::Path, sync::Mutex};

#[derive(Default)]
struct Manager {
    loaded: RefCell<bool>,
    no_desktop: bool,
}
impl ServiceManager for Manager {
    fn configuration(&self, _: &Paths, session: Session) -> Result<Vec<u8>> {
        Ok(match session {
            Session::Desktop => b"desktop".to_vec(),
            Session::Background => b"background".to_vec(),
        })
    }
    fn configured_session(&self, configuration: &[u8]) -> Option<Session> {
        match configuration {
            b"desktop" => Some(Session::Desktop),
            b"background" => Some(Session::Background),
            _ => None,
        }
    }
    fn desktop_available(&self) -> Result<bool> {
        Ok(!self.no_desktop)
    }
    fn loaded(&self) -> Result<bool> {
        Ok(*self.loaded.borrow())
    }
    fn start(&self, _: &Path, _: Session) -> Result<()> {
        *self.loaded.borrow_mut() = true;
        Ok(())
    }
    fn stop(&self) -> Result<()> {
        *self.loaded.borrow_mut() = false;
        Ok(())
    }
}

#[derive(Default)]
struct Desktop {
    fail_sign: bool,
    fail_open: bool,
    signed: Mutex<usize>,
    opened: Mutex<Vec<Vec<u8>>>,
}
impl WindowLauncher for Desktop {
    fn sign(&self, app: &Path) -> Result<()> {
        if self.fail_sign {
            return Err(failed("window_sign_failed"));
        }
        // Signing changes the Mach-O bytes in production too.
        let executable = app.join("Contents/MacOS/Elsecall");
        let mut bytes = fs::read(&executable).unwrap();
        bytes.extend_from_slice(b"-signed");
        fs::write(executable, bytes).unwrap();
        *self.signed.lock().unwrap() += 1;
        Ok(())
    }
    fn open(&self, app: &Path) -> Result<()> {
        self.opened
            .lock()
            .unwrap()
            .push(fs::read(app.join("Contents/MacOS/Elsecall")).unwrap());
        if self.fail_open {
            Err(failed("window_open_failed"))
        } else {
            Ok(())
        }
    }
}
fn failed(code: &str) -> Failure {
    Failure::new(code, "Window operation failed.", "Retry.")
}

/// A home with a release directory holding `elsecall` and, optionally, its window.
fn release(window: Option<&[u8]>) -> (tempfile::TempDir, Paths, std::path::PathBuf) {
    let home = tempfile::Builder::new()
        .prefix("ec-iw-")
        .tempdir_in("/tmp")
        .unwrap();
    let paths = Paths::for_home(home.path()).unwrap();
    let directory = home.path().join("release");
    fs::create_dir(&directory).unwrap();
    let source = directory.join("elsecall");
    fs::write(&source, b"service").unwrap();
    if let Some(window) = window {
        fs::write(directory.join("elsecall-ui"), window).unwrap();
    }
    (home, paths, source)
}

/// `elsecall install [--open]` with a service that is ready at once.
fn run(paths: &Paths, source: &Path, manager: &Manager, desktop: &Desktop, open: bool) -> Window {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (_, window) = runtime
        .block_on(install_window::install(
            paths,
            source,
            manager,
            None,
            desktop,
            open,
            async { Ok(()) },
        ))
        .unwrap();
    window
}
fn install(paths: &Paths, source: &Path, manager: &Manager, desktop: &Desktop) -> Window {
    run(paths, source, manager, desktop, false)
}
fn install_open(paths: &Paths, source: &Path, manager: &Manager, desktop: &Desktop) -> Window {
    run(paths, source, manager, desktop, true)
}

fn app_executable(paths: &Paths) -> std::path::PathBuf {
    paths.window_app().join("Contents/MacOS/Elsecall")
}

#[test]
fn a_desktop_install_places_the_signed_app_without_opening_it() {
    let (_home, paths, source) = release(Some(b"window"));
    let desktop = Desktop::default();
    let window = install(&paths, &source, &Manager::default(), &desktop);
    let Window::Placed { app } = &window else {
        panic!("expected a placed app, got {window:?}");
    };
    assert_eq!(app, &paths.window_app());
    assert_eq!(fs::read(app_executable(&paths)).unwrap(), b"window-signed");
    // The running window reads its release identity from the bundle, not the signed bytes.
    assert_eq!(
        window_app::running_build(&app_executable(&paths)).unwrap(),
        Digest::of(b"window").build_id()
    );
    assert!(desktop.opened.lock().unwrap().is_empty());
    assert_eq!(
        serde_json::to_value(&window).unwrap(),
        serde_json::json!({"state": "placed", "app": paths.window_app()})
    );
    // Finder and the Dock show the icon the bundle names.
    let info = plist::Value::from_file(app.join("Contents/Info.plist")).unwrap();
    let icon = info
        .as_dictionary()
        .and_then(|info| info.get("CFBundleIconFile"))
        .and_then(plist::Value::as_string)
        .unwrap();
    let icon = fs::read(app.join("Contents/Resources").join(format!("{icon}.icns"))).unwrap();
    assert_eq!(&icon[..4], b"icns");
}

#[test]
fn open_launches_the_placed_app_and_waits_for_it() {
    let (_home, paths, source) = release(Some(b"window"));
    let desktop = Desktop::default();
    let window = install_open(&paths, &source, &Manager::default(), &desktop);
    assert!(window.opened(), "{window:?}");
    assert_eq!(
        *desktop.opened.lock().unwrap(),
        vec![b"window-signed".to_vec()]
    );
    assert_eq!(
        serde_json::to_value(&window).unwrap(),
        serde_json::json!({"state": "opened", "app": paths.window_app()})
    );
}

#[test]
fn the_window_opens_only_after_the_service_is_ready_and_inside_the_installation() {
    let (_home, paths, source) = release(Some(b"window"));
    let desktop = Desktop::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let observed = Mutex::new(None);
    let (_, window) = runtime
        .block_on(install_window::install(
            &paths,
            &source,
            &Manager::default(),
            None,
            &desktop,
            true,
            async {
                // The app is already in place, nothing opened it yet, and no other
                // lifecycle operation can start while the service comes up.
                *observed.lock().unwrap() = Some((
                    paths.window_app().is_dir(),
                    desktop.opened.lock().unwrap().len(),
                    Transaction::begin(&paths).err().map(|error| error.code),
                ));
                Ok(())
            },
        ))
        .unwrap();
    assert!(window.opened(), "{window:?}");
    assert_eq!(
        observed.into_inner().unwrap(),
        Some((true, 0, Some("operation_in_progress".into())))
    );
    // The transaction ends with the command.
    assert!(Transaction::begin(&paths).is_ok());
}

#[test]
fn a_service_that_never_becomes_ready_fails_the_install_without_opening() {
    let (_home, paths, source) = release(Some(b"window"));
    let desktop = Desktop::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let error = runtime
        .block_on(install_window::install(
            &paths,
            &source,
            &Manager::default(),
            None,
            &desktop,
            true,
            async { Err::<(), _>(failed("startup_failed")) },
        ))
        .unwrap_err();
    assert_eq!(error.code, "startup_failed");
    assert!(desktop.opened.lock().unwrap().is_empty());
}

#[test]
fn reinstalling_a_newer_release_replaces_the_app() {
    let (home, paths, source) = release(Some(b"old window"));
    let desktop = Desktop::default();
    install(&paths, &source, &Manager::default(), &desktop);
    fs::write(home.path().join("release/elsecall-ui"), b"new window").unwrap();
    let manager = Manager::default();
    *manager.loaded.borrow_mut() = true;
    let window = install(&paths, &source, &manager, &desktop);
    assert!(matches!(window, Window::Placed { .. }), "{window:?}");
    assert_eq!(
        fs::read(app_executable(&paths)).unwrap(),
        b"new window-signed"
    );
    // No staging bundle is left beside the app.
    let leftovers: Vec<_> = fs::read_dir(paths.window_app().parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != "Elsecall.app")
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn a_background_session_gets_no_app() {
    let (_home, paths, source) = release(Some(b"window"));
    let desktop = Desktop::default();
    let manager = Manager {
        no_desktop: true,
        ..Manager::default()
    };
    let window = install_open(&paths, &source, &manager, &desktop);
    assert!(matches!(window, Window::BackgroundSession), "{window:?}");
    assert!(!window.opened());
    assert!(!paths.window_app().exists());
    assert_eq!(*desktop.signed.lock().unwrap(), 0);
    assert_eq!(
        serde_json::to_value(&window).unwrap(),
        serde_json::json!({"state": "background_session"})
    );
}

#[test]
fn a_build_without_a_window_installs_without_an_app() {
    let (_home, paths, source) = release(None);
    let desktop = Desktop::default();
    let window = install(&paths, &source, &Manager::default(), &desktop);
    assert!(matches!(window, Window::NotIncluded), "{window:?}");
    assert!(installation::installed(&paths).unwrap());
    assert!(!paths.window_app().exists());
}

#[test]
fn a_window_that_cannot_be_signed_leaves_the_installation_in_place() {
    let (_home, paths, source) = release(Some(b"window"));
    let desktop = Desktop {
        fail_sign: true,
        ..Desktop::default()
    };
    let window = install_open(&paths, &source, &Manager::default(), &desktop);
    let Window::Failed { error, .. } = &window else {
        panic!("expected a failure, got {window:?}");
    };
    assert_eq!(error.code, "window_sign_failed");
    assert!(desktop.opened.lock().unwrap().is_empty());
    assert!(installation::installed(&paths).unwrap());
    assert!(!paths.window_app().exists());
}

#[test]
fn uninstall_removes_the_app_it_published() {
    let (_home, paths, source) = release(Some(b"window"));
    let manager = Manager::default();
    install(&paths, &source, &manager, &Desktop::default());
    assert!(paths.window_app().is_dir());
    installation::uninstall(
        &paths,
        &manager,
        &elsecall::uninstall::InstalledSlices,
        false,
    )
    .unwrap();
    assert!(!paths.window_app().exists());
    assert!(!paths.data.exists());
}

#[test]
fn uninstall_keeps_another_application_at_the_app_path() {
    let (_home, paths, source) = release(None);
    let manager = Manager::default();
    install(&paths, &source, &manager, &Desktop::default());
    let contents = paths.window_app().join("Contents");
    fs::create_dir_all(&contents).unwrap();
    fs::write(
        contents.join("Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.example.other</string></dict></plist>"#,
    )
    .unwrap();
    installation::uninstall(
        &paths,
        &manager,
        &elsecall::uninstall::InstalledSlices,
        false,
    )
    .unwrap();
    assert!(contents.join("Info.plist").is_file());
}

#[test]
fn a_window_that_does_not_open_stays_placed_for_finder() {
    let (_home, paths, source) = release(Some(b"window"));
    let desktop = Desktop {
        fail_open: true,
        ..Desktop::default()
    };
    let window = install_open(&paths, &source, &Manager::default(), &desktop);
    let Window::Failed { app, error } = &window else {
        panic!("expected a failure, got {window:?}");
    };
    assert_eq!(error.code, "window_open_failed");
    assert_eq!(app, &paths.window_app());
    assert_eq!(fs::read(app_executable(&paths)).unwrap(), b"window-signed");
    assert!(installation::installed(&paths).unwrap());
}
