#![cfg(target_os = "macos")]
//! Housekeeping of a member's workspace copies between real member services in
//! separate temporary homes, with loopback standing in for the tailnet. Only
//! tailnet discovery, machine load, the housekeeping clock and, where disk pressure
//! is the subject, the volume's size are replaced.
mod members;
mod processes;

use elsecall::{
    housekeeping::{
        caller_state::CallerState,
        callers::Nobody,
        catalog::Catalog,
        clock::{Clock, FileClock},
        pass::{Housekeeper, Parts},
        settings::Settings,
        volumes::Statfs,
    },
    scheduling::timestamp::Timestamp,
    workspace::{
        copies::Copies,
        framed_link::FramedLink,
        location::{Locations, Setting},
        object_sources::ObjectSources,
        push_cache::CacheFiles,
        snapshot, sync,
    },
};
use members::{TestMember, enrol};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

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

/// A directory of caller repositories, removed with the test.
struct Workspaces {
    _temp: tempfile::TempDir,
    base: PathBuf,
}
fn workspaces() -> Workspaces {
    let temp = tempfile::Builder::new()
        .prefix("ec-hk-")
        .tempdir_in("/tmp")
        .unwrap();
    let base = temp.path().canonicalize().unwrap();
    Workspaces { _temp: temp, base }
}
impl Workspaces {
    /// A repository with a commit, an ignored build directory and an edit.
    fn repository(&self, name: &str) -> PathBuf {
        let root = self.base.join(name);
        std::fs::create_dir_all(root.join("src")).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(root.join("src/lib.txt"), "one\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        std::fs::write(root.join("src/lib.txt"), "one\ntwo\n").unwrap();
        root
    }
    /// A second working tree of `repository`.
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

/// The settings every test member uses unless it says otherwise: no periodic
/// passes during a test, and no disk pressure from this machine's real disk.
const QUIET: &str = "[housekeeping]\ninterval = \"7d\"\nmin_free_percent = 0\nmin_free = \"0\"\n";

/// A master and a member whose housekeeping clock reads `clock`.
fn fleet(
    clock: &Path,
    configuration: &str,
    environment: &[(&str, &str)],
) -> (TestMember, TestMember) {
    let alpha = TestMember::start("alpha");
    let mut environment = environment.to_vec();
    let clock = clock.to_str().unwrap().to_owned();
    environment.push(("ELSECALL_HOUSEKEEPING_CLOCK", &clock));
    let bravo = TestMember::start_with("bravo", configuration, &environment);
    enrol(&alpha, &bravo);
    (alpha, bravo)
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

fn copies(member: &TestMember) -> Value {
    member.ok(&["copies"])
}

fn prune(member: &TestMember, dry_run: bool) -> Value {
    if dry_run {
        member.ok(&["copies", "prune", "--dry-run"])
    } else {
        member.ok(&["copies", "prune"])
    }
}

fn copy_of<'a>(listing: &'a Value, caller_path: &Path) -> &'a Value {
    listing["copies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["caller_path"] == caller_path.to_str().unwrap())
        .unwrap_or_else(|| panic!("no copy of {}: {listing}", caller_path.display()))
}

fn timestamp(value: &Value) -> Timestamp {
    Timestamp::parse_rfc3339(value.as_str().unwrap()).unwrap()
}

fn set_clock(clock: &Path, at: Timestamp) {
    std::fs::write(clock, at.to_string()).unwrap();
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

fn until(what: &str, limit: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "{what} did not happen within {limit:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn copies_are_listed_and_idle_ones_go_exactly_at_the_idle_period() {
    let w = workspaces();
    let clock = w.file("clock");
    let (alpha, bravo) = fleet(&clock, QUIET, &[]);
    let app = w.repository("app");
    ran(run_on(
        &alpha,
        "bravo",
        &app,
        "mkdir -p build && echo warm > build/state",
    ));

    let listing = copies(&bravo);
    assert_eq!(listing["member"], "bravo");
    let copy = copy_of(&listing, &app).clone();
    assert_eq!(copy["calling_member"], "alpha");
    assert_eq!(copy["setting"], "mapped");
    assert_eq!(copy["state"], "idle");
    let location = PathBuf::from(copy["location"].as_str().unwrap());
    assert!(location.starts_with(bravo.home.path()), "{location:?}");
    assert!(
        location.join("build/state").exists(),
        "the ignored build state is kept"
    );
    let used = timestamp(&copy["last_used_at"]);
    assert_eq!(timestamp(&copy["idle_expires_at"]), used.after(14 * DAY));
    let stores = listing["stores"].as_array().unwrap();
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0]["copies"], 1);
    assert_eq!(stores[0]["path"], copy["store"]);

    // A dry run measures and reports, and removes nothing.
    set_clock(&clock, used.after(14 * DAY));
    let planned = prune(&bravo, true);
    assert_eq!(planned["dry_run"], true);
    assert_eq!(
        reasons(&planned),
        vec![(app.to_str().unwrap().to_owned(), "idle".into())]
    );
    assert_eq!(planned["stores_removed"].as_array().unwrap().len(), 1);
    assert!(location.join("build/state").exists());
    let store = PathBuf::from(copy["store"].as_str().unwrap());
    assert!(store.join("objects").exists());
    assert!(
        !git(&store, &["for-each-ref", "refs/elsecall/"]).is_empty(),
        "the store keeps the copy's references"
    );
    let state = bravo
        .home
        .path()
        .join("Library/Application Support/Elsecall/copies");
    assert_eq!(std::fs::read_dir(&state).unwrap().count(), 1);
    let listing = copies(&bravo);
    assert!(copy_of(&listing, &app)["size_bytes"].as_u64().unwrap() > 0);
    assert!(listing["stores"][0]["size_bytes"].as_u64().unwrap() > 0);
    assert!(listing["removals"].as_array().unwrap().is_empty());

    // One millisecond before the idle period ends, the copy stays.
    set_clock(&clock, used.after(14 * DAY - Duration::from_millis(1)));
    let kept = prune(&bravo, false);
    assert!(kept["removed"].as_array().unwrap().is_empty(), "{kept}");
    assert!(location.join("build/state").exists());

    // At the end of the idle period it goes, with its store and its record here.
    set_clock(&clock, used.after(14 * DAY));
    let report = prune(&bravo, false);
    assert_eq!(
        reasons(&report),
        vec![(app.to_str().unwrap().to_owned(), "idle".into())]
    );
    assert_eq!(report["stores_removed"].as_array().unwrap().len(), 1);
    assert!(
        report["problems"].as_array().unwrap().is_empty(),
        "{report}"
    );
    assert!(!location.exists());
    let store = PathBuf::from(copy["store"].as_str().unwrap());
    assert!(!store.exists());
    let state = bravo
        .home
        .path()
        .join("Library/Application Support/Elsecall/copies");
    assert_eq!(
        std::fs::read_dir(&state).map(|d| d.count()).unwrap_or(0),
        0,
        "the copy's sync state is gone too"
    );

    let listing = copies(&bravo);
    assert!(listing["copies"].as_array().unwrap().is_empty());
    assert!(listing["stores"].as_array().unwrap().is_empty());
    let removals = listing["removals"].as_array().unwrap();
    assert_eq!(removals.len(), 2, "{listing}");
    assert_eq!(removals[0]["kind"], "store");
    assert_eq!(removals[0]["reason"], "unreferenced");
    assert_eq!(removals[1]["kind"], "copy");
    assert_eq!(removals[1]["reason"], "idle");
    assert_eq!(removals[1]["caller_path"], app.to_str().unwrap());
    assert_eq!(removals[1]["calling_member"], "alpha");
    assert_eq!(removals[1]["last_used_at"], copy["last_used_at"]);
    assert!(removals[1]["size_bytes"].as_u64().unwrap() > 0);
    assert_eq!(listing["housekeeping"]["last_pass"]["removed"], 2);

    // The next run starts a fresh copy.
    ran(run_on(
        &alpha,
        "bravo",
        &app,
        "test ! -e build/state && git status --short",
    ));
    assert_eq!(copies(&bravo)["copies"].as_array().unwrap().len(), 1);
}

#[test]
fn an_object_store_stays_while_any_worktree_copy_uses_it() {
    let w = workspaces();
    let clock = w.file("clock");
    let (alpha, bravo) = fleet(&clock, QUIET, &[]);
    let app = w.repository("app");
    let feature = w.worktree(&app, "feature");
    let start = Timestamp::now();
    set_clock(&clock, start);
    ran(run_on(&alpha, "bravo", &app, "true"));
    set_clock(&clock, start.after(10 * DAY));
    ran(run_on(&alpha, "bravo", &feature, "true"));
    let listing = copies(&bravo);
    assert_eq!(listing["stores"].as_array().unwrap().len(), 1, "{listing}");
    assert_eq!(listing["stores"][0]["copies"], 2);
    let store = PathBuf::from(listing["stores"][0]["path"].as_str().unwrap());

    set_clock(&clock, start.after(14 * DAY));
    let report = prune(&bravo, false);
    assert_eq!(
        reasons(&report),
        vec![(app.to_str().unwrap().to_owned(), "idle".into())]
    );
    assert!(report["stores_removed"].as_array().unwrap().is_empty());
    assert!(store.exists());
    assert_eq!(copies(&bravo)["stores"][0]["copies"], 1);
    // The remaining copy still reads its history from the store.
    ran(run_on(
        &alpha,
        "bravo",
        &feature,
        "git log --oneline | grep -q initial",
    ));

    // The second copy was last used just now, at the clock's 14 days.
    set_clock(&clock, start.after(28 * DAY));
    let report = prune(&bravo, false);
    assert_eq!(
        reasons(&report),
        vec![(feature.to_str().unwrap().to_owned(), "idle".into())]
    );
    assert_eq!(report["stores_removed"][0]["path"], store.to_str().unwrap());
    assert!(!store.exists());
}

#[test]
fn disk_pressure_evicts_the_least_recently_used_idle_copies_until_it_recovers() {
    let w = workspaces();
    let clock = w.file("clock");
    let volume = w.file("volume.json");
    let (alpha, bravo) = fleet(
        &clock,
        "[housekeeping]\ninterval = \"7d\"\nmin_free_percent = 0\nmin_free = \"1GB\"\n",
        &[("ELSECALL_SIMULATED_VOLUME", volume.to_str().unwrap())],
    );
    let names = ["first", "second", "third"];
    let repos: Vec<PathBuf> = names.iter().map(|n| w.repository(n)).collect();
    // About 2 MB of ignored build state in each copy.
    for repo in &repos {
        ran(run_on(
            &alpha,
            "bravo",
            repo,
            "mkdir -p build && head -c 2000000 /dev/zero > build/big",
        ));
    }
    // The first is used again, so the second is now the least recently used.
    ran(run_on(&alpha, "bravo", &repos[0], "true"));

    let root = bravo.home.path().join(".elsecall/workspaces");
    let total: u64 = 10_000_000_000;
    let threshold: u64 = 1_000_000_000;
    // Free space 3 MB short of the threshold: one copy and its store are not
    // enough, two are.
    let held = elsecall::housekeeping::measure::allocated(&root).unwrap();
    let simulate = |short: u64| {
        std::fs::write(
            &volume,
            serde_json::json!({
                "total_bytes": total,
                "used_elsewhere_bytes": total - threshold - held + short,
            })
            .to_string(),
        )
        .unwrap()
    };
    simulate(3_000_000);
    let disk = &copies(&bravo)["disk"];
    assert_eq!(disk["pressure"], true, "{disk}");
    assert_eq!(disk["threshold_bytes"], threshold);

    let report = prune(&bravo, false);
    assert_eq!(
        reasons(&report),
        vec![
            (
                repos[1].to_str().unwrap().to_owned(),
                "disk_pressure".into()
            ),
            (
                repos[2].to_str().unwrap().to_owned(),
                "disk_pressure".into()
            ),
        ],
        "{report}"
    );
    assert_eq!(report["disk"][0]["outcome"], "recovered", "{report}");
    assert_eq!(report["disk"][0]["evicted"], 2);
    assert_eq!(report["stores_removed"].as_array().unwrap().len(), 2);
    let listing = copies(&bravo);
    assert_eq!(listing["copies"].as_array().unwrap().len(), 1);
    copy_of(&listing, &repos[0]);
    assert_eq!(listing["disk"]["pressure"], false);
    let recorded: Vec<&str> = listing["removals"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["kind"] == "copy")
        .map(|r| r["reason"].as_str().unwrap())
        .collect();
    assert_eq!(recorded, ["disk_pressure", "disk_pressure"]);

    // When even every idle copy could not make up the shortfall, the copies stay
    // warm and the pass says it could not recover.
    simulate(500_000_000);
    let report = prune(&bravo, false);
    assert!(report["removed"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["disk"][0]["outcome"], "unrecoverable", "{report}");
    assert_eq!(report["disk"][0]["evicted"], 0);
    let evictable = report["disk"][0]["evictable_bytes"].as_u64().unwrap();
    assert!(
        evictable > 2_000_000 && evictable < 500_000_000,
        "{evictable}"
    );
    copy_of(&copies(&bravo), &repos[0]);
    assert_eq!(
        copies(&bravo)["housekeeping"]["last_pass"]["disk_pressure_unrelieved"],
        true
    );
}

#[test]
fn a_copy_in_use_is_never_removed() {
    let w = workspaces();
    let clock = w.file("clock");
    let volume = w.file("volume.json");
    std::fs::write(
        &volume,
        r#"{"total_bytes": 10000000000, "used_elsewhere_bytes": 0}"#,
    )
    .unwrap();
    let (alpha, bravo) = fleet(
        &clock,
        "[housekeeping]\ninterval = \"7d\"\nmin_free_percent = 0\nmin_free = \"1GB\"\n",
        &[("ELSECALL_SIMULATED_VOLUME", volume.to_str().unwrap())],
    );
    let app = w.repository("app");
    let mut running = crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .current_dir(&app)
        .env("HOME", alpha.home.path())
        .args([
            "run",
            "--member",
            "bravo",
            "--",
            "/bin/sh",
            "-c",
            "echo started; sleep 60",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(running.stdout.as_mut().unwrap()),
        &mut line,
    )
    .unwrap();
    assert_eq!(line, "started\n");
    let copy = copy_of(&copies(&bravo), &app).clone();
    assert_eq!(copy["state"], "in_use");
    let location = PathBuf::from(copy["location"].as_str().unwrap());

    // Idle by the clock, and the volume under pressure: still kept.
    set_clock(&clock, Timestamp::now().after(30 * DAY));
    std::fs::write(
        &volume,
        r#"{"total_bytes": 10000000000, "used_elsewhere_bytes": 9999000000}"#,
    )
    .unwrap();
    let report = prune(&bravo, false);
    assert!(report["removed"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["spared"][0]["reason"], "idle");
    assert_eq!(report["spared"][0]["spared"], "in_use");
    assert_eq!(report["disk"][0]["outcome"], "unrecoverable");
    assert!(location.join(".git").exists());

    // SAFETY: interrupting our own child.
    unsafe { libc::kill(running.id() as i32, libc::SIGINT) };
    running.wait().unwrap();
    until("the run ended on bravo", crate::processes::WAIT, || {
        copy_of(&copies(&bravo), &app)["state"] == "idle"
    });
    // The run's end is its last use; the copy goes once that is idle long enough.
    let used = timestamp(&copy_of(&copies(&bravo), &app)["last_used_at"]);
    set_clock(&clock, used.after(14 * DAY));
    std::fs::write(
        &volume,
        r#"{"total_bytes": 10000000000, "used_elsewhere_bytes": 0}"#,
    )
    .unwrap();
    let report = prune(&bravo, false);
    assert_eq!(
        reasons(&report),
        vec![(app.to_str().unwrap().to_owned(), "idle".into())]
    );
    assert!(!location.exists());
}

#[test]
fn copies_made_before_housekeeping_are_found_and_judged() {
    let w = workspaces();
    let clock = w.file("clock");
    let (alpha, mut bravo) = fleet(&clock, QUIET, &[]);
    let app = w.repository("app");
    ran(run_on(&alpha, "bravo", &app, "true"));
    let location = copy_of(&copies(&bravo), &app)["location"].clone();
    // A member that kept no catalog, as one updated from an earlier version.
    bravo.stop();
    let data = bravo
        .home
        .path()
        .join("Library/Application Support/Elsecall");
    for name in ["copies.sqlite", "copies.sqlite-wal", "copies.sqlite-shm"] {
        let _ = std::fs::remove_file(data.join(name));
    }
    bravo.restart();
    assert!(copies(&bravo)["copies"].as_array().unwrap().is_empty());
    let report = prune(&bravo, false);
    assert_eq!(report["adopted"], 1, "{report}");
    assert!(report["removed"].as_array().unwrap().is_empty());
    let listing = copies(&bravo);
    let copy = copy_of(&listing, &app);
    assert_eq!(copy["location"], location);
    assert_eq!(copy["calling_member"], "alpha");
    assert_eq!(listing["stores"][0]["copies"], 1);
}

#[test]
fn housekeeping_runs_periodically_as_deferrable_pinned_work() {
    let w = workspaces();
    let clock = w.file("clock");
    let (alpha, bravo) = fleet(
        &clock,
        "[housekeeping]\ninterval = \"2s\"\nmin_free_percent = 0\nmin_free = \"0\"\n",
        &[],
    );
    let app = w.repository("app");
    ran(run_on(&alpha, "bravo", &app, "true"));
    set_clock(&clock, Timestamp::now().after(30 * DAY));
    until(
        "a periodic pass removed the idle copy",
        Duration::from_secs(30),
        || copies(&bravo)["copies"].as_array().unwrap().is_empty(),
    );
    let runs = bravo.ok(&["runs"]);
    let pass = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == "elsecall housekeeping")
        .unwrap_or_else(|| panic!("no housekeeping record: {runs}"))
        .clone();
    let record = bravo.ok(&["explain", pass["run_id"].as_str().unwrap()]);
    assert_eq!(record["placement"], "pinned", "{record}");
    assert!(record["urgency"]["deferrable"].is_object(), "{record}");
    assert_eq!(record["urgency"]["deferrable"]["at_deadline"], "skip");
}

/// A transport that receives the member's first reply, then waits before
/// delivering it. The reply proves the member opened and checked the copy.
struct Gated<L> {
    inner: L,
    gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
}
impl<L: sync::Link> sync::Link for Gated<L> {
    async fn send(&mut self, message: sync::SyncMessage) -> elsecall::failure::Result<()> {
        self.inner.send(message).await
    }
    async fn send_content(&mut self, chunk: Vec<u8>) -> elsecall::failure::Result<()> {
        self.inner.send_content(chunk).await
    }
    async fn receive(&mut self) -> elsecall::failure::Result<sync::Received> {
        let received = self.inner.receive().await?;
        if let Some((release, reached)) = self.gate.take() {
            reached.notify_one();
            release.notified().await;
        }
        Ok(received)
    }
}

/// The member's half of a sync that has begun and waits for the caller: the copy
/// is open and checked, and the caller has not sent its plan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sync_in_flight_holds_its_copy_against_housekeeping() {
    let w = workspaces();
    let app = w.repository("app");
    let member = w.file("member");
    let locations = Locations {
        setting: Setting::Mapped,
        root: member.join("workspaces"),
        same_path_base: member.join("same"),
        state: member.join("state/copies"),
    };
    std::fs::create_dir_all(&locations.state).unwrap();
    let clock_file = w.file("clock");
    let clock: Arc<dyn Clock> = Arc::new(FileClock(clock_file.clone()));
    let catalog = Arc::new(Catalog::open(&member.join("copies.sqlite"), clock.clone()).unwrap());
    let copies = Copies::new(catalog.clone());
    let housekeeper = Arc::new(Housekeeper::new(Parts {
        catalog,
        copies: copies.clone(),
        locations: locations.clone(),
        volumes: Arc::new(Statfs),
        settings: Settings {
            min_free_percent: 0,
            min_free_bytes: 0,
            ..Settings::default()
        },
        member: Box::new(|| "bravo".into()),
        callers: Arc::new(Nobody),
        caller_state: Arc::new(CallerState::new(member.join("caller"))),
    }));
    let located = snapshot::locate(&app).unwrap().unwrap();
    let location = locations
        .locate("alpha", &located.root_text, &located.common_dir)
        .unwrap();
    let caches = w.file("caches");
    std::fs::create_dir_all(&caches).unwrap();

    // A first, complete sync, as a run's.
    let sync_once = |gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>| {
        let (a, b) = tokio::io::duplex(1 << 20);
        let (caller_read, caller_write) = tokio::io::split(a);
        let (member_read, member_write) = tokio::io::split(b);
        let caches = CacheFiles::new(&caches, &located.root_text);
        let located = located.clone();
        let location = location.clone();
        let caller = tokio::spawn(async move {
            let mut link = Gated {
                inner: FramedLink {
                    reader: caller_read,
                    writer: caller_write,
                },
                gate,
            };
            sync::push(&mut link, &located, &caches, "bravo", &[]).await
        });
        let member = tokio::spawn(async move {
            let mut link = FramedLink {
                reader: member_read,
                writer: member_write,
            };
            sync::receive(&mut link, &location, &ObjectSources::default())
                .await
                .map(|_| ())
        });
        (caller, member)
    };
    let mut claim = copies.claim(&location).unwrap();
    claim.begin_use();
    let (caller, member_side) = sync_once(None);
    caller.await.unwrap().unwrap();
    member_side.await.unwrap().unwrap();
    drop(claim);
    assert!(location.root.join(".git").exists());

    // The next run's sync begins and stalls; its claim holds the copy.
    std::fs::write(app.join("src/lib.txt"), "one\ntwo\nthree\n").unwrap();
    let mut claim = copies.claim(&location).unwrap();
    claim.begin_use();
    let gate = Arc::new(tokio::sync::Notify::new());
    let reached = Arc::new(tokio::sync::Notify::new());
    let (caller, member_side) = sync_once(Some((gate.clone(), reached.clone())));
    tokio::time::timeout(crate::processes::WAIT, reached.notified())
        .await
        .expect("the member checked the copy");
    assert!(!member_side.is_finished());
    std::fs::write(&clock_file, Timestamp::now().after(30 * DAY).to_string()).unwrap();
    let report = housekeeper.pass(false).await;
    let report = serde_json::to_value(&report).unwrap();
    assert!(report["removed"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["spared"][0]["spared"], "in_use");
    assert!(report["stores_removed"].as_array().unwrap().is_empty());
    assert!(location.store.exists());

    // The sync completes into the intact copy.
    gate.notify_one();
    caller.await.unwrap().unwrap();
    member_side.await.unwrap().unwrap();
    assert_eq!(
        std::fs::read_to_string(location.root.join("src/lib.txt")).unwrap(),
        "one\ntwo\nthree\n"
    );
    drop(claim);

    // Released, its last use is the clock's 30 days; idle 14 days later, it goes.
    std::fs::write(&clock_file, Timestamp::now().after(44 * DAY).to_string()).unwrap();
    let report = serde_json::to_value(housekeeper.pass(false).await).unwrap();
    assert_eq!(report["removed"][0]["reason"], "idle", "{report}");
    assert!(!location.root.exists());
    assert!(!location.store.exists());
}

#[test]
fn a_store_with_many_packs_is_compacted_without_losing_history() {
    let w = workspaces();
    let clock = w.file("clock");
    let (alpha, bravo) = fleet(
        &clock,
        "[housekeeping]\ninterval = \"7d\"\nmin_free_percent = 0\nmin_free = \"0\"\ncompact_store_packs = 3\n",
        &[],
    );
    let app = w.repository("app");
    // Every new commit arrives as a pack of its own.
    for n in 0..3 {
        std::fs::write(app.join(format!("file{n}.txt")), format!("{n}\n")).unwrap();
        git(&app, &["add", "-A"]);
        git(&app, &["commit", "-q", "-m", &format!("commit {n}")]);
        ran(run_on(&alpha, "bravo", &app, "true"));
    }
    let store = PathBuf::from(copies(&bravo)["stores"][0]["path"].as_str().unwrap());
    let packs = || {
        std::fs::read_dir(store.join("objects/pack"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".pack"))
            .count()
    };
    assert!(packs() >= 3, "{}", packs());

    let report = prune(&bravo, false);
    let compacted = &report["stores_compacted"][0];
    assert_eq!(compacted["path"], store.to_str().unwrap(), "{report}");
    assert!(compacted["packs_before"].as_u64().unwrap() >= 3);
    assert!(packs() < 3, "{}", packs());
    assert!(report["removed"].as_array().unwrap().is_empty());
    // The copy still has every commit, and the next run needs no history.
    ran(run_on(
        &alpha,
        "bravo",
        &app,
        "test \"$(git rev-list --count HEAD)\" = 4 && git fsck --connectivity-only",
    ));
    let report = prune(&bravo, false);
    assert!(
        report["stores_compacted"].as_array().unwrap().is_empty(),
        "{report}"
    );
}

#[test]
fn a_same_path_copy_holding_another_is_removed_only_after_it() {
    let w = workspaces();
    let clock = w.file("clock");
    let base = w.file("member-paths");
    std::fs::create_dir_all(&base).unwrap();
    let (alpha, bravo) = fleet(
        &clock,
        &format!("{QUIET}[workspace]\nlocation = \"same-path\"\n"),
        &[("ELSECALL_SAME_PATH_BASE", base.to_str().unwrap())],
    );
    // A worktree inside the main checkout, which ignores it.
    let app = w.repository("app");
    std::fs::write(app.join(".gitignore"), "build/\n.worktrees/\n").unwrap();
    let inner = w.worktree(&app, "x");
    let nested = app.join(".worktrees/x");
    std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
    git(
        &app,
        &[
            "worktree",
            "move",
            inner.to_str().unwrap(),
            nested.to_str().unwrap(),
        ],
    );
    ran(run_on(&alpha, "bravo", &app, "true"));
    ran(run_on(&alpha, "bravo", &nested, "true"));
    let outer_copy = base.join(app.strip_prefix("/").unwrap());
    let inner_copy = base.join(nested.strip_prefix("/").unwrap());
    assert!(inner_copy.join(".git").exists());

    set_clock(&clock, Timestamp::now().after(30 * DAY));
    let report = prune(&bravo, false);
    assert_eq!(
        reasons(&report),
        vec![(nested.to_str().unwrap().to_owned(), "idle".into())],
        "{report}"
    );
    assert_eq!(report["spared"][0]["spared"], "contains_copy");
    assert!(outer_copy.join(".git").exists());
    assert!(!inner_copy.exists());
    let report = prune(&bravo, false);
    assert_eq!(
        reasons(&report),
        vec![(app.to_str().unwrap().to_owned(), "idle".into())]
    );
    assert!(!outer_copy.exists());
    assert!(base.exists(), "only the copies go");
}

#[test]
fn a_directory_that_is_no_longer_the_copy_is_left_alone() {
    let w = workspaces();
    let clock = w.file("clock");
    let (alpha, bravo) = fleet(&clock, QUIET, &[]);
    let app = w.repository("app");
    ran(run_on(&alpha, "bravo", &app, "true"));
    let location = PathBuf::from(copy_of(&copies(&bravo), &app)["location"].as_str().unwrap());
    // Command exit reaches the caller before the member releases its claim.
    until("the copy to become idle", crate::processes::WAIT, || {
        copy_of(&copies(&bravo), &app)["state"] == "idle"
    });
    // Someone replaced the copy with a directory of their own.
    std::fs::remove_dir_all(&location).unwrap();
    std::fs::create_dir(&location).unwrap();
    std::fs::write(location.join("precious"), "mine").unwrap();
    set_clock(&clock, Timestamp::now().after(30 * DAY));
    let output = bravo.run(&["copies", "prune"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a pass with problems exits 1"
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["data"]["problems"][0]["code"], "housekeeping_refused",
        "{report}"
    );
    assert!(report["data"]["removed"].as_array().unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(location.join("precious")).unwrap(),
        "mine"
    );
    assert!(
        copies(&bravo)["copies"].as_array().unwrap().is_empty(),
        "no longer tracked"
    );
}

#[test]
fn a_store_another_copy_may_still_read_is_kept() {
    let w = workspaces();
    let clock = w.file("clock");
    let (alpha, mut bravo) = fleet(&clock, QUIET, &[]);
    let app = w.repository("app");
    ran(run_on(&alpha, "bravo", &app, "true"));
    let listing = copies(&bravo);
    let location = PathBuf::from(copy_of(&listing, &app)["location"].as_str().unwrap());
    let store = PathBuf::from(listing["stores"][0]["path"].as_str().unwrap());
    // A member that lost track of a copy, and the copy moved out of its root.
    bravo.stop();
    let data = bravo
        .home
        .path()
        .join("Library/Application Support/Elsecall");
    for name in ["copies.sqlite", "copies.sqlite-wal", "copies.sqlite-shm"] {
        let _ = std::fs::remove_file(data.join(name));
    }
    let elsewhere = w.file("moved-copy");
    std::fs::rename(&location, &elsewhere).unwrap();
    bravo.restart();
    let report = prune(&bravo, false);
    assert!(
        report["stores_removed"].as_array().unwrap().is_empty(),
        "{report}"
    );
    assert_eq!(report["stores_kept"][0]["unknown_copies"], 1);
    assert!(store.exists());
    // The moved copy still reads its history.
    assert!(git(&elsewhere, &["log", "--oneline"]).contains("initial"));
}

#[test]
fn queued_deferrable_work_does_not_keep_placed_runs_out() {
    let w = workspaces();
    let clock = w.file("clock");
    // Capacity 2 on this machine's 12 weighted cores.
    let (alpha, bravo) = fleet(
        &clock,
        &format!("{QUIET}[capacity]\nreserved_cores = 10\n"),
        &[],
    );
    let app = w.repository("app");
    let hold = |options: &str| {
        crate::processes::command("/bin/sh")
            .arg("-c")
            .arg(format!(
                "\"$EC\" lease acquire {options} --holder $$ >/dev/null; sleep 300"
            ))
            .env("HOME", bravo.home.path())
            .env("EC", env!("CARGO_BIN_EXE_elsecall"))
            .spawn()
            .unwrap()
    };
    let mut holding = hold("--size 1");
    until("the first lease", crate::processes::WAIT, || {
        bravo.ok(&["runs"])["member"]["in_use"] == 1.0
    });
    let mut deferred = hold("--size 2 --deferrable --deadline 30m --at-deadline skip");
    until(
        "the deferrable request queues",
        crate::processes::WAIT,
        || bravo.ok(&["runs"])["member"]["queued"]["deferrable"] == 1,
    );
    // The placed run fits the free core and starts within a short hold limit.
    ran(crate::processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .current_dir(&app)
        .env("HOME", alpha.home.path())
        .args(["run", "--member", "bravo", "--hold-limit", "30s", "--"])
        .args(["/bin/sh", "-c", "test \"$ELSECALL_MEMBER\" = bravo"])
        .stdin(Stdio::null())
        .output()
        .unwrap());
    assert_eq!(bravo.ok(&["runs"])["member"]["queued"]["deferrable"], 1);
    for child in [&mut holding, &mut deferred] {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[test]
fn the_fleet_view_shows_each_members_copy_disk_use() {
    let w = workspaces();
    let clock = w.file("clock");
    let (alpha, bravo) = fleet(&clock, QUIET, &[]);
    let app = w.repository("app");
    ran(run_on(
        &alpha,
        "bravo",
        &app,
        "mkdir -p build && head -c 1000000 /dev/zero > build/big",
    ));
    prune(&bravo, true);
    let view = alpha.view_of("bravo");
    let copies = &view["status"]["copies"];
    assert_eq!(copies["copies"], 1, "{view}");
    assert!(copies["copies_bytes"].as_u64().unwrap() >= 1_000_000);
    assert_eq!(copies["stores"], 1);
    assert!(copies["stores_bytes"].as_u64().unwrap() > 0);
    assert_eq!(copies["unmeasured_copies"], 0);
    assert_eq!(copies["disk"]["pressure"], false);
    assert_eq!(copies["disk"]["threshold_bytes"], 0);
    assert!(copies["disk"]["available_bytes"].as_u64().unwrap() > 0);
    assert!(copies["last_pass_at"].is_null(), "a dry run is not a pass");
    prune(&bravo, false);
    assert!(alpha.view_of("bravo")["status"]["copies"]["last_pass_at"].is_string());
    let own = &alpha.view_of("alpha")["status"]["copies"];
    assert_eq!(own["copies"], 0, "{own}");
}
