//! Lets the window close itself once Elsecall is uninstalled, the way it retires
//! after an upgrade. Uninstall stops the service, removes the CLI link, and removes
//! the app last.

use std::path::{Path, PathBuf};

/// The installation this window belongs to.
pub(crate) trait Bundle: Send + Sync {
    /// Whether both the app this window runs from and the CLI link are gone. An
    /// app moved elsewhere by the user leaves the CLI link in place.
    fn uninstalled(&self) -> bool;
    fn quit(&self);
}

/// The running executable's app and the installation's CLI link, captured at
/// startup while both still exist.
pub(crate) struct RunningBundle {
    app: Option<PathBuf>,
    cli: Option<PathBuf>,
}

impl RunningBundle {
    pub(crate) fn current() -> Self {
        Self {
            app: std::env::current_exe().ok().as_deref().and_then(app_of),
            cli: elsecall_host::paths::Paths::current()
                .ok()
                .map(|paths| paths.bin),
        }
    }
}

impl Bundle for RunningBundle {
    fn uninstalled(&self) -> bool {
        [&self.app, &self.cli]
            .iter()
            .all(|path| path.as_deref().is_some_and(missing))
    }

    fn quit(&self) {
        std::process::exit(0);
    }
}

fn missing(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

/// The `.app` containing `executable`, when it runs as the published app's
/// `Contents/MacOS/Elsecall`. A window started from a build directory has none,
/// so it never closes itself.
fn app_of(executable: &Path) -> Option<PathBuf> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (executable.file_name()? == "Elsecall"
        && macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app")
        .then(|| app.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_published_apps_executable_belongs_to_an_app() {
        assert_eq!(
            app_of(Path::new(
                "/Users/me/Applications/Elsecall.app/Contents/MacOS/Elsecall"
            )),
            Some(PathBuf::from("/Users/me/Applications/Elsecall.app"))
        );
        for executable in [
            "/Users/me/elsecall/target/debug/elsecall-ui",
            "/Users/me/Applications/Elsecall.app/Contents/MacOS/other",
            "/Users/me/Applications/Elsecall/Contents/MacOS/Elsecall",
            "/Elsecall",
        ] {
            assert_eq!(app_of(Path::new(executable)), None, "{executable}");
        }
    }

    #[test]
    fn only_an_app_and_cli_link_both_gone_are_an_uninstall() {
        let directory = tempfile::tempdir().unwrap();
        let app = directory.path().join("Elsecall.app");
        let cli = directory.path().join("elsecall");
        std::fs::create_dir(&app).unwrap();
        std::os::unix::fs::symlink("/nonexistent/elsecall", &cli).unwrap();
        let bundle = RunningBundle {
            app: Some(app.clone()),
            cli: Some(cli.clone()),
        };
        assert!(!bundle.uninstalled());
        // Moved to /Applications by the user: still installed. A dangling link
        // still counts as the installation's.
        std::fs::remove_dir(&app).unwrap();
        assert!(!bundle.uninstalled());
        std::fs::remove_file(&cli).unwrap();
        assert!(bundle.uninstalled());
        // A window from a build directory never closes itself.
        assert!(
            !RunningBundle {
                app: None,
                cli: Some(cli)
            }
            .uninstalled()
        );
    }
}
