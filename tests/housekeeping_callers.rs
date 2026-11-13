#![cfg(target_os = "macos")]
//! Copies whose workspace was deleted on the calling member, between real member
//! services in separate temporary homes on loopback: the member learns it from the
//! copy's owner alone, over the authenticated member channel, and both sides clean
//! up their state.
mod members;
mod processes;

use elsecall::{
    failure::Result,
    housekeeping::{
        caller_state::CallerState,
        callers::Callers,
        catalog::Catalog,
        clock::SystemClock,
        exchange::{Answer, Question, ROUTE, RemovedCopy, WorkspaceAnswer, WorkspaceState},
        pass::{Housekeeper, Parts},
        settings::Settings,
        volumes::Statfs,
    },
    paths::Paths,
    peer_client,
    scheduling::timestamp::Timestamp,
    workspace::{
        copies::Copies,
        location::{CopyLocation, Locations, Setting},
        push_cache::CacheFiles,
    },
};
use futures_util::future::BoxFuture;
use members::{TestMember, enrol};
use serde_json::Value;
use std::sync::Arc;
use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = crate::processes::command("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

struct Workspaces {
    _temp: tempfile::TempDir,
    base: PathBuf,
}
fn workspaces() -> Workspaces {
    let temp = tempfile::Builder::new()
        .prefix("ec-hkc-")
        .tempdir_in("/tmp")
        .unwrap();
    let base = temp.path().canonicalize().unwrap();
    Workspaces { _temp: temp, base }
}
impl Workspaces {
    fn repository(&self, name: &str) -> PathBuf {
        let root = self.base.join(name);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("README.md"), "hello\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        root
    }
    fn worktree(&self, repository: &Path, name: &str) -> PathBuf {
        let root = self.base.join(name);
        git(
            repository,
            &["worktree", "add", "-q", "-b", name, root.to_str().unwrap()],
        );
        root
    }
    fn file(&self, name: &str) -> PathBuf {
        self.base.join(name)
    }
}

const QUIET: &str = "[housekeeping]\ninterval = \"7d\"\nmin_free_percent = 0\nmin_free = \"0\"\n";

fn member(name: &'static str, clock: &Path) -> TestMember {
    TestMember::start_with(
        name,
        QUIET,
        &[("ELSECALL_HOUSEKEEPING_CLOCK", clock.to_str().unwrap())],
    )
}

fn run_on(caller: &TestMember, member: &str, cwd: &Path, command: &str) -> Output {
    crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .current_dir(cwd)
        .env("HOME", caller.home.path())
        .args(["run", "--member", member, "--", "/bin/sh", "-c", command])
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn ran(output: Output) {
    assert!(output.status.success(), "{output:?}");
}

fn reasons(report: &Value) -> Vec<(String, String)> {
    report["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["caller_path"].as_str().unwrap().to_owned(),
                r["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn delete_worktree(repository: &Path, worktree: &Path) {
    std::fs::remove_dir_all(worktree).unwrap();
    git(repository, &["worktree", "prune"]);
}

/// The caller's sync state for the workspace at `root`.
fn caller_files(caller: &TestMember, root: &Path) -> CacheFiles {
    CacheFiles::new(
        &Paths::for_home(caller.home.path())
            .unwrap()
            .data
            .join("workspaces"),
        root.to_str().unwrap(),
    )
}

fn caller_state_of(caller: &TestMember, root: &Path) -> Vec<String> {
    let files = caller_files(caller, root);
    let directory = files.stat().parent().unwrap().to_owned();
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(files.id()))
        .map(|name| name[files.id().len()..].to_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_copy_goes_once_its_workspace_is_deleted_on_the_caller() {
    let w = workspaces();
    let clock = w.file("clock");
    let alpha = TestMember::start_with("alpha", QUIET, &[]);
    let bravo = member("bravo", &clock);
    enrol(&alpha, &bravo);
    let app = w.repository("app");
    let feature = w.worktree(&app, "feature");
    ran(run_on(&alpha, "bravo", &app, "true"));
    ran(run_on(
        &alpha,
        "bravo",
        &feature,
        "mkdir -p build && echo x > build/o",
    ));
    let listing = bravo.ok(&["copies"]);
    let location = listing["copies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["caller_path"] == feature.to_str().unwrap())
        .unwrap()["location"]
        .as_str()
        .unwrap()
        .to_owned();

    // While the worktree exists, its copy stays.
    let report = bravo.ok(&["copies", "prune"]);
    assert!(report["removed"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["owners"][0]["member"], "alpha");
    assert_eq!(report["owners"][0]["asked"], 2);
    assert_eq!(report["owners"][0]["gone"], 0);

    delete_worktree(&app, &feature);
    let planned = bravo.ok(&["copies", "prune", "--dry-run"]);
    assert_eq!(
        reasons(&planned),
        vec![(
            feature.to_str().unwrap().to_owned(),
            "caller_deleted".into()
        )]
    );
    assert!(Path::new(&location).exists());
    assert!(caller_files(&alpha, &feature).pushed("bravo").exists());

    let report = bravo.ok(&["copies", "prune"]);
    assert_eq!(
        reasons(&report),
        vec![(
            feature.to_str().unwrap().to_owned(),
            "caller_deleted".into()
        )]
    );
    assert_eq!(report["owners"][0]["gone"], 1);
    assert!(!Path::new(&location).exists());
    assert!(
        report["stores_removed"].as_array().unwrap().is_empty(),
        "the other worktree still uses the store"
    );
    let listing = bravo.ok(&["copies"]);
    assert_eq!(listing["copies"].as_array().unwrap().len(), 1);
    assert_eq!(listing["removals"][0]["reason"], "caller_deleted");
    assert_eq!(
        listing["removals"][0]["caller_path"],
        feature.to_str().unwrap()
    );
    ran(run_on(
        &alpha,
        "bravo",
        &app,
        "git log --oneline | grep -q initial",
    ));
}

#[test]
fn caller_state_is_cleaned_up_for_deleted_workspaces_and_removed_copies() {
    let w = workspaces();
    let clock = w.file("clock");
    let alpha = TestMember::start_with("alpha", QUIET, &[]);
    let bravo = member("bravo", &clock);
    enrol(&alpha, &bravo);
    let app = w.repository("app");
    let feature = w.worktree(&app, "feature");
    ran(run_on(&alpha, "bravo", &app, "true"));
    ran(run_on(&alpha, "bravo", &feature, "true"));
    assert_eq!(
        caller_state_of(&alpha, &feature),
        ["-bravo.pushed", ".stat", ".workspace"]
    );

    delete_worktree(&app, &feature);
    let report = bravo.ok(&["copies", "prune"]);
    assert_eq!(reasons(&report).len(), 1, "{report}");
    assert_eq!(
        caller_state_of(&alpha, &feature),
        [".stat", ".workspace"],
        "bravo told alpha it removed the copy"
    );

    // Alpha's own pass removes what is left of the deleted workspace.
    let planned = alpha.ok(&["copies", "prune", "--dry-run"]);
    assert_eq!(planned["caller_state"]["files"], 2, "{planned}");
    assert_eq!(caller_state_of(&alpha, &feature).len(), 2);
    let report = alpha.ok(&["copies", "prune"]);
    assert_eq!(report["caller_state"]["files"], 2, "{report}");
    assert!(caller_state_of(&alpha, &feature).is_empty());
    assert_eq!(
        caller_state_of(&alpha, &app),
        ["-bravo.pushed", ".stat", ".workspace"],
        "a live workspace keeps its state"
    );

    // A copy removed while its workspace lives drops only that member's push.
    std::fs::write(
        &clock,
        Timestamp::now()
            .after(Duration::from_secs(30 * 86_400))
            .to_string(),
    )
    .unwrap();
    let report = bravo.ok(&["copies", "prune"]);
    assert_eq!(
        reasons(&report),
        vec![(app.to_str().unwrap().to_owned(), "idle".into())]
    );
    assert_eq!(caller_state_of(&alpha, &app), [".stat", ".workspace"]);
    ran(run_on(&alpha, "bravo", &app, "git status --short"));
}

#[test]
fn only_the_member_that_owns_a_copy_can_cause_its_removal() {
    let w = workspaces();
    let clock = w.file("clock");
    let mut alpha = TestMember::start_with("alpha", QUIET, &[]);
    let bravo = member("bravo", &clock);
    let charlie = member("charlie", &clock);
    enrol(&alpha, &bravo);
    enrol(&alpha, &charlie);
    let app = w.repository("app");
    let feature = w.worktree(&app, "feature");
    ran(run_on(&alpha, "bravo", &feature, "true"));
    delete_worktree(&app, &feature);

    // With its owner unreachable, the copy stays, though its workspace is gone.
    alpha.stop();
    let report = bravo.ok(&["copies", "prune"]);
    assert!(report["removed"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["owners"][0]["member"], "alpha");
    assert_eq!(report["owners"][0]["error"]["code"], "member_unavailable");
    alpha.restart();

    // Another enrolled member learns nothing and changes nothing about workspaces
    // alpha pushed to bravo, even when it names them.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let ask = |asker: &TestMember, question: &Question| -> Answer {
        runtime
            .block_on(peer_client::call(
                &asker.key(),
                &alpha.as_member(),
                hyper::Method::POST,
                ROUTE,
                Some(question),
                Duration::from_secs(10),
            ))
            .unwrap()
    };
    let path = feature.to_str().unwrap().to_owned();
    let forged = Question {
        dry_run: false,
        held: vec![path.clone()],
        removed: vec![RemovedCopy {
            path: path.clone(),
            removed_at: Timestamp::now().after(Duration::from_secs(3600)),
        }],
    };
    let answer = ask(&charlie, &forged);
    assert_eq!(answer.workspaces[0].state, WorkspaceState::Unknown);
    assert_eq!(answer.forgotten, 0);
    assert!(caller_files(&alpha, &feature).pushed("bravo").exists());
    let honest = Question {
        dry_run: true,
        held: vec![path.clone()],
        removed: Vec::new(),
    };
    assert_eq!(
        ask(&bravo, &honest).workspaces[0].state,
        WorkspaceState::Gone
    );

    // Only the owner's answer removes the copy.
    let report = bravo.ok(&["copies", "prune"]);
    assert_eq!(
        reasons(&report),
        vec![(path, "caller_deleted".into())],
        "{report}"
    );
}

/// Calling members whose answers are scripted: each says every path it names is
/// gone, including paths it was not asked about. One may use a copy while it is
/// asked, as a run that starts meanwhile.
struct Scripted {
    gone: Vec<(&'static str, &'static str)>,
    used_while_asked: Option<(Arc<Catalog>, CopyLocation)>,
}
impl Callers for Scripted {
    fn members(&self) -> Option<Vec<String>> {
        Some(vec!["alpha".into(), "charlie".into()])
    }
    fn ask(&self, member: &str, question: Question) -> BoxFuture<'static, Result<Answer>> {
        if let Some((catalog, location)) = &self.used_while_asked {
            catalog
                .record_use(location, Timestamp::now().after(Duration::from_secs(1)))
                .unwrap();
        }
        let workspaces = if question.held.is_empty() {
            Vec::new()
        } else {
            self.gone
                .iter()
                .filter(|(owner, _)| *owner == member)
                .map(|(_, path)| WorkspaceAnswer {
                    path: (*path).into(),
                    state: WorkspaceState::Gone,
                })
                .collect()
        };
        Box::pin(async move {
            Ok(Answer {
                workspaces,
                forgotten: 0,
            })
        })
    }
}

struct Member {
    _temp: tempfile::TempDir,
    catalog: Arc<Catalog>,
    locations: Locations,
}
fn holding_member() -> Member {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    Member {
        catalog: Arc::new(
            Catalog::open(&base.join("copies.sqlite"), Arc::new(SystemClock)).unwrap(),
        ),
        locations: Locations {
            setting: Setting::Mapped,
            root: base.join("workspaces"),
            same_path_base: base.join("same"),
            state: base.join("state"),
        },
        _temp: temp,
    }
}
impl Member {
    /// A marked copy owned by `owner`, last used a minute ago.
    fn copy(&self, owner: &str, caller_path: &str) -> CopyLocation {
        let location = self
            .locations
            .locate(owner, caller_path, caller_path)
            .unwrap();
        std::fs::create_dir_all(location.root.join(".git")).unwrap();
        std::fs::write(
            location.root.join(".git/elsecall-copy"),
            format!("{owner}\n{caller_path}\n"),
        )
        .unwrap();
        self.catalog
            .record_use(
                &location,
                Timestamp::from_millis(Timestamp::now().millis() - 60_000),
            )
            .unwrap();
        location
    }
    fn housekeeper(&self, callers: Scripted) -> Arc<Housekeeper> {
        Arc::new(Housekeeper::new(Parts {
            catalog: self.catalog.clone(),
            copies: Copies::default(),
            locations: self.locations.clone(),
            volumes: Arc::new(Statfs),
            settings: Settings {
                min_free_percent: 0,
                min_free_bytes: 0,
                ..Settings::default()
            },
            member: Box::new(|| "bravo".into()),
            callers: Arc::new(callers),
            caller_state: Arc::new(CallerState::new(self.locations.state.join("caller"))),
        }))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_what_the_owner_was_asked_about_its_own_copies_counts() {
    let m = holding_member();
    let alphas = m.copy("alpha", "/w/app");
    let charlies = m.copy("charlie", "/w/app");
    let charlies_other = m.copy("charlie", "/w/other");
    // Alpha says its workspace is gone, and also charlie's other one, which it was
    // not asked about. Charlie says nothing is gone.
    let housekeeper = m.housekeeper(Scripted {
        gone: vec![("alpha", "/w/app"), ("alpha", "/w/other")],
        used_while_asked: None,
    });
    let report = serde_json::to_value(housekeeper.pass(false).await).unwrap();
    let removed: Vec<&str> = report["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["location"].as_str().unwrap())
        .collect();
    assert_eq!(removed, [alphas.root.to_str().unwrap()], "{report}");
    assert_eq!(report["removed"][0]["reason"], "caller_deleted");
    assert!(
        report["problems"].as_array().unwrap().is_empty(),
        "{report}"
    );
    assert!(!alphas.root.exists());
    assert!(charlies.root.exists(), "the same path, another owner");
    assert!(charlies_other.root.exists(), "alpha was not asked about it");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_copy_used_after_its_owner_was_asked_is_kept() {
    let m = holding_member();
    let copy = m.copy("alpha", "/w/app");
    let housekeeper = m.housekeeper(Scripted {
        gone: vec![("alpha", "/w/app")],
        used_while_asked: Some((m.catalog.clone(), copy.clone())),
    });
    let report = serde_json::to_value(housekeeper.pass(false).await).unwrap();
    assert!(report["removed"].as_array().unwrap().is_empty(), "{report}");
    assert!(copy.root.exists());
}
