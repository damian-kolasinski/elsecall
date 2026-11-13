#![cfg(target_os = "macos")]
//! The workspace copy engine against real git repositories: a caller's dirty working
//! tree is synced into a member's copy over the real sync protocol, and the copy must
//! then look the same to git and to the file system.
mod processes;
use elsecall::workspace::{
    carried_refs::RefPattern,
    framed_link::FramedLink,
    location::{CopyLocation, Locations, Setting},
    object_sources::ObjectSources,
    push_cache::CacheFiles,
    snapshot::{self, Located},
    sync::{self, MemberStats, SyncStats},
};
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    task::{Context, Poll},
    time::{Duration, Instant},
};

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
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
    output.stdout
}
fn text(dir: &Path, args: &[&str]) -> String {
    String::from_utf8(git(dir, args)).unwrap()
}
/// git's output, empty when it fails, as some commands do before the first commit.
fn lenient(dir: &Path, args: &[&str]) -> String {
    let output = crate::processes::command("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}
fn write(dir: &Path, path: &str, content: impl AsRef<[u8]>) {
    let path = dir.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

struct World {
    _temp: tempfile::TempDir,
    remote: PathBuf,
    caller: PathBuf,
    member: PathBuf,
    caches: PathBuf,
}

/// A caller clone of a remote with history, and an empty member home.
fn world() -> World {
    let temp = tempfile::Builder::new()
        .prefix("ec-ws-")
        .tempdir_in("/tmp")
        .unwrap();
    let base = temp.path().canonicalize().unwrap();
    let remote = base.join("remote.git");
    let caller = base.join("caller/app");
    let member = base.join("member");
    let caches = base.join("caches");
    for dir in [&caller, &member, &caches] {
        std::fs::create_dir_all(dir).unwrap();
    }
    git(
        &base,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            remote.to_str().unwrap(),
        ],
    );
    git(&caller, &["init", "-q", "-b", "main"]);
    git(
        &caller,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    World {
        _temp: temp,
        remote,
        caller,
        member,
        caches,
    }
}

impl World {
    fn locations(&self, setting: Setting) -> Locations {
        Locations {
            setting,
            root: self.member.join("workspaces"),
            same_path_base: self.member.join("same"),
            state: self.member.join("state/copies"),
        }
    }
    fn locate(&self, cwd: &Path) -> Located {
        snapshot::locate(cwd)
            .unwrap()
            .expect("inside a working tree")
    }
    fn copy_location(&self, located: &Located, setting: Setting) -> CopyLocation {
        self.locations(setting)
            .locate("alpha", &located.root_text, &located.common_dir)
            .unwrap()
    }
    /// A mapped location for `calling_member`, whose copies share their own store.
    fn calling_member_location(&self, located: &Located, calling_member: &str) -> CopyLocation {
        self.locations(Setting::Mapped)
            .locate(calling_member, &located.root_text, &located.common_dir)
            .unwrap()
    }
}

async fn sync_into(
    located: &Located,
    caches: &Path,
    location: &CopyLocation,
) -> (SyncStats, MemberStats) {
    sync_with(located, caches, location, &ObjectSources::default(), None).await
}

/// A link that carries at most this many bytes a second from the caller, as a
/// network does; the member's answers are small.
struct Throttled<W> {
    inner: W,
    bytes_per_second: Option<u64>,
    pause: Option<Pin<Box<tokio::time::Sleep>>>,
}
impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Throttled<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if let Some(pause) = self.pause.as_mut() {
            std::task::ready!(pause.as_mut().poll(cx));
            self.pause = None;
        }
        let Some(rate) = self.bytes_per_second else {
            return Pin::new(&mut self.inner).poll_write(cx, buf);
        };
        let limit = buf.len().min(64 * 1024);
        let written = std::task::ready!(Pin::new(&mut self.inner).poll_write(cx, &buf[..limit]))?;
        let pause = Duration::from_secs_f64(written as f64 / rate as f64);
        self.pause = Some(Box::pin(tokio::time::sleep(pause)));
        Poll::Ready(Ok(written))
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A sync with the member's `sources`, over a link limited to `bytes_per_second`.
async fn sync_with(
    located: &Located,
    caches: &Path,
    location: &CopyLocation,
    sources: &ObjectSources,
    bytes_per_second: Option<u64>,
) -> (SyncStats, MemberStats) {
    let (caller, member) =
        sync_carrying(located, caches, location, sources, bytes_per_second, &[]).await;
    (caller.expect("caller side"), member.expect("member side"))
}

/// A sync that carries the refs `declared` patterns match, as each side ended it.
async fn sync_carrying(
    located: &Located,
    caches: &Path,
    location: &CopyLocation,
    sources: &ObjectSources,
    bytes_per_second: Option<u64>,
    declared: &[&str],
) -> (
    elsecall::failure::Result<SyncStats>,
    elsecall::failure::Result<MemberStats>,
) {
    let declared: Vec<RefPattern> = declared
        .iter()
        .map(|pattern| RefPattern::parse(pattern).expect("a valid pattern"))
        .collect();
    let (a, b) = tokio::io::duplex(1 << 20);
    let (caller_read, caller_write) = tokio::io::split(a);
    let caller_write = Throttled {
        inner: caller_write,
        bytes_per_second,
        pause: None,
    };
    let sources = sources.clone();
    let (member_read, member_write) = tokio::io::split(b);
    let caches = CacheFiles::new(caches, &located.root_text);
    let located = located.clone();
    let location = location.clone();
    let caller = tokio::spawn(async move {
        let mut link = FramedLink {
            reader: caller_read,
            writer: caller_write,
        };
        sync::push(&mut link, &located, &caches, "bravo", &declared).await
    });
    let member = tokio::spawn(async move {
        let mut link = FramedLink {
            reader: member_read,
            writer: member_write,
        };
        sync::receive(&mut link, &location, &sources)
            .await
            .map(|(_, stats)| stats)
    });
    (caller.await.unwrap(), member.await.unwrap())
}

/// What git and the file system show of a working tree.
fn appearance(root: &Path) -> Vec<String> {
    let mut seen = vec![
        text(
            root,
            &[
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=all",
            ],
        ),
        text(root, &["diff", "--cached", "--binary"]),
        text(root, &["diff", "--binary"]),
        // Empty before the first commit. Only HEAD's branch and upstream are carried.
        lenient(root, &["log", "--format=%H %P %s", "--graph", "HEAD"]),
        lenient(root, &["rev-list", "--count", "HEAD"]),
        text(root, &["ls-files", "-s"]),
    ];
    // Every listed file's bytes, mode and links.
    let listed = text(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    );
    let mut paths: Vec<&str> = listed.split('\0').filter(|p| !p.is_empty()).collect();
    paths.sort();
    paths.dedup();
    for path in paths {
        let full = root.join(path);
        match std::fs::symlink_metadata(&full) {
            Ok(meta) if meta.file_type().is_symlink() => seen.push(format!(
                "{path} -> {:?}",
                std::fs::read_link(&full).unwrap()
            )),
            Ok(meta) => seen.push(format!(
                "{path} {:o} {:x?}",
                meta.permissions().mode() & 0o777,
                elsecall::workspace::digest::Digest::of(&std::fs::read(&full).unwrap())
            )),
            Err(_) => seen.push(format!("{path} absent")),
        }
    }
    seen
}

fn assert_same(caller: &Path, copy: &Path) {
    let (a, b) = (appearance(caller), appearance(copy));
    for (left, right) in a.iter().zip(&b) {
        assert_eq!(left, right, "the copy differs from the caller");
    }
    assert_eq!(a.len(), b.len());
}

/// A caller repository with history on a branch that tracks origin/main, two
/// commits that exist only locally, and every kind of uncommitted change.
fn dirty_caller(w: &World) {
    let c = &w.caller;
    write(c, ".gitignore", "build/\n*.log\n.env\n");
    write(c, "README.md", "hello\n");
    write(c, "src/lib.rs", "fn main() {}\n");
    write(c, "src/old_name.rs", "// to be renamed\n");
    write(c, "deleted.txt", "goes away\n");
    write(c, "staged_deleted.txt", "removed from the index\n");
    write(c, "partial.txt", "one\n");
    write(c, "tool.sh", "#!/bin/sh\necho tool\n");
    git(c, &["add", "-A"]);
    git(c, &["commit", "-q", "-m", "initial"]);
    git(c, &["push", "-q", "-u", "origin", "main"]);
    write(c, "src/lib.rs", "fn main() { println!(\"local\"); }\n");
    git(c, &["commit", "-q", "-am", "local only 1"]);
    write(c, "README.md", "hello\nlocal\n");
    git(c, &["commit", "-q", "-am", "local only 2"]);
    // Staged and unstaged edits, a partially staged file, deletions and a rename.
    write(c, "README.md", "hello\nlocal\nstaged\n");
    git(c, &["add", "README.md"]);
    write(c, "partial.txt", "one\ntwo\n");
    git(c, &["add", "partial.txt"]);
    write(c, "partial.txt", "one\ntwo\nthree (unstaged)\n");
    std::fs::remove_file(c.join("deleted.txt")).unwrap();
    git(c, &["rm", "-q", "staged_deleted.txt"]);
    git(c, &["mv", "src/old_name.rs", "src/new_name.rs"]);
    // An executable bit staged, binary content staged and untracked.
    std::fs::set_permissions(c.join("tool.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    git(c, &["add", "tool.sh"]);
    let binary: Vec<u8> = (0..70_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
        .collect();
    write(c, "assets/staged.bin", &binary);
    git(c, &["add", "assets/staged.bin"]);
    write(c, "assets/untracked.bin", &binary[..1000]);
    // Untracked files, links inside and pointing out of the tree.
    write(c, "notes/new.md", "untracked\n");
    std::os::unix::fs::symlink("../README.md", c.join("notes/readme-link")).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", c.join("hosts-link")).unwrap();
    std::os::unix::fs::symlink("../../outside", c.join("outside-link")).unwrap();
    // Ignored files, by .gitignore and by info/exclude, are never copied.
    write(c, "build/cache.o", "build state");
    write(c, "debug.log", "log");
    write(c, ".env", "SECRET=1");
    write(c, "local-only.cfg", "machine config");
    std::fs::write(c.join(".git/info/exclude"), "local-only.cfg\n").unwrap();
    std::fs::create_dir_all(c.join("src/deep/er")).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dirty_tree_arrives_exactly_and_ignored_files_stay_behind() {
    let w = world();
    dirty_caller(&w);
    let located = w.locate(&w.caller.join("src/deep/er"));
    assert_eq!(located.prefix.as_ref().unwrap().as_str(), "src/deep/er");
    let location = w.copy_location(&located, Setting::Mapped);
    let (stats, member) = sync_into(&located, &w.caches, &location).await;
    assert!(member.full_manifest && member.full_index);
    assert!(member.pack_bytes > 0, "history was sent");
    assert!(stats.sent_bytes > 0);

    let copy = &location.root;
    assert!(copy.starts_with(w.member.join("workspaces/alpha")));
    assert_same(&w.caller, copy);
    let status = text(copy, &["status", "--porcelain=v2", "--branch"]);
    assert!(status.contains("# branch.head main"), "{status}");
    assert!(status.contains("# branch.upstream origin/main"), "{status}");
    assert!(
        status.contains("# branch.ab +2 -0"),
        "local-only commits: {status}"
    );
    assert!(
        status.contains("R. N... 100644 100644 100644"),
        "rename: {status}"
    );
    for ignored in ["build/cache.o", "debug.log", ".env", "local-only.cfg"] {
        assert!(!copy.join(ignored).exists(), "{ignored} was copied");
    }
    assert_eq!(
        std::fs::read_link(copy.join("hosts-link")).unwrap(),
        Path::new("/etc/hosts")
    );
    assert_eq!(
        std::fs::read_link(copy.join("outside-link")).unwrap(),
        Path::new("../../outside")
    );
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&copy.join("tool.sh")), 0o755);
    assert_eq!(mode(copy) & 0o077, 0, "copies are owner-only");
    // The remote's URL never reaches the copy; only the tracking configuration.
    let config = std::fs::read_to_string(copy.join(".git/config")).unwrap();
    assert!(!config.contains(w.remote.to_str().unwrap()), "{config}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_syncs_move_only_changes_and_keep_ignored_build_state() {
    let w = world();
    dirty_caller(&w);
    let located = w.locate(&w.caller);
    let location = w.copy_location(&located, Setting::Mapped);
    sync_into(&located, &w.caches, &location).await;
    let copy = location.root.clone();

    // Unchanged: nothing moves but digests.
    let (stats, member) = sync_into(&located, &w.caches, &location).await;
    assert_eq!(
        (member.written, member.removed, member.received_files),
        (0, 0, 0)
    );
    assert_eq!(member.pack_bytes, 0);
    assert!(!member.full_manifest && !member.full_index);
    assert_eq!(stats.hashed_files, 0, "nothing was hashed again");
    assert!(stats.sent_bytes < 200, "only empty differences: {stats:?}");

    // A run builds, edits a tracked file, leaves an untracked file, stages and
    // commits in the copy. Its ignored build state must survive the next sync.
    write(&copy, "build/out.o", "warm build state");
    write(&copy, "target.log", "ignored log");
    write(&copy, "src/lib.rs", "changed by the run\n");
    write(&copy, "stray.txt", "left by the run");
    git(&copy, &["add", "notes/new.md"]);
    git(
        &copy,
        &["commit", "-q", "-m", "run committed", "--allow-empty"],
    );
    // The caller edits one file and renames another.
    write(&w.caller, "notes/new.md", "untracked, edited\n");
    std::fs::rename(
        w.caller.join("assets/untracked.bin"),
        w.caller.join("assets/moved.bin"),
    )
    .unwrap();
    let (stats, member) = sync_into(&located, &w.caches, &location).await;
    assert_eq!(
        member.dirty, 1,
        "the tracked file the run edited: {member:?}"
    );
    assert_eq!(member.extras_removed, 1, "the run's stray file: {member:?}");
    assert_eq!(
        member.reused_local, 1,
        "the rename moved no content: {member:?}"
    );
    assert!(member.received_files <= 2, "{member:?}");
    assert!(!member.full_manifest, "{member:?}");
    assert_eq!(stats.hashed_files, 2, "{stats:?}");
    assert_eq!(
        std::fs::read_to_string(copy.join("build/out.o")).unwrap(),
        "warm build state"
    );
    assert_eq!(
        std::fs::read_to_string(copy.join("target.log")).unwrap(),
        "ignored log"
    );
    assert!(!copy.join("stray.txt").exists());
    assert_same(&w.caller, &copy);

    // A new commit on the caller sends a small pack of only the new objects.
    git(&w.caller, &["add", "-A"]);
    git(&w.caller, &["commit", "-q", "-m", "everything"]);
    let (_, member) = sync_into(&located, &w.caches, &location).await;
    assert!(
        member.pack_bytes > 0 && member.pack_bytes < 4096,
        "{member:?}"
    );
    assert_same(&w.caller, &copy);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_caller_cache_falls_back_to_full_listings() {
    let w = world();
    dirty_caller(&w);
    let located = w.locate(&w.caller);
    let location = w.copy_location(&located, Setting::Mapped);
    sync_into(&located, &w.caches, &location).await;
    std::fs::remove_dir_all(&w.caches).unwrap();
    std::fs::create_dir_all(&w.caches).unwrap();
    let (_, member) = sync_into(&located, &w.caches, &location).await;
    assert!(member.full_manifest && member.full_index);
    assert_eq!(
        (member.written, member.received_files),
        (0, 0),
        "content was already there"
    );
    assert_same(&w.caller, &location.root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submodules_appear_as_not_checked_out_rather_than_deleted() {
    let w = world();
    write(&w.caller, "a.txt", "a\n");
    git(&w.caller, &["add", "a.txt"]);
    git(&w.caller, &["commit", "-q", "-m", "one"]);
    let commit = text(&w.caller, &["rev-parse", "HEAD"]);
    let entry = format!("160000,{},vendor/lib", commit.trim());
    git(&w.caller, &["update-index", "--add", "--cacheinfo", &entry]);
    git(&w.caller, &["commit", "-q", "-m", "submodule"]);
    std::fs::create_dir_all(w.caller.join("vendor/lib")).unwrap();
    let located = w.locate(&w.caller);
    let location = w.copy_location(&located, Setting::Mapped);
    sync_into(&located, &w.caches, &location).await;
    assert!(location.root.join("vendor/lib").is_dir());
    assert_eq!(
        text(&location.root, &["status", "--short"]),
        text(&w.caller, &["status", "--short"])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repository_without_commits_and_a_detached_head() {
    let w = world();
    write(&w.caller, "a.txt", "first\n");
    git(&w.caller, &["add", "a.txt"]);
    let located = w.locate(&w.caller);
    let location = w.copy_location(&located, Setting::Mapped);
    sync_into(&located, &w.caches, &location).await;
    assert_same(&w.caller, &location.root);
    git(&w.caller, &["commit", "-q", "-m", "one"]);
    write(&w.caller, "a.txt", "second\n");
    git(&w.caller, &["commit", "-q", "-am", "two"]);
    git(&w.caller, &["checkout", "-q", "--detach", "HEAD~1"]);
    sync_into(&located, &w.caches, &location).await;
    assert_same(&w.caller, &location.root);
    assert!(
        text(&location.root, &["status", "--porcelain=v2", "--branch"])
            .contains("# branch.head (detached)")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_path_copies_live_at_the_callers_path_and_never_overwrite_strangers() {
    let w = world();
    dirty_caller(&w);
    let located = w.locate(&w.caller);
    let location = w.copy_location(&located, Setting::SamePath);
    assert_eq!(
        location.root,
        w.member
            .join("same")
            .join(w.caller.strip_prefix("/").unwrap())
    );
    sync_into(&located, &w.caches, &location).await;
    assert_same(&w.caller, &location.root);

    // Another calling member's workspace at the same path is refused clearly; it
    // would otherwise see this member's ignored files.
    let other_member = w
        .locations(Setting::SamePath)
        .locate("gamma", &located.root_text, &located.common_dir)
        .unwrap();
    assert_eq!(other_member.root, location.root);
    let refused = refused_sync(&located, &w.caches, &other_member).await;
    assert!(
        refused.message.contains("another calling member"),
        "{}",
        refused.message
    );

    // A directory there that is not an Elsecall copy is refused, untouched, even
    // when it is empty.
    let other = w.world_with_stranger();
    let located = w.locate(&other);
    let location = w.copy_location(&located, Setting::SamePath);
    std::fs::create_dir_all(&location.root).unwrap();
    assert_eq!(
        refused_sync(&located, &w.caches, &location).await.code,
        "workspace_sync_failed"
    );
    std::fs::write(location.root.join("precious"), "keep").unwrap();
    assert_eq!(
        refused_sync(&located, &w.caches, &location).await.code,
        "workspace_sync_failed"
    );
    assert_eq!(
        std::fs::read_to_string(location.root.join("precious")).unwrap(),
        "keep"
    );
}

/// Runs a sync the member must refuse, and returns its failure.
async fn refused_sync(
    located: &Located,
    caches: &Path,
    location: &CopyLocation,
) -> elsecall::failure::Failure {
    let (a, b) = tokio::io::duplex(1 << 20);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let caches = CacheFiles::new(caches, &located.root_text);
    let caller = tokio::spawn({
        let located = located.clone();
        async move {
            sync::push(
                &mut FramedLink {
                    reader: ar,
                    writer: aw,
                },
                &located,
                &caches,
                "bravo",
                &[],
            )
            .await
        }
    });
    let refused = sync::receive(
        &mut FramedLink {
            reader: br,
            writer: bw,
        },
        location,
        &ObjectSources::default(),
    )
    .await;
    caller.abort();
    refused.err().expect("the member refuses")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_that_restores_size_and_time_is_still_undone() {
    let w = world();
    dirty_caller(&w);
    let located = w.locate(&w.caller);
    let location = w.copy_location(&located, Setting::Mapped);
    sync_into(&located, &w.caches, &location).await;
    let file = location.root.join("partial.txt");
    let original = std::fs::read(&file).unwrap();
    let mut edited = original.clone();
    edited[0] = b'X';
    let reference = location.root.join("reference");
    std::fs::copy(&file, &reference).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&file, &edited).unwrap();
    let touched = crate::processes::command("touch")
        .arg("-r")
        .arg(&reference)
        .arg(&file)
        .status()
        .unwrap();
    assert!(touched.success());
    std::fs::remove_file(&reference).unwrap();
    let (_, member) = sync_into(&located, &w.caches, &location).await;
    assert_eq!(member.dirty, 1, "{member:?}");
    assert_eq!(std::fs::read(&file).unwrap(), original);
    assert_same(&w.caller, &location.root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupted_first_setup_is_completed_or_cleared() {
    let w = world();
    dirty_caller(&w);
    let located = w.locate(&w.caller);
    // A mapped copy left without its marker is Elsecall's own and is cleared.
    let mapped = w.copy_location(&located, Setting::Mapped);
    std::fs::create_dir_all(mapped.root.join(".git")).unwrap();
    std::fs::write(mapped.root.join("half-written"), "x").unwrap();
    sync_into(&located, &w.caches, &mapped).await;
    assert!(!mapped.root.join("half-written").exists());
    assert_same(&w.caller, &mapped.root);
    // A marked same-path copy whose setup stopped before it was ready is completed.
    let same = w.copy_location(&located, Setting::SamePath);
    sync_into(&located, &w.caches, &same).await;
    std::fs::remove_file(same.root.join(".git/elsecall-ready")).unwrap();
    std::fs::remove_file(same.root.join(".git/objects/info/alternates")).unwrap();
    sync_into(&located, &w.caches, &same).await;
    assert_same(&w.caller, &same.root);
}

impl World {
    fn world_with_stranger(&self) -> PathBuf {
        let other = self.caller.parent().unwrap().join("other");
        std::fs::create_dir_all(&other).unwrap();
        git(&other, &["init", "-q", "-b", "main"]);
        write(&other, "f", "x");
        git(&other, &["add", "f"]);
        git(&other, &["commit", "-q", "-m", "x"]);
        other
    }
}

#[test]
fn outside_a_working_tree_there_is_nothing_to_copy() {
    let dir = tempfile::tempdir().unwrap();
    assert!(snapshot::locate(dir.path()).unwrap().is_none());
    let w = world();
    assert!(
        snapshot::locate(&w.caller.join(".git")).unwrap().is_none(),
        "inside .git"
    );
}

/// Performance: a tree of 20,000 files. The baseline is the first, cold sync; a
/// repeated sync of the unchanged tree must stay well under a second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn perf_repeated_sync_of_a_large_unchanged_tree_is_fast() {
    let w = world();
    let c = &w.caller;
    for dir in 0..200 {
        for file in 0..100 {
            write(
                c,
                &format!("module{dir:03}/source{file:03}.swift"),
                format!("// {dir} {file}\nlet value = {};\n", dir * file),
            );
        }
    }
    git(c, &["add", "-A"]);
    git(c, &["commit", "-q", "-m", "large"]);
    let located = w.locate(c);
    let location = w.copy_location(&located, Setting::Mapped);
    let started = Instant::now();
    let (cold, _) = sync_into(&located, &w.caches, &location).await;
    let cold_ms = started.elapsed().as_millis();
    let mut warm = Vec::new();
    for _ in 0..3 {
        let started = Instant::now();
        let (stats, member) = sync_into(&located, &w.caches, &location).await;
        assert_eq!(member.written, 0);
        warm.push((started.elapsed().as_millis(), stats.sent_bytes));
    }
    write(c, "module007/source042.swift", "// changed\n");
    let started = Instant::now();
    let (one, member) = sync_into(&located, &w.caches, &location).await;
    let one_ms = started.elapsed().as_millis();
    assert_eq!(member.written, 1);
    eprintln!(
        "PERF 20000 files: cold {cold_ms} ms ({} bytes sent), warm unchanged {:?} (ms, bytes), one change {one_ms} ms ({} bytes)",
        cold.sent_bytes, warm, one.sent_bytes
    );
    let best = warm.iter().map(|(ms, _)| *ms).min().unwrap();
    if performance_budgets() {
        assert!(
            best * 3 < cold_ms,
            "warm {best} ms against cold {cold_ms} ms"
        );
    }
    assert!(
        warm.iter().all(|(_, bytes)| *bytes < 200),
        "only digests move: {warm:?}"
    );
    // Unoptimized test builds are several times slower; the absolute bound holds
    // for the release build that is installed.
    if performance_budgets() && !cfg!(debug_assertions) {
        assert!(best < 1000, "warm unchanged sync took {best} ms");
    }
}

/// A member's own clone of the caller's remote, as an operator would configure as
/// an object source, and its refs and status, to show it is only read.
fn own_clone(w: &World, name: &str, args: &[&str]) -> (PathBuf, String) {
    let clone = w.member.join(name);
    let remote = format!("file://{}", w.remote.display());
    let mut command = vec!["clone", "-q"];
    command.extend_from_slice(args);
    command.extend_from_slice(&[&remote, clone.to_str().unwrap()]);
    git(&w.member, &command);
    let state = source_state(&clone);
    (clone, state)
}
fn source_state(clone: &Path) -> String {
    text(clone, &["for-each-ref"]) + &text(clone, &["status", "--porcelain"])
}

/// The files of a member's object store outside its objects that mention
/// `needle`, to show nothing of a source's configuration or remotes arrives there.
fn store_mentions(store: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![store.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if !path.ends_with("objects") {
                    pending.push(path);
                }
            } else if std::fs::read(&path)
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle.as_bytes())
            {
                found.push(path);
            }
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_comes_from_a_members_own_clone_and_the_copy_is_still_exact() {
    let w = world();
    dirty_caller(&w);
    let (source, before) = own_clone(&w, "own-clone", &[]);
    let unrelated = w.member.join("unrelated");
    std::fs::create_dir_all(&unrelated).unwrap();
    git(&unrelated, &["init", "-q"]);
    write(&unrelated, "other.txt", "another project\n");
    git(&unrelated, &["add", "-A"]);
    git(&unrelated, &["commit", "-q", "-m", "other"]);
    let sources = ObjectSources::new(vec![w.member.join("missing"), unrelated, source.clone()]);
    let located = w.locate(&w.caller);

    let plain = w.calling_member_location(&located, "alpha");
    let (_, without) = sync_into(&located, &w.caches, &plain).await;
    let sourced = w.calling_member_location(&located, "gamma");
    let (_, with) = sync_with(&located, &w.caches, &sourced, &sources, None).await;
    assert!(!without.history_from_source && with.history_from_source);
    assert!(
        with.pack_bytes < without.pack_bytes,
        "only local-only commits come from the caller: {} against {}",
        with.pack_bytes,
        without.pack_bytes
    );

    assert_same(&w.caller, &sourced.root);
    assert_same(&w.caller, &plain.root);
    let status = text(&sourced.root, &["status", "--porcelain=v2", "--branch"]);
    assert!(status.contains("# branch.upstream origin/main"), "{status}");
    assert!(status.contains("# branch.ab +2 -0"), "{status}");
    // The source is only read, and neither its remotes nor the caller's reach the
    // copy or the store.
    assert_eq!(source_state(&source), before);
    let remote = w.remote.to_str().unwrap();
    let config = std::fs::read_to_string(sourced.root.join(".git/config")).unwrap();
    assert!(!config.contains(remote), "{config}");
    assert_eq!(
        store_mentions(&sourced.store, remote),
        Vec::<PathBuf>::new()
    );
    let quarantines = sourced.store.join("elsecall-incoming");
    assert!(!quarantines.exists() || quarantines.read_dir().unwrap().next().is_none());

    // A later commit comes from the caller against what the store now holds.
    write(&w.caller, "later.txt", "later\n");
    git(&w.caller, &["add", "later.txt"]);
    git(&w.caller, &["commit", "-q", "-m", "later"]);
    let (_, later) = sync_with(&located, &w.caches, &sourced, &sources, None).await;
    assert!(later.pack_bytes < 4096, "{later:?}");
    assert!(
        !later.history_from_source && later.phases.sources == 0,
        "the store holds the upstream, so no source is asked: {later:?}"
    );
    assert_same(&w.caller, &sourced.root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shallow_source_supplies_nothing_and_the_copy_is_still_exact() {
    let w = world();
    dirty_caller(&w);
    // HEAD is the upstream and the shallow clone's tip, without its parent, so
    // only the source could supply history: a commit kept without it would mean
    // the caller sends none.
    git(&w.caller, &["push", "-q", "origin", "main"]);
    let (shallow, before) = own_clone(&w, "shallow", &["--depth", "1"]);
    let sources = ObjectSources::new(vec![shallow.clone()]);
    let located = w.locate(&w.caller);
    let location = w.calling_member_location(&located, "gamma");
    let (_, member) = sync_with(&located, &w.caches, &location, &sources, None).await;
    assert!(!member.history_from_source, "{member:?}");
    assert!(member.pack_bytes > 0, "history came from the caller");
    assert!(
        text(&location.root, &["status", "--porcelain=v2", "--branch"])
            .contains("# branch.ab +0 -0")
    );
    assert_same(&w.caller, &location.root);
    let log = |dir: &Path| text(dir, &["log", "--format=%H %P %s"]);
    assert_eq!(log(&location.root), log(&w.caller));
    assert_eq!(source_state(&shallow), before);
}

/// A repository whose history dwarfs its working tree, as long-lived projects'
/// does: 300 commits rewriting ten of 100 files of incompressible content each.
fn long_history(w: &World) {
    let mut stream = Vec::new();
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for commit in 1..=300u32 {
        writeln!(stream, "commit refs/heads/main\nmark :{commit}").unwrap();
        writeln!(
            stream,
            "committer Test <test@example.com> {} +0000",
            1_700_000_000 + commit
        )
        .unwrap();
        let message = format!("history {commit}");
        write!(stream, "data {}\n{message}\n", message.len()).unwrap();
        if commit > 1 {
            writeln!(stream, "from :{}", commit - 1).unwrap();
        }
        for file in 0..10u32 {
            let content: Vec<u8> = (0..10_240)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            let path = format!("data/file{:03}.bin", (commit * 10 + file) % 100);
            writeln!(stream, "M 100644 inline {path}\ndata {}", content.len()).unwrap();
            stream.extend_from_slice(&content);
            stream.push(b'\n');
        }
    }
    let mut import = crate::processes::command("git")
        .current_dir(&w.caller)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    import.stdin.take().unwrap().write_all(&stream).unwrap();
    assert!(import.wait().unwrap().success());
    git(&w.caller, &["reset", "-q", "--hard", "main"]);
    git(&w.caller, &["push", "-q", "-u", "origin", "main"]);
}

/// Performance: the first sync of a workspace whose history is large, over a link
/// of 32 MB/s. The baseline is the first sync without object sources; with the
/// member's own clone as one, only the commits since that clone, the local-only
/// commits and the working tree's changes cross the link.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn perf_a_first_sync_with_an_object_source_sends_only_local_work() {
    const LINK: u64 = 32_000_000;
    let w = world();
    long_history(&w);
    // The member's clone predates the caller's latest pushed commit, so its
    // history is found through an ancestor of the caller's HEAD.
    let (source, _) = own_clone(&w, "own-clone", &[]);
    dirty_caller(&w);
    let located = w.locate(&w.caller);

    let plain = w.calling_member_location(&located, "alpha");
    let started = Instant::now();
    let (baseline, without) = sync_with(
        &located,
        &w.caches,
        &plain,
        &ObjectSources::default(),
        Some(LINK),
    )
    .await;
    let baseline_ms = started.elapsed().as_millis();
    let sourced = w.calling_member_location(&located, "gamma");
    let sources = ObjectSources::new(vec![source]);
    let started = Instant::now();
    let (first, with) = sync_with(&located, &w.caches, &sourced, &sources, Some(LINK)).await;
    let first_ms = started.elapsed().as_millis();
    let started = Instant::now();
    let (warm, unchanged) = sync_with(&located, &w.caches, &sourced, &sources, Some(LINK)).await;
    let warm_ms = started.elapsed().as_millis();
    eprintln!(
        "PERF first sync of a 300-commit history: without a source {baseline_ms} ms ({} bytes sent, pack {}, phases {:?}), with one {first_ms} ms ({} bytes sent, pack {}, phases {:?}); warm {warm_ms} ms ({} bytes)",
        baseline.sent_bytes,
        without.pack_bytes,
        without.phases,
        first.sent_bytes,
        with.pack_bytes,
        with.phases,
        warm.sent_bytes
    );
    assert_same(&w.caller, &sourced.root);
    assert_same(&w.caller, &plain.root);
    assert!(with.history_from_source && !without.history_from_source);
    assert!(
        with.pack_bytes * 100 < without.pack_bytes,
        "pack {} against {}",
        with.pack_bytes,
        without.pack_bytes
    );
    // History is what the source changes: taking it from the source must cost a
    // fraction of receiving it, and the whole sync must be faster.
    if performance_budgets() {
        let history =
            |phases: &elsecall::workspace::sync::MemberPhases| phases.sources + phases.pack;
        assert!(
            history(&with.phases) * 3 < history(&without.phases),
            "history {:?} against {:?} without a source",
            with.phases,
            without.phases
        );
        assert!(
            first_ms < baseline_ms,
            "first sync {first_ms} ms against {baseline_ms} ms without a source"
        );
    }
    assert_eq!(unchanged.written, 0);
    assert!(
        warm.sent_bytes < 200,
        "only digests move: {}",
        warm.sent_bytes
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_with_a_shallow_branch_supplies_only_complete_history() {
    let w = world();
    dirty_caller(&w);
    // The caller's HEAD is on the remote as `feature`. The member's clone has main
    // in full but `feature` fetched shallow: its tip is there without its parent.
    git(&w.caller, &["push", "-q", "origin", "HEAD:feature"]);
    let (source, _) = own_clone(&w, "own-clone", &["--single-branch", "--branch", "main"]);
    git(
        &source,
        &["fetch", "-q", "--depth", "1", "origin", "feature"],
    );
    assert!(source.join(".git/shallow").exists());
    let sources = ObjectSources::new(vec![source]);
    let located = w.locate(&w.caller);
    let location = w.calling_member_location(&located, "gamma");
    let (_, member) = sync_with(&located, &w.caches, &location, &sources, None).await;
    // The shallow tip fails to import; main's history comes from the source, and
    // the commits above it from the caller.
    assert!(member.history_from_source, "{member:?}");
    assert!(
        member.pack_bytes > 0,
        "local-only commits came from the caller"
    );
    assert_same(&w.caller, &location.root);
    let log = |dir: &Path| text(dir, &["log", "--format=%H %P %s"]);
    assert_eq!(log(&location.root), log(&w.caller));
    git(
        &location.root,
        &["fsck", "--connectivity-only", "--no-dangling"],
    );
}

/// Another clone of the caller's remote, where others push: main moves on, and
/// release branches, a feature branch and tags appear. The caller fetches them
/// and learns the remote's default branch.
fn others_push(w: &World) -> PathBuf {
    let other = w.member.join("others");
    let remote = format!("file://{}", w.remote.display());
    git(
        &w.member,
        &["clone", "-q", &remote, other.to_str().unwrap()],
    );
    for n in 1..=3 {
        write(&other, "main.txt", format!("main {n}\n"));
        git(&other, &["add", "-A"]);
        git(&other, &["commit", "-q", "-m", &format!("main {n}")]);
    }
    git(&other, &["push", "-q", "origin", "main"]);
    for (branch, from) in [
        ("release/1.0", "main~2"),
        ("release/2.0", "main~1"),
        ("their-feature", "main"),
    ] {
        git(&other, &["switch", "-q", "-c", branch, from]);
        write(&other, "branch.txt", format!("{branch}\n"));
        git(&other, &["add", "-A"]);
        git(&other, &["commit", "-q", "-m", branch]);
        git(&other, &["push", "-q", "origin", branch]);
    }
    git(&other, &["switch", "-q", "main"]);
    git(&other, &["tag", "-a", "-m", "one", "v1.0", "release/1.0"]);
    git(&other, &["tag", "v0.9", "main~3"]);
    git(&other, &["push", "-q", "origin", "--tags"]);
    git(&w.caller, &["fetch", "-q", "origin", "--tags"]);
    git(&w.caller, &["remote", "set-head", "origin", "main"]);
    other
}

/// What scripts comparing against the default branch and the upstream see.
fn comparisons(dir: &Path) -> Vec<String> {
    [
        &["merge-base", "HEAD", "origin/main"][..],
        &["rev-parse", "origin/HEAD"],
        &["symbolic-ref", "refs/remotes/origin/HEAD"],
        &["rev-list", "--count", "HEAD..origin/main"],
        &["rev-list", "--count", "origin/main..HEAD"],
        &["rev-parse", "@{upstream}"],
        &["log", "--format=%H %P %s", "origin/main"],
        &[
            "merge-base",
            "--is-ancestor",
            "origin/release/1.0",
            "origin/main",
        ],
    ]
    .iter()
    .map(|args| lenient(dir, args))
    .collect()
}

/// The refs under `patterns`, with their objects and symbolic targets.
fn ref_listing(dir: &Path, patterns: &[&str]) -> String {
    let mut args = vec![
        "for-each-ref",
        "--format=%(refname) %(objectname) %(symref)",
    ];
    args.extend_from_slice(patterns);
    text(dir, &args)
}

/// A sync that carries `declared` refs and must succeed on both sides.
async fn sync_declared(
    located: &Located,
    caches: &Path,
    location: &CopyLocation,
    declared: &[&str],
) -> (SyncStats, MemberStats) {
    let (caller, member) = sync_carrying(
        located,
        caches,
        location,
        &ObjectSources::default(),
        None,
        declared,
    )
    .await;
    (caller.expect("caller side"), member.expect("member side"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_copy_carries_the_callers_upstream_default_branch_and_declared_refs() {
    let w = world();
    dirty_caller(&w);
    // HEAD is a branch whose upstream is not the default branch, a commit ahead
    // of it, while the default branch moved on.
    git(&w.caller, &["switch", "-q", "-c", "work"]);
    git(&w.caller, &["push", "-q", "-u", "origin", "work"]);
    git(
        &w.caller,
        &["commit", "-q", "--allow-empty", "-m", "after the push"],
    );
    let other = others_push(&w);
    let declared = ["refs/remotes/origin/release/*", "refs/tags/v1*"];
    let carried = [
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
        "refs/remotes/origin/work",
        "refs/remotes/origin/release/*",
        "refs/tags/v1*",
    ];
    let located = w.locate(&w.caller);
    let location = w.copy_location(&located, Setting::Mapped);
    let copy = &location.root;

    let (stats, member) = sync_declared(&located, &w.caches, &location, &declared).await;
    assert_same(&w.caller, copy);
    assert_eq!(comparisons(copy), comparisons(&w.caller));
    assert_eq!(
        lenient(copy, &["rev-list", "--count", "HEAD..origin/main"]),
        "3\n"
    );
    // Exactly the carried refs: not their-feature, and not v0.9.
    assert_eq!(
        ref_listing(copy, &["refs/remotes", "refs/tags"]),
        ref_listing(&w.caller, &carried)
    );
    assert!(ref_listing(&w.caller, &["refs/remotes/origin/their-feature"]).len() > 1);
    assert_eq!(text(copy, &["cat-file", "-t", "v1.0"]), "tag\n");
    assert_eq!(text(copy, &["describe", "origin/release/1.0"]), "v1.0\n");
    git(copy, &["fsck", "--connectivity-only", "--no-dangling"]);
    let names: Vec<String> = [
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
        "refs/remotes/origin/release/1.0",
        "refs/remotes/origin/release/2.0",
        "refs/remotes/origin/work",
        "refs/tags/v1.0",
    ]
    .map(String::from)
    .to_vec();
    assert_eq!(stats.refs, names);
    assert_eq!(member.refs, names);
    // origin/main's three commits and one on each release branch.
    assert_eq!(stats.refs_only_commits, Some(5), "{stats:?}");

    // No remote URL reaches the copy or its store, so a fetch in the copy fails.
    let remote = w.remote.to_str().unwrap();
    let config = std::fs::read_to_string(copy.join(".git/config")).unwrap();
    assert!(!config.contains(remote), "{config}");
    assert_eq!(
        store_mentions(&location.store, remote),
        Vec::<PathBuf>::new()
    );
    for fetch in [&["fetch", "-q"][..], &["fetch", "-q", "origin"]] {
        let status = crate::processes::command("git")
            .current_dir(copy)
            .args(fetch)
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "git {fetch:?} succeeded in the copy");
    }

    // The caller's tracking refs move: main on, and release/2.0 deleted. A run
    // in the copy had changed refs of its own.
    write(&other, "main.txt", "main 4\n");
    git(&other, &["commit", "-q", "-am", "main 4"]);
    git(&other, &["push", "-q", "origin", "main", ":release/2.0"]);
    git(&w.caller, &["fetch", "-q", "--prune", "origin"]);
    git(copy, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(copy, &["update-ref", "refs/remotes/origin/junk", "HEAD"]);
    git(copy, &["tag", "v1.run"]);
    git(
        copy,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/junk",
        ],
    );
    let (stats, member) = sync_declared(&located, &w.caches, &location, &declared).await;
    assert_eq!(comparisons(copy), comparisons(&w.caller));
    assert_eq!(
        lenient(copy, &["rev-list", "--count", "HEAD..origin/main"]),
        "4\n"
    );
    assert_eq!(
        ref_listing(copy, &["refs/remotes", "refs/tags"]),
        ref_listing(&w.caller, &carried)
    );
    assert!(
        !stats.refs.iter().any(|r| r.ends_with("release/2.0")),
        "{stats:?}"
    );
    assert!(
        member.pack_bytes > 0 && member.pack_bytes < 4096,
        "{member:?}"
    );
    assert_eq!(stats.refs_only_commits, Some(1), "{stats:?}");

    // A run leaves what git does not list but blocks a carried ref: a symbolic
    // ref to nothing where release/1.0 needs a directory, and another beside it.
    git(
        copy,
        &["update-ref", "-d", "refs/remotes/origin/release/1.0"],
    );
    for (name, target) in [
        ("refs/remotes/origin/release", "refs/remotes/nothing"),
        ("refs/tags/dangling", "refs/tags/nothing"),
    ] {
        git(copy, &["symbolic-ref", name, target]);
    }
    sync_declared(&located, &w.caches, &location, &declared).await;
    assert_eq!(comparisons(copy), comparisons(&w.caller));
    assert_eq!(
        ref_listing(copy, &["refs/remotes", "refs/tags"]),
        ref_listing(&w.caller, &carried)
    );
    assert!(!copy.join(".git/refs/tags/dangling").exists());

    // Unchanged, a sync moves digests only.
    let (warm, member) = sync_declared(&located, &w.caches, &location, &declared).await;
    assert!(warm.sent_bytes < 200, "{warm:?}");
    assert_eq!((member.pack_bytes, warm.refs_only_commits), (0, None));

    // Without the declaration, the defaults stay, in the copy and in the store.
    sync_declared(&located, &w.caches, &location, &[]).await;
    assert_eq!(
        ref_listing(copy, &["refs/remotes", "refs/tags"]),
        ref_listing(&w.caller, &carried[..3])
    );
    let kept = text(
        &location.store,
        &["for-each-ref", "--format=%(refname)", "refs/elsecall/"],
    );
    assert!(
        !kept.contains("release") && !kept.contains("tags"),
        "{kept}"
    );
    assert!(kept.contains("/carried/remotes/origin/main\n"), "{kept}");
    assert_eq!(comparisons(copy), comparisons(&w.caller));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_default_branch_comes_from_an_object_source_first() {
    let w = world();
    dirty_caller(&w);
    git(&w.caller, &["switch", "-q", "-c", "work"]);
    git(&w.caller, &["push", "-q", "-u", "origin", "work"]);
    let other = others_push(&w);
    // The member's clone holds main as the caller fetched it, but not `work`.
    let (source, _) = own_clone(&w, "own-clone", &["--single-branch", "--branch", "main"]);
    let sources = ObjectSources::new(vec![source.clone()]);
    let located = w.locate(&w.caller);
    let location = w.calling_member_location(&located, "gamma");
    let (stats, member) = sync_with(&located, &w.caches, &location, &sources, None).await;
    assert!(member.history_from_source, "{member:?}");
    assert_eq!(comparisons(&location.root), comparisons(&w.caller));
    // Only the work branch's own commits came from the caller.
    assert!(member.pack_bytes < 4096, "{member:?}");
    assert_eq!(stats.refs_only_commits, Some(0), "{stats:?}");

    // main moves on, and both the caller and the member's clone fetch it: the
    // store lacks the caller's default branch, so the source is asked again.
    write(&other, "main.txt", "main 4\n");
    git(&other, &["commit", "-q", "-am", "main 4"]);
    git(&other, &["push", "-q", "origin", "main"]);
    git(&w.caller, &["fetch", "-q", "origin"]);
    git(&source, &["pull", "-q", "--ff-only"]);
    let (stats, member) = sync_with(&located, &w.caches, &location, &sources, None).await;
    assert!(member.history_from_source, "{member:?}");
    assert_eq!(member.pack_bytes, 0, "{member:?}");
    assert_eq!(stats.refs_only_commits, None, "no history was sent");
    assert_eq!(comparisons(&location.root), comparisons(&w.caller));
    git(
        &location.root,
        &["fsck", "--connectivity-only", "--no-dangling"],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_holding_the_upstream_and_the_default_branch_supplies_both() {
    let w = world();
    dirty_caller(&w);
    git(&w.caller, &["switch", "-q", "-c", "work"]);
    git(&w.caller, &["push", "-q", "-u", "origin", "work"]);
    others_push(&w);
    // A full clone holds both the caller's upstream, origin/work, and origin/main,
    // which has moved on from where work branched.
    let (source, _) = own_clone(&w, "own-clone", &[]);
    let sources = ObjectSources::new(vec![source]);
    let located = w.locate(&w.caller);
    let location = w.calling_member_location(&located, "gamma");
    let (stats, member) = sync_with(&located, &w.caches, &location, &sources, None).await;
    assert!(member.history_from_source, "{member:?}");
    // No history came from the caller; with one import per sync, origin/main's
    // commits since work branched would have.
    assert_eq!(stats.refs_only_commits, None, "{member:?}");
    assert_eq!(comparisons(&location.root), comparisons(&w.caller));
    assert_same(&w.caller, &location.root);
    git(
        &location.root,
        &["fsck", "--connectivity-only", "--no-dangling"],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_refs_that_match_too_many_are_refused() {
    let w = world();
    dirty_caller(&w);
    let head = text(&w.caller, &["rev-parse", "HEAD"]);
    let input: String = (0..300)
        .map(|n| format!("create refs/tags/t{n} {head}"))
        .collect();
    let mut update = crate::processes::command("git")
        .current_dir(&w.caller)
        .args(["update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    update
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    assert!(update.wait().unwrap().success());
    let located = w.locate(&w.caller);
    let location = w.copy_location(&located, Setting::Mapped);
    let (caller, member) = sync_carrying(
        &located,
        &w.caches,
        &location,
        &ObjectSources::default(),
        None,
        &["refs/tags/t*"],
    )
    .await;
    let failure = caller.unwrap_err();
    assert_eq!(failure.code, "invalid_request", "{failure:?}");
    assert!(
        failure.message.contains("[workspace] refs") && failure.hint.contains(".elsecall.toml"),
        "{failure:?}"
    );
    assert!(member.is_err(), "the member gives up with the caller");
    // Narrowed, the same workspace syncs: t1, t10–t19, t100–t199 and the upstream.
    let (stats, _) = sync_declared(&located, &w.caches, &location, &["refs/tags/t1*"]).await;
    assert_eq!(stats.refs.len(), 112);
    assert_same(&w.caller, &location.root);
}

/// Runs `git update-ref --stdin` in `dir` with `input`.
fn update_refs(dir: &Path, input: &str) {
    let mut update = crate::processes::command("git")
        .current_dir(dir)
        .args(["update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    update
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    assert!(update.wait().unwrap().success());
}

/// Performance: warm syncs of a 5,000-file tree whose caller has 300 remote
/// branches, as a busy project does, and carries its upstream, its default branch
/// and 64 declared release branches with history of their own. The baseline is
/// the same copy once the caller has no remote-tracking refs left to carry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn perf_warm_syncs_carrying_refs_cost_what_they_did_without() {
    let w = world();
    let c = &w.caller;
    for dir in 0..50 {
        for file in 0..100 {
            write(
                c,
                &format!("module{dir:03}/source{file:03}.swift"),
                format!("// {dir} {file}\n"),
            );
        }
    }
    git(c, &["add", "-A"]);
    git(c, &["commit", "-q", "-m", "large"]);
    git(c, &["push", "-q", "-u", "origin", "main"]);
    git(c, &["remote", "set-head", "origin", "main"]);
    let head = text(c, &["rev-parse", "HEAD"]);
    let tree = text(c, &["rev-parse", "HEAD^{tree}"]);
    let mut input = String::new();
    for n in 0..300 {
        input.push_str(&format!("create refs/remotes/origin/branch-{n} {head}"));
    }
    for n in 0..64 {
        let commit = text(
            c,
            &[
                "commit-tree",
                tree.trim(),
                "-p",
                head.trim(),
                "-m",
                &format!("release {n}"),
            ],
        );
        input.push_str(&format!("create refs/remotes/origin/release/{n} {commit}"));
    }
    update_refs(c, &input);
    let declared = ["refs/remotes/origin/release/*"];
    let located = w.locate(c);
    let location = w.copy_location(&located, Setting::Mapped);
    let started = Instant::now();
    let (cold, member) = sync_declared(&located, &w.caches, &location, &declared).await;
    let cold_ms = started.elapsed().as_millis();
    assert_eq!(cold.refs.len(), 66, "{:?}", cold.refs);
    assert_eq!(cold.refs_only_commits, Some(64));
    let cold_pack = member.pack_bytes;
    let warm = async |declared: &[&str]| {
        let mut times = Vec::new();
        for _ in 0..7 {
            let started = Instant::now();
            let (stats, member) = sync_declared(&located, &w.caches, &location, declared).await;
            assert_eq!((member.written, member.pack_bytes), (0, 0));
            assert!(stats.sent_bytes < 200, "{stats:?}");
            times.push(started.elapsed().as_micros() as u64);
        }
        times.sort();
        times
    };
    let carrying = warm(&declared).await;
    // Only the defaults: origin/HEAD and origin/main, which is also the upstream.
    let (settled, _) = sync_declared(&located, &w.caches, &location, &[]).await;
    assert_eq!(settled.refs.len(), 2, "{:?}", settled.refs);
    let defaults = warm(&[]).await;

    // The baseline: nothing left to carry but HEAD's branch.
    let remote_refs = text(
        c,
        &[
            "for-each-ref",
            "--format=delete %(refname)",
            "refs/remotes/",
        ],
    );
    let input: String = remote_refs
        .lines()
        .map(|line| format!("{line}\n"))
        .collect();
    git(c, &["branch", "--unset-upstream"]);
    git(c, &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
    update_refs(c, &input);
    let (settled, _) = sync_declared(&located, &w.caches, &location, &[]).await;
    assert!(settled.refs.is_empty(), "{:?}", settled.refs);
    let without = warm(&[]).await;
    let median = |times: &[u64]| times[times.len() / 2] / 1000;
    eprintln!(
        "PERF warm syncs of 5,000 files: carrying 66 refs of 366 {:?} µs (median {} ms), the 2 defaults {:?} µs (median {} ms), none {:?} µs (median {} ms); cold with refs {cold_ms} ms, pack {cold_pack} bytes",
        carrying,
        median(&carrying),
        defaults,
        median(&defaults),
        without,
        median(&without),
    );
    // Reading the caller's refs runs beside its other reads, and the copy's are
    // listed while the caller lists its tree.
    if performance_budgets() {
        assert!(
            median(&defaults) <= median(&without) * 5 / 4 + 15,
            "carrying the defaults {defaults:?} µs against {without:?} µs without"
        );
        assert!(
            median(&carrying) <= median(&without) * 5 / 4 + 15,
            "carrying refs {carrying:?} µs against {without:?} µs without"
        );
    }
}

/// Timing budgets are opt-in; normal checks still run the complete fixtures and
/// verify contents, writes, pack sizes and wire bytes. See `mise run perf`.
fn performance_budgets() -> bool {
    std::env::var_os("EC_TEST_PERF").is_some()
}
