//! The caller's working tree as a copy must reproduce it: where it is, its HEAD,
//! upstream and the other refs a copy carries, its ignore rules, its index, and
//! every tracked or untracked non-ignored file. Reading it changes nothing in the
//! caller's repository.
use crate::{
    failure::Result,
    workspace::{
        carried_refs::{CarriedRefs, RefPattern},
        confined::{Confined, Kind},
        digest::Digest,
        git::{Git, ObjectFormat, Oid},
        head::{BranchName, Head, Reference, RemoteName, Upstream},
        index::{GITLINK, IndexListing},
        manifest::{Entry, Manifest, PERMISSIONS},
        path::WorkspacePath,
        stat_cache::{StatCache, StatKey},
        sync_failed,
    },
};

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

/// Ignore rules larger than this are not reproduced.
pub const MAXIMUM_EXCLUDES: usize = 1024 * 1024;

/// The git working tree containing the caller's working directory.
#[derive(Clone, Debug)]
pub struct Located {
    /// The working tree's root, as git reports it.
    pub root: PathBuf,
    /// The same, as text; placed workspaces need UTF-8 paths.
    pub root_text: String,
    /// The caller's working directory inside the tree; `None` at its root.
    pub prefix: Option<WorkspacePath>,
    /// The repository's common git directory, shared by its worktrees.
    pub common_dir: String,
}

/// Finds the working tree containing `directory`. `None` outside one, including
/// inside a git directory or a bare repository.
pub fn locate(directory: &Path) -> Result<Option<Located>> {
    let git = Git::caller(directory);
    let Some(output) = git.try_output(&[
        "rev-parse",
        "--show-prefix",
        "--path-format=absolute",
        "--show-toplevel",
        "--git-common-dir",
    ]) else {
        return Ok(None);
    };
    let text = String::from_utf8(output)
        .map_err(|_| sync_failed("Placed work needs a working tree whose path is UTF-8."))?;
    let mut lines = text.lines();
    let (Some(prefix), Some(root), Some(common_dir)) = (lines.next(), lines.next(), lines.next())
    else {
        return Ok(None);
    };
    let prefix = match prefix.trim_end_matches('/') {
        "" => None,
        prefix => Some(WorkspacePath::parse(prefix).ok_or_else(|| {
            sync_failed("The working directory has a name this version cannot copy.")
        })?),
    };
    Ok(Some(Located {
        root: PathBuf::from(root),
        root_text: root.to_owned(),
        prefix,
        common_dir: common_dir.to_owned(),
    }))
}

/// Repository-wide state the copy reproduces besides files and the index.
#[derive(Clone, Debug)]
pub struct Repository {
    pub format: ObjectFormat,
    pub head: Head,
    /// Remote-tracking refs and tags the copy carries, the upstream's included.
    pub refs: CarriedRefs,
    /// The repository's `info/exclude`.
    pub info_exclude: Option<Vec<u8>>,
    /// The caller's global excludes file (`core.excludesFile`).
    pub global_excludes: Option<Vec<u8>>,
}

fn read_limited(path: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    (bytes.len() <= MAXIMUM_EXCLUDES).then_some(bytes)
}

/// The repository's hash, which reading its index needs.
pub fn object_format(located: &Located) -> Result<ObjectFormat> {
    match Git::caller(&located.root)
        .line(&["rev-parse", "--show-object-format"])?
        .as_str()
    {
        "sha1" => Ok(ObjectFormat::Sha1),
        "sha256" => Ok(ObjectFormat::Sha256),
        other => Err(sync_failed(format!("Unknown object format {other}."))),
    }
}

/// HEAD, its upstream, the refs a copy carries with the patterns `declared`, and
/// the ignore rules. The git calls run side by side: each costs a process, and
/// syncs of warm copies should take well under a second.
pub fn repository(
    located: &Located,
    format: ObjectFormat,
    declared: &[RefPattern],
) -> Result<Repository> {
    let git = Git::caller(&located.root);
    let (commit, branch, global_excludes, refs) = std::thread::scope(|scope| {
        let commit = scope.spawn(|| {
            git.try_output(&["rev-parse", "--verify", "-q", "HEAD^{commit}"])
                .and_then(|out| Oid::parse(String::from_utf8_lossy(&out).trim()))
        });
        let branch = scope.spawn(|| {
            let branch = git
                .try_output(&["symbolic-ref", "-q", "HEAD"])
                .map(|out| String::from_utf8_lossy(&out).trim().to_owned())
                .and_then(|reference| reference.strip_prefix("refs/heads/").map(str::to_owned))?;
            let upstream = upstream(&git, &branch);
            Some((branch, upstream))
        });
        let excludes = scope.spawn(|| global_excludes(&git));
        let refs = scope.spawn(|| CarriedRefs::list(&git, declared));
        (
            commit.join().expect("git calls do not panic"),
            branch.join().expect("git calls do not panic"),
            excludes.join().expect("git calls do not panic"),
            refs.join().expect("git calls do not panic"),
        )
    });
    let head = match (branch, commit) {
        (Some((branch, upstream)), commit) => Head::Branch {
            upstream,
            name: BranchName::parse(&branch).ok_or_else(|| {
                sync_failed("The checked-out branch has a name this version cannot copy.")
            })?,
            commit,
        },
        (None, Some(commit)) => Head::Detached { commit },
        (None, None) => return Err(sync_failed("HEAD names neither a branch nor a commit.")),
    };
    let refs = CarriedRefs::read(&refs?, head.upstream())?;
    Ok(Repository {
        format,
        head,
        refs,
        info_exclude: read_limited(&Path::new(&located.common_dir).join("info/exclude")),
        global_excludes,
    })
}

/// The caller's global excludes file: `core.excludesFile`, or git's default.
fn global_excludes(git: &Git) -> Option<Vec<u8>> {
    git.try_output(&["config", "--path", "--get", "core.excludesFile"])
        .map(|out| PathBuf::from(String::from_utf8_lossy(&out).trim()))
        .or_else(|| {
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
            Some(base.join("git/ignore"))
        })
        .and_then(|path| read_limited(&path))
}

fn upstream(git: &Git, branch: &str) -> Option<Upstream> {
    let output = git.try_output(&[
        "for-each-ref",
        "--format=%(upstream)%00%(upstream:remotename)%00%(upstream:remoteref)",
        &format!("refs/heads/{branch}"),
    ])?;
    let text = String::from_utf8(output).ok()?;
    let mut fields = text.trim_end_matches('\n').split('\0');
    let (tracking, remote, merge) = (fields.next()?, fields.next()?, fields.next()?);
    // Only branches that track a remote; a local upstream has no remote name.
    if tracking.is_empty() || remote.is_empty() || !tracking.starts_with("refs/remotes/") {
        return None;
    }
    let commit = git
        .try_output(&[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{tracking}^{{commit}}"),
        ])
        .and_then(|out| Oid::parse(String::from_utf8_lossy(&out).trim()))?;
    // An upstream whose names this version cannot reproduce is left out.
    Some(Upstream {
        remote: RemoteName::parse(remote)?,
        merge: Reference::parse(merge)?,
        tracking: Reference::parse(tracking)?,
        commit,
    })
}

/// The index and files of a working tree.
pub struct Tree {
    pub manifest: Manifest,
    pub index: IndexListing,
    /// Files read and hashed because their status changed.
    pub hashed_files: usize,
    pub hashed_bytes: u64,
}

/// Lists the caller's index and every file a copy needs, hashing only files whose
/// status changed since `cache` saw them.
pub fn tree(located: &Located, format: ObjectFormat, cache: &mut StatCache) -> Result<Tree> {
    let git = Git::caller(&located.root);
    // Listing untracked files walks the tree and takes longest; it runs beside the
    // index listing.
    let (index, untracked) = std::thread::scope(|scope| {
        let untracked =
            scope.spawn(|| git.output(&["ls-files", "-z", "--others", "--exclude-standard"]));
        let index = git.output(&["ls-files", "-s", "-z"]);
        (index, untracked.join().expect("git calls do not panic"))
    });
    let index = IndexListing::parse_git(&index?, format)
        .ok_or_else(|| sync_failed("The index lists a path or entry this version cannot copy."))?;
    let untracked = untracked?;
    let mut paths: BTreeSet<WorkspacePath> = index
        .paths()
        .iter()
        .filter(|(_, entries)| entries.iter().any(|e| e.mode != GITLINK))
        .map(|(path, _)| path.clone())
        .collect();
    for record in untracked.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        // Nested repositories are listed with a trailing slash; they are not copied.
        if record.ends_with(b"/") {
            continue;
        }
        let path = std::str::from_utf8(record)
            .ok()
            .and_then(WorkspacePath::parse);
        paths.insert(path.ok_or_else(|| {
            sync_failed("An untracked file has a name this version cannot copy.")
        })?);
    }

    let confined = Confined::open(&located.root)
        .map_err(|e| sync_failed(format!("The working tree could not be opened: {e}.")))?;
    let mut entries = BTreeMap::new();
    let mut keys = HashMap::new();
    let mut to_hash = Vec::new();
    for path in paths {
        let Some(status) = confined
            .stat(&path)
            .map_err(|e| sync_failed(format!("{path} could not be read: {e}.")))?
        else {
            continue;
        };
        match status.kind {
            Kind::File => {
                let key = StatKey::from(&status);
                match cache.get(&path, &key) {
                    Some(digest) => {
                        entries.insert(path.clone(), file_entry(&status, digest));
                        keys.insert(path, (key, digest));
                    }
                    None => to_hash.push(path),
                }
            }
            Kind::Symlink => {
                let target = confined
                    .read_link(&path)
                    .map_err(|e| sync_failed(format!("{path} could not be read: {e}.")))?
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .filter(|t| !t.is_empty() && t.len() <= 4096)
                    .ok_or_else(|| {
                        sync_failed(format!(
                            "The link {path} has a target this version cannot copy."
                        ))
                    })?;
                entries.insert(path, Entry::Symlink { target });
            }
            // A tracked path that is now a directory or something else is absent.
            Kind::Directory | Kind::Other => (),
        }
    }

    let hashed = hash_all(&confined, to_hash)?;
    let hashed_files = hashed.len();
    let mut hashed_bytes = 0;
    for (path, status, digest) in hashed {
        hashed_bytes += status.size;
        entries.insert(path.clone(), file_entry(&status, digest));
        keys.insert(path, (StatKey::from(&status), digest));
    }
    cache.replace(keys);
    Ok(Tree {
        manifest: Manifest::new(entries),
        index,
        hashed_files,
        hashed_bytes,
    })
}

fn file_entry(status: &crate::workspace::confined::Status, digest: Digest) -> Entry {
    Entry::File {
        mode: status.mode & PERMISSIONS,
        size: status.size,
        digest,
    }
}

type Hashed = (WorkspacePath, crate::workspace::confined::Status, Digest);

/// Hashes files on several threads. A file that changes while it is read is read
/// again; one that disappears is absent.
fn hash_all(confined: &Confined, paths: Vec<WorkspacePath>) -> Result<Vec<Hashed>> {
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(paths.len()));
    let failure = Mutex::new(None);
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8)
        .min(paths.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(i) else { return };
                    match hash_one(confined, path) {
                        Ok(Some((status, digest))) => {
                            results
                                .lock()
                                .expect("lock")
                                .push((path.clone(), status, digest))
                        }
                        Ok(None) => (),
                        Err(error) => {
                            *failure.lock().expect("lock") = Some(error);
                            return;
                        }
                    }
                }
            });
        }
    });
    if let Some(error) = failure.into_inner().expect("lock") {
        return Err(error);
    }
    Ok(results.into_inner().expect("lock"))
}

fn hash_one(
    confined: &Confined,
    path: &WorkspacePath,
) -> Result<Option<(crate::workspace::confined::Status, Digest)>> {
    let failed = |e: std::io::Error| sync_failed(format!("{path} could not be read: {e}."));
    for _ in 0..3 {
        let Some(file) = confined.open_file(path).map_err(failed)? else {
            return Ok(None);
        };
        let before = crate::workspace::confined::fstat(&file).map_err(failed)?;
        let (digest, size) = Digest::of_reader(&file).map_err(failed)?;
        let after = crate::workspace::confined::fstat(&file).map_err(failed)?;
        if StatKey::from(&before) == StatKey::from(&after) && size == after.size {
            return Ok(Some((after, digest)));
        }
    }
    Err(sync_failed(format!(
        "{path} kept changing while it was read; retry once it is written."
    )))
}
