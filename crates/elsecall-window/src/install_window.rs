//! `elsecall install` with the fleet window that came with the release.
//!
//! The window never decides whether the service installed: a failure to place or
//! open the app is reported beside a successful installation.
use crate::window_app::{self, WindowLauncher};
use elsecall_contract::failure::{Failure, Result};
use elsecall_installation::installation::{Installed, ServiceManager, Session, Transaction};
use elsecall_local_state::paths::Paths;
use serde::Serialize;
use std::{future::Future, path::Path, path::PathBuf};

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Window {
    /// The app is in place and its window acknowledged that it is ready.
    Opened { app: PathBuf },
    /// The app is in place, to open from Finder, Spotlight or `elsecall ui`.
    Placed { app: PathBuf },
    /// The service runs without a desktop login, so there is nowhere to show a window.
    BackgroundSession,
    /// The installed build came without a window.
    NotIncluded,
    /// The service is installed, but the app could not be placed or opened.
    Failed { app: PathBuf, error: Failure },
}
impl Window {
    pub fn opened(&self) -> bool {
        matches!(self, Window::Opened { .. })
    }
}

/// Installs the service from `source`, puts its window in place as the app, waits
/// for `ready` (the new service answering) and, when asked, opens the window.
///
/// One installation transaction covers every step, so no other lifecycle
/// operation, such as `elsecall ui` or an update, can swap the app between
/// placing and opening it.
pub async fn install(
    paths: &Paths,
    source: &Path,
    manager: &dyn ServiceManager,
    session: Option<Session>,
    launcher: &dyn WindowLauncher,
    open: bool,
    ready: impl Future<Output = Result<impl Sized>>,
) -> Result<(Installed, Window)> {
    let transaction = Transaction::begin(paths)?;
    let installed = transaction.install(source, manager, session)?;
    let window = place(&transaction, installed.session, launcher);
    // The window reads the fleet from the service, so it opens only once that answers.
    ready.await?;
    let window = if open {
        open_placed(window, launcher)
    } else {
        window
    };
    Ok((installed, window))
}

fn place(transaction: &Transaction, session: Session, launcher: &dyn WindowLauncher) -> Window {
    let paths = transaction.paths();
    if session == Session::Background {
        return Window::BackgroundSession;
    }
    // The transaction excludes every other lifecycle operation, so the stored
    // window cannot appear or disappear before it is placed.
    if !paths.window_executable().is_file() {
        return Window::NotIncluded;
    }
    // A window that later fails to open stays in place for Finder: it is the
    // build this installation stored, unlike the bundle it replaced.
    match window_app::place_locked(transaction, launcher) {
        Ok(placed) => Window::Placed {
            app: placed.app().to_path_buf(),
        },
        Err(error) => Window::Failed {
            app: paths.window_app(),
            error,
        },
    }
}

fn open_placed(window: Window, launcher: &dyn WindowLauncher) -> Window {
    match window {
        Window::Placed { app } => match launcher.open(&app) {
            Ok(()) => Window::Opened { app },
            Err(error) => Window::Failed { app, error },
        },
        other => other,
    }
}
