//! Repositories on a member that may already hold a caller's history, such as the
//! member account's own clone of the same project. The member's operator names
//! them; a sync that lacks the caller's commits takes what it can from them
//! instead of receiving it over the network.
//!
//! A source is read, never written, and trusted like the member's own object
//! store: it lives in the same account, so whatever could alter its objects could
//! alter the store's too. Its objects are not hashed again, which is what makes a
//! first sync fast. What is still checked:
//! - Only commits the caller named are taken: HEAD, its upstream, a carried ref's,
//!   or one of HEAD's first-parent ancestors.
//! - A quarantine reads the store's and the source's objects in place and packs
//!   exactly the commit's history that the store lacks. Packing walks that whole
//!   history and fails on anything missing or unreadable. So a shallow, partial
//!   or damaged source, or a shallow branch in one, supplies nothing rather than
//!   a commit without its history, and nothing else of the source enters the
//!   store. Partial clones, whose git would fetch missing objects from their own
//!   remotes, are passed over before that.
//! - The store sees the pack only once its index is in place, all at once.
//! - File content taken from history is checked against the caller's digests
//!   before it is written to the copy, as all content is.
//! - The source's configuration cannot make Elsecall's git run programs or reach
//!   the network; see [`Git::member`].
//! - Nothing of the source reaches the copy except objects: not its remotes, refs
//!   or configuration.
//! - Every git command a source answers is killed when the sync is dropped, so a
//!   stalled source never outlives a sync's deadline.
use crate::workspace::git::{Git, ObjectFormat, Oid};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{io::AsyncWriteExt, task::block_in_place};

/// Most ancestors a caller may offer as candidates.
pub const MAXIMUM_BASES: usize = 32;
/// Most candidates one source is asked to import before it is passed over.
const MAXIMUM_ATTEMPTS: usize = 3;
/// Inside the object store: one quarantine per copy, as one sync at a time uses it.
const QUARANTINES: &str = "elsecall-incoming";

/// The member's configured sources, in the order the operator listed them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectSources {
    repositories: Vec<PathBuf>,
}

/// The object store a fetch lands in, and the name it is kept under there.
pub struct Destination<'a> {
    pub store: &'a Path,
    pub format: ObjectFormat,
    pub reference: &'a str,
    /// Names the quarantine, unique among the store's copies.
    pub copy_id: &'a str,
}

impl ObjectSources {
    /// Sources at `repositories`, which configuration validated as absolute paths.
    pub fn new(repositories: Vec<PathBuf>) -> Self {
        Self { repositories }
    }

    pub fn is_empty(&self) -> bool {
        self.repositories.is_empty()
    }

    /// Fetches into the destination store the first of `candidates` that a source
    /// holds as a commit, with its whole history, and returns it. Candidates come
    /// best first. A source that is missing, unreadable, partial or refuses is
    /// passed over: sources only ever save work, so their failures are never the
    /// sync's.
    pub async fn supply(&self, destination: &Destination<'_>, candidates: &[Oid]) -> Option<Oid> {
        if candidates.is_empty() {
            return None;
        }
        for repository in &self.repositories {
            let source = Git::member(repository);
            if is_partial(&source).await {
                continue;
            }
            // A commit whose history the source lacks, as on a branch fetched
            // shallow, fails to import; the next may not.
            // Each attempt may walk much of the history before it fails, so a
            // damaged source costs a bounded few before the caller sends instead.
            let held = held_commits(&source, candidates).await;
            for commit in held.into_iter().take(MAXIMUM_ATTEMPTS) {
                if import(destination, &source, &commit).await {
                    return Some(commit);
                }
            }
        }
        None
    }
}

/// Runs git, killed if the caller is dropped; its output, or `None` if it failed.
async fn run(git: &Git, args: &[&OsStr], input: Option<&[u8]>) -> Option<Vec<u8>> {
    let mut command = git.streaming(args);
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::child_start::spawn_async(&mut command).ok()?;
    if let Some(input) = input {
        // Answers come only once all input is read, or a line per candidate, so
        // writing first cannot block.
        let mut stdin = child.stdin.take()?;
        stdin.write_all(input).await.ok()?;
    }
    let output = child.wait_with_output().await.ok()?;
    output.status.success().then_some(output.stdout)
}

/// Whether `source` is a partial clone, whose git would fetch what it lacks.
async fn is_partial(source: &Git) -> bool {
    let pattern = r"^(extensions\.partialclone|remote\..*\.promisor)$";
    let args = ["config", "--get-regexp", pattern].map(OsStr::new);
    // git exits 1 when nothing matches.
    run(source, &args, None).await.is_some_and(|output| {
        String::from_utf8_lossy(&output)
            .lines()
            .any(|line| line.starts_with("extensions.partialclone ") || line.ends_with(" true"))
    })
}

/// The candidates `source` holds as commits, in order.
async fn held_commits(source: &Git, candidates: &[Oid]) -> Vec<Oid> {
    let input: String = candidates.iter().map(|c| format!("{c}\n")).collect();
    let args = ["cat-file", "--batch-check=%(objecttype)"].map(OsStr::new);
    let Some(output) = run(source, &args, Some(input.as_bytes())).await else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output);
    let kinds: Vec<&str> = text.lines().collect();
    if kinds.len() != candidates.len() {
        return Vec::new();
    }
    candidates
        .iter()
        .zip(kinds)
        .filter(|(_, kind)| *kind == "commit")
        .map(|(candidate, _)| candidate.clone())
        .collect()
}

/// Imports `commit` from `source` through a quarantine; returns whether the store
/// now holds it, with its whole history, under the destination's reference.
async fn import(destination: &Destination<'_>, source: &Git, commit: &Oid) -> bool {
    let quarantine = destination
        .store
        .join(QUARANTINES)
        .join(destination.copy_id);
    let imported = quarantined_import(destination, &quarantine, source, commit).await;
    let _ = block_in_place(|| fs::remove_dir_all(&quarantine));
    imported.is_some()
}

async fn quarantined_import(
    destination: &Destination<'_>,
    quarantine: &Path,
    source: &Git,
    commit: &Oid,
) -> Option<()> {
    let store = Git::member(destination.store);
    let args = [
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "objects",
    ]
    .map(OsStr::new);
    let objects = String::from_utf8(run(source, &args, None).await?).ok()?;
    let objects = objects.trim_end();
    // Alternates are listed a line each.
    if objects.contains('\n') {
        return None;
    }
    let args = ["for-each-ref", "--format=%(objectname)"].map(OsStr::new);
    let held = String::from_utf8(run(&store, &args, None).await?).ok()?;
    block_in_place(|| -> Option<()> {
        // A sync dropped mid-import leaves its quarantine; the next one clears it.
        let _ = fs::remove_dir_all(quarantine);
        fs::create_dir_all(quarantine.parent()?).ok()?;
        store
            .output(&[
                "init".as_ref(),
                "--bare".as_ref(),
                "-q".as_ref(),
                format!("--object-format={}", destination.format.name()).as_ref(),
                quarantine.as_os_str(),
            ])
            .ok()?;
        // The quarantine reads the store's and the source's objects in place.
        fs::write(
            quarantine.join("objects/info/alternates"),
            format!(
                "{}\n{objects}\n",
                destination.store.join("objects").display()
            ),
        )
        .ok()
    })?;
    // One pack of exactly the commit's history that the store lacks: git walks it
    // whole, reading every commit and tree and every blob it packs, and fails on
    // anything missing or unreadable, as below a shallow source's commit. It
    // reuses the source's compressed objects and deltas rather than hashing them.
    let mut revisions = format!("{commit}\n");
    for tip in held.lines().filter_map(Oid::parse) {
        revisions.push_str(&format!("^{tip}\n"));
    }
    let base = quarantine.join("objects/pack/pack");
    let args = [
        "pack-objects".as_ref(),
        "--revs".as_ref(),
        "--delta-base-offset".as_ref(),
        "-q".as_ref(),
        base.as_os_str(),
    ];
    run(&Git::member(quarantine), &args, Some(revisions.as_bytes())).await?;
    block_in_place(|| admit(destination, &store, quarantine, commit))
}

/// Moves the quarantine's pack into the store and names the commit there. The
/// store sees a pack only once its index is in place, so moving the index last
/// makes the whole history visible at once.
fn admit(destination: &Destination, store: &Git, quarantine: &Path, commit: &Oid) -> Option<()> {
    let target = destination.store.join("objects/pack");
    let mut files: Vec<PathBuf> = fs::read_dir(quarantine.join("objects/pack"))
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|e| matches!(e.to_str(), Some("pack" | "rev" | "idx")))
        })
        .collect();
    files.sort_by_key(|path| path.extension().is_some_and(|e| e == "idx"));
    for file in files {
        // A pack's name follows from its content, so one the store has is the same.
        let moved = target.join(file.file_name()?);
        if !moved.exists() {
            fs::rename(&file, moved).ok()?;
        }
    }
    if store.existing(std::slice::from_ref(commit)).ok()? != [true] {
        return None;
    }
    store
        .output(&["update-ref", destination.reference, commit.as_str()])
        .ok()?;
    Some(())
}
