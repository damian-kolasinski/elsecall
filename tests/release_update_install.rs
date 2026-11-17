#![cfg(target_os = "macos")]
//! The real installer, transaction, service restart and rollback; provider/OS adapters are fakes.
use elsecall::{
    build_identity::Digest,
    failure::{Failure, Result},
    installation::{self, ServiceManager, Session},
    paths::Paths,
    release_update::{
        self, Job, JobId, Phase, Release, ReleaseDownload, ReleaseVersion, StagedRelease, helper,
        selection::{CliSelection, Snapshot},
        store::Store,
    },
    window_app::{self, WindowLauncher},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Manager {
    paths: Paths,
    child: Mutex<Option<Child>>,
    fail_digest: Option<Digest>,
    unavailable_update_digest: Option<Digest>,
    port: u16,
}
impl Manager {
    fn new(paths: Paths, fail_digest: Option<Digest>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        Self {
            paths,
            child: Mutex::new(None),
            fail_digest,
            unavailable_update_digest: None,
            port,
        }
    }
}
impl ServiceManager for Manager {
    fn configuration(&self, _: &Paths, _: Session) -> Result<Vec<u8>> {
        Ok(b"test release service".to_vec())
    }
    fn configured_session(&self, _: &[u8]) -> Option<Session> {
        Some(Session::Desktop)
    }
    fn desktop_available(&self) -> Result<bool> {
        Ok(true)
    }
    fn loaded(&self) -> Result<bool> {
        Ok(self
            .child
            .lock()
            .unwrap()
            .as_mut()
            .is_some_and(|child| child.try_wait().unwrap().is_none()))
    }
    fn start(&self, _: &Path, _: Session) -> Result<()> {
        if self
            .unavailable_update_digest
            .is_some_and(|digest| Digest::of_file(&self.paths.executable()).unwrap() == digest)
        {
            fs::write(
                Store::new(&self.paths).directory().join("job.json"),
                b"unreadable",
            )
            .unwrap();
        }
        if self
            .fail_digest
            .is_some_and(|digest| Digest::of_file(&self.paths.executable()).unwrap() == digest)
        {
            return Err(release_update::invalid(
                "The candidate service failed to start.",
            ));
        }
        let child = Command::new(self.paths.executable())
            .arg("serve")
            .env("HOME", &self.paths.home)
            .env("ELSECALL_SERVICE_MANAGER", "process")
            .env("ELSECALL_MACHINE_LOAD", "idle")
            .env("ELSECALL_MACHINE_SPEED", "1")
            .env("ELSECALL_TAILNET_NAME", "mac-mini")
            .env("ELSECALL_TAILNET_ADDRESSES", "127.0.0.1")
            .env("ELSECALL_FLEET_PORT", self.port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        *self.child.lock().unwrap() = Some(child);
        Ok(())
    }
    fn stop(&self) -> Result<()> {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            let until = std::time::Instant::now() + Duration::from_secs(10);
            while child.try_wait().unwrap().is_none() {
                if std::time::Instant::now() > until {
                    let _ = child.kill();
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(())
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
struct Download {
    source: PathBuf,
    tamper: bool,
    fail: bool,
}
impl ReleaseDownload for Download {
    fn stage(&self, _: &Release, directory: &Path) -> Result<StagedRelease> {
        if self.fail {
            return Err(Failure::new(
                "signature_rejected",
                "The release signature was rejected.",
                "Retry with a valid signed release.",
            ));
        }
        let executable = directory.join("elsecall");
        fs::copy(&self.source, &executable).unwrap();
        let window = directory.join("elsecall-ui");
        fs::write(&window, b"new-ui").unwrap();
        Ok(StagedRelease {
            digest: if self.tamper {
                Digest::of(b"different")
            } else {
                Digest::of_file(&executable).unwrap()
            },
            window_digest: Digest::of_file(&window).unwrap(),
            executable,
            window,
        })
    }
}
struct Selection {
    fail: bool,
    activated: AtomicUsize,
    restored: AtomicUsize,
}
impl CliSelection for Selection {
    fn snapshot(&self) -> Result<Option<Snapshot>> {
        Ok(Some(Snapshot {
            config: PathBuf::from("/Users/me/.config/mise/config.toml"),
            version: CURRENT.into(),
            identity: None,
            issuer: None,
        }))
    }
    fn activate(&self, _: &Release, _: &StagedRelease, _: &Snapshot) -> Result<()> {
        if self.fail {
            return Err(release_update::invalid("The CLI selection failed."));
        }
        self.activated.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn restore(&self, _: &Release, _: &Snapshot) -> Result<()> {
        self.restored.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn references(&self, _: &Release, _: &Path) -> Result<bool> {
        Ok(false)
    }
}
struct Window {
    fail: bool,
    opened: AtomicUsize,
}
impl WindowLauncher for Window {
    fn sign(&self, _: &Path) -> Result<()> {
        Ok(())
    }
    fn open(&self, _: &Path) -> Result<()> {
        if self.fail {
            return Err(release_update::invalid("The window did not become ready."));
        }
        self.opened.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
/// This build's version.
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The next patch release, which a patched copy of this build claims to be.
fn future() -> String {
    let mut version = semver::Version::parse(CURRENT).unwrap();
    version.patch += 1;
    version.to_string()
}

fn variant(directory: &Path) -> PathBuf {
    let binary = directory.join("candidate");
    fs::copy(env!("CARGO_BIN_EXE_elsecall"), &binary).unwrap();
    let mut bytes = fs::read(&binary).unwrap();
    let (current, next) = (CURRENT.as_bytes(), future());
    // The patch keeps every offset in the executable where it was.
    assert_eq!(next.len(), current.len());
    for index in 0..bytes.len().saturating_sub(current.len()) {
        if &bytes[index..index + current.len()] == current {
            bytes[index..index + current.len()].copy_from_slice(next.as_bytes());
        }
    }
    fs::write(&binary, bytes).unwrap();
    assert!(
        Command::new("/usr/bin/codesign")
            .args([
                "--force",
                "--sign",
                "-",
                "--identifier",
                "dev.elsecall.test.release"
            ])
            .arg(&binary)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        String::from_utf8(
            Command::new(&binary)
                .arg("--version")
                .output()
                .unwrap()
                .stdout
        )
        .unwrap()
        .trim(),
        format!("elsecall {}", future())
    );
    binary
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helper_survives_the_service_restart_and_rolls_back_every_changed_component_on_failure() {
    for mode in [
        "success",
        "install",
        "selection",
        "window",
        "signature",
        "tamper",
        "update-api",
    ] {
        let home = tempfile::Builder::new()
            .prefix("ec-up-install-")
            .tempdir_in("/tmp")
            .unwrap();
        let paths = Paths::for_home(home.path()).unwrap();
        let builds = home.path().join("builds");
        fs::create_dir(&builds).unwrap();
        let original = builds.join("elsecall");
        fs::copy(env!("CARGO_BIN_EXE_elsecall"), &original).unwrap();
        fs::write(builds.join("elsecall-ui"), b"old-ui").unwrap();
        let candidate = variant(home.path());
        let incoming = Digest::of_file(&candidate).unwrap();
        let before = Digest::of_file(&original).unwrap();
        assert_ne!(before, incoming);
        let mut manager = Manager::new(paths.clone(), (mode == "install").then_some(incoming));
        manager.unavailable_update_digest = (mode == "update-api").then_some(incoming);
        installation::install(&paths, &original, &manager, None).unwrap();
        elsecall::fleet_upgrade::helper::confirm(&paths, &before.build_id(), true)
            .await
            .unwrap();
        let old_window = Window {
            fail: false,
            opened: AtomicUsize::new(0),
        };
        window_app::publish(&paths, &old_window).unwrap();
        let store = Store::new(&paths);
        store.prepare(&paths).unwrap();
        let job = Job {
            id: JobId::new().unwrap(),
            release: Release {
                version: ReleaseVersion::try_from(future()).unwrap(),
            },
            from_build: before.build_id(),
            started_at_ms: release_update::now_millis(),
            reopen: true,
            wait_seconds: 1,
            phase: Phase::Starting,
            updated_at: release_update::now(),
        };
        store.save_job(&job).unwrap();
        let selection = Selection {
            fail: mode == "selection",
            activated: AtomicUsize::new(0),
            restored: AtomicUsize::new(0),
        };
        let window = Window {
            fail: mode == "window",
            opened: AtomicUsize::new(0),
        };
        fs::write(store.worker(&job.id), b"worker executable").unwrap();
        // A monitor/status probe may own the lock when the helper starts.
        let probe = store.helper_lock().unwrap().unwrap();
        let release_probe = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            drop(probe);
        });
        helper::run(
            &paths,
            &job.id,
            &Download {
                source: candidate,
                tamper: mode == "tamper",
                fail: mode == "signature",
            },
            &selection,
            &manager,
            &window,
        )
        .await
        .unwrap();
        release_probe.await.unwrap();
        assert!(!store.worker(&job.id).exists());
        assert!(
            !paths
                .home
                .join(".local/share/elsecall/releases")
                .join(job.id.as_str())
                .exists(),
            "unused cache is removed"
        );
        let completed = store.job().unwrap().unwrap();
        if mode == "success" {
            assert!(
                matches!(completed.phase, Phase::Succeeded),
                "{}",
                serde_json::to_string(&completed).unwrap()
            );
            assert_eq!(Digest::of_file(&paths.executable()).unwrap(), incoming);
            assert_eq!(fs::read(paths.window_executable()).unwrap(), b"new-ui");
            assert_eq!(window.opened.load(Ordering::SeqCst), 1);
        } else {
            assert!(
                matches!(completed.phase,Phase::Failed{rolled_back,..} if rolled_back==!matches!(mode,"signature"|"tamper")),
                "{mode}: {}",
                serde_json::to_string(&completed).unwrap()
            );
            assert_eq!(
                Digest::of_file(&paths.executable()).unwrap(),
                before,
                "{mode}"
            );
            assert_eq!(
                fs::read(paths.window_executable()).unwrap(),
                b"old-ui",
                "{mode}"
            );
            assert_eq!(
                fs::read(paths.window_app().join("Contents/MacOS/Elsecall")).unwrap(),
                b"old-ui",
                "{mode}"
            );
            assert_eq!(
                selection.restored.load(Ordering::SeqCst),
                usize::from(mode == "window")
            );
        }
        // The successor exposes the same durable job and releases its admission hold.
        let status: release_update::Status = elsecall::client::get(&paths.socket(), "/v1/update")
            .await
            .unwrap();
        assert_eq!(status.job.unwrap().id, job.id);
        manager.stop().unwrap();
    }
}

mod diagnostics;
mod members;
mod processes;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_production_helper_installs_a_verified_future_build_and_outlives_its_parent_service()
 {
    use std::os::unix::fs::PermissionsExt;
    let builds = tempfile::Builder::new()
        .prefix("ec-release-e2e-")
        .tempdir_in("/tmp")
        .unwrap();
    let candidate = variant(builds.path());
    let mise = builds.path().join("mise");
    let script = builds.path().join("mise.py");
    fs::write(
        &script,
        r#"
import os,sys,json,shutil
from pathlib import Path
args=sys.argv[1:]
if args[0]=='ls': print('[]')
elif args[0]=='trust': pass
elif args[0]=='install':
    root=Path(os.environ['MISE_DATA_DIR'])/'installs/release';root.mkdir(parents=True)
    shutil.copy2(os.environ['TEST_RELEASE_CLI'],root/'elsecall')
    (root/'elsecall-ui').write_bytes(b'future-window')
    (root/'.mise-packslip.json').write_text('{}')
elif args[0]=='which': print(Path(os.environ['MISE_DATA_DIR'])/'installs/release/elsecall')
else: sys.exit(1)
"#,
    )
    .unwrap();
    fs::write(
        &mise,
        format!(
            "#!/bin/sh\nexec /usr/bin/python3 '{}' \"$@\"\n",
            script.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&mise, fs::Permissions::from_mode(0o700)).unwrap();
    let mut member = members::TestMember::install_with(
        "mac-mini",
        Path::new(env!("CARGO_BIN_EXE_elsecall")),
        "",
        &[
            ("ELSECALL_MISE", mise.to_str().unwrap()),
            ("TEST_RELEASE_CLI", candidate.to_str().unwrap()),
        ],
    );
    let paths = Paths::for_home(member.home.path()).unwrap();
    let old_pid = member.installed_pid();
    let store = Store::new(&paths);
    store
        .save_discovery(&release_update::Discovery {
            latest: Some(Release {
                version: ReleaseVersion::try_from(future()).unwrap(),
            }),
            checked_at: Some(release_update::now()),
            error: None,
        })
        .unwrap();
    member.restart();
    let job: Job = elsecall::client::post_within(
        &paths.socket(),
        "/v1/update/install",
        serde_json::json!({"reopen":false,"wait_seconds":2}),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(status) =
            elsecall::client::get::<release_update::Status>(&paths.socket(), "/v1/update").await
            && let Some(current) = status.job
            && current.id == job.id
            && current.phase.terminal()
        {
            assert!(
                matches!(current.phase, Phase::Succeeded),
                "{}",
                serde_json::to_string(&current).unwrap()
            );
            assert_eq!(status.current_version.to_string(), future());
            assert!(status.available.is_none());
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "detached update did not finish"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_ne!(member.installed_pid(), old_pid);
    assert_eq!(
        Digest::of_file(&paths.executable()).unwrap(),
        Digest::of_file(&candidate).unwrap()
    );
    // Terminal state can be observed just before the helper drops its guard.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while store.worker(&job.id).exists() {
        assert!(std::time::Instant::now() < deadline, "worker copy leaked");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
