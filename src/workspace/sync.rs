//! Bringing a member's copy to the caller's snapshot.
//!
//! 1. The caller sends its HEAD, the refs the copy carries and its ignore rules,
//!    then lists its tree while the member checks what runs changed in its copy.
//! 2. The member answers with digests of what it last wrote, the paths runs
//!    changed, and whether it holds the commits of HEAD and those refs.
//! 3. When it does not, the caller names ancestors of HEAD; the member fetches the
//!    best one it can from its own object sources and answers with the commits it
//!    holds, to send history against.
//! 4. The caller sends its manifest and index, as a difference from what it last
//!    pushed when the member still holds exactly that, and a pack of missing
//!    history.
//! 5. The member asks only for content it cannot take from its own copy or object
//!    store; the caller sends it; the member applies everything and confirms.
use crate::{
    failure::{Failure, Result},
    wire,
    workspace::{
        carried_refs::{CarriedRefs, RefPattern},
        confined::Confined,
        copy::{Applied, Copy, MAXIMUM_PACK},
        digest::Digest,
        git::{Git, ObjectFormat, Oid},
        head::Head,
        index::{IndexDelta, IndexListing},
        location::CopyLocation,
        manifest::{Delta, Entry, Manifest},
        object_sources::{MAXIMUM_BASES, ObjectSources},
        path::WorkspacePath,
        push_cache::{CacheFiles, Pushed},
        snapshot::{self, Located, MAXIMUM_EXCLUDES},
        stat_cache::StatCache,
        sync_failed,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Read,
    process::Stdio,
    time::Instant,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::block_in_place,
};

/// A listing larger than this is refused.
const MAXIMUM_LISTING: usize = 256 << 20;
/// Most commits a member may offer as the base of a pack.
const MAXIMUM_TIPS: usize = 256;
/// Most commits a member imports from its object sources in one sync.
const MAXIMUM_IMPORTS: usize = 4;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "sync", rename_all = "snake_case")]
pub enum SyncMessage {
    Begin {
        format: ObjectFormat,
        head: Head,
        /// The remote-tracking refs and tags the copy carries besides HEAD.
        #[serde(default)]
        refs: CarriedRefs,
        #[serde(with = "wire::bytes::optional")]
        info_exclude: Option<Vec<u8>>,
        #[serde(with = "wire::bytes::optional")]
        global_excludes: Option<Vec<u8>>,
    },
    /// With `search`, the member will look for the missing history in its object
    /// sources and answers `History` with `Tips`; without, `tips` are final.
    Copy {
        manifest: Digest,
        index: Digest,
        dirty: Vec<WorkspacePath>,
        missing_commits: bool,
        search: bool,
        tips: Vec<Oid>,
    },
    /// HEAD's first-parent ancestors, nearest first, for the member's search.
    History {
        bases: Vec<Oid>,
    },
    /// What the member holds after taking what it could from its object sources.
    Tips {
        missing_commits: bool,
        tips: Vec<Oid>,
    },
    /// Followed by the manifest blob, the index blob and, with `pack`, a pack.
    Plan {
        manifest: Digest,
        index: Digest,
        manifest_delta: bool,
        index_delta: bool,
        pack: bool,
    },
    /// Followed, from the caller, by one blob per content digest and, with
    /// objects, one pack.
    Want {
        contents: Vec<Digest>,
        objects: Vec<Oid>,
    },
    Synced {
        member: MemberStats,
    },
    Abort {
        message: String,
    },
}

#[allow(
    clippy::large_enum_variant,
    reason = "one message is received at a time; boxing would only add an allocation"
)]
pub enum Received {
    Message(SyncMessage),
    Content(Vec<u8>),
}

/// How sync traffic travels. A failing link reports why, such as a lost connection.
pub trait Link: Send {
    fn send(&mut self, message: SyncMessage) -> impl Future<Output = Result<()>> + Send;
    /// Sends one chunk of a blob; an empty chunk ends it.
    fn send_content(&mut self, chunk: Vec<u8>) -> impl Future<Output = Result<()>> + Send;
    fn receive(&mut self) -> impl Future<Output = Result<Received>> + Send;
}

/// What the member did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberStats {
    /// Paths runs had changed since the previous sync.
    pub dirty: usize,
    pub written: usize,
    pub removed: usize,
    /// Untracked, non-ignored files a run left that the caller does not have.
    pub extras_removed: usize,
    /// Content taken from the copy itself, as after a rename.
    pub reused_local: usize,
    /// Content taken from the object store.
    pub from_objects: usize,
    pub received_files: usize,
    pub received_bytes: u64,
    pub pack_bytes: u64,
    /// Whether history came from one of the member's object sources.
    pub history_from_source: bool,
    /// The refs the copy carries besides HEAD, by full name.
    #[serde(default)]
    pub refs: Vec<String>,
    pub full_manifest: bool,
    pub full_index: bool,
    /// Bytes of file content the copy now holds, without ignored files.
    pub content_bytes: u64,
    pub files: usize,
    pub phases: MemberPhases,
}

/// Where the member's part of a sync spent its time, in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberPhases {
    /// Opening the copy and finding what runs changed.
    pub verify: u64,
    /// Fetching the caller's history from the member's own object sources.
    pub sources: u64,
    /// Receiving and indexing the history the caller sent.
    pub pack: u64,
    /// Taking content from the copy and its object store.
    pub staging: u64,
    /// Receiving content from the caller.
    pub contents: u64,
    /// Writing the tree, the index and HEAD.
    pub apply: u64,
}

/// Where the caller's part of a sync spent its time, in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerPhases {
    /// Listing and hashing the working tree.
    pub listing: u64,
    /// From the member's report until the caller's history was sent, including
    /// the member's search of its object sources.
    pub history: u64,
    /// From the member's request until the content it wanted was sent.
    pub contents: u64,
}

/// Phases by name, as records keep them.
pub fn phases(caller: Option<&CallerPhases>, member: &MemberPhases) -> BTreeMap<String, u64> {
    let mut named = BTreeMap::new();
    if let Some(caller) = caller {
        for (name, ms) in [
            ("listing", caller.listing),
            ("history", caller.history),
            ("contents", caller.contents),
        ] {
            named.insert(format!("caller.{name}"), ms);
        }
    }
    for (name, ms) in [
        ("verify", member.verify),
        ("sources", member.sources),
        ("pack", member.pack),
        ("staging", member.staging),
        ("contents", member.contents),
        ("apply", member.apply),
    ] {
        named.insert(format!("member.{name}"), ms);
    }
    named
}

fn since(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// What one sync cost, as the caller measured it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncStats {
    pub duration_ms: u64,
    /// Every byte of listings, content and packs the caller sent.
    pub sent_bytes: u64,
    pub hashed_files: usize,
    pub hashed_bytes: u64,
    pub phases: CallerPhases,
    /// The refs the copy carries besides HEAD, by full name.
    pub refs: Vec<String>,
    /// Commits sent only because a carried ref needs them: history that neither
    /// HEAD nor its upstream reaches. `None` when no history was sent.
    pub refs_only_commits: Option<u64>,
    pub member: MemberStats,
}

pub(crate) fn unexpected() -> Failure {
    sync_failed("The other side of the sync sent something unexpected.")
}

async fn expect_message(link: &mut impl Link) -> Result<SyncMessage> {
    match link.receive().await? {
        Received::Message(SyncMessage::Abort { message }) => Err(sync_failed(message)),
        Received::Message(message) => Ok(message),
        Received::Content(_) => Err(unexpected()),
    }
}

async fn send_blob(link: &mut impl Link, bytes: &[u8]) -> Result<u64> {
    for chunk in bytes.chunks(wire::CHUNK) {
        link.send_content(chunk.to_vec()).await?;
    }
    link.send_content(Vec::new()).await?;
    Ok(bytes.len() as u64)
}

async fn receive_blob(link: &mut impl Link, limit: usize) -> Result<Vec<u8>> {
    let mut blob = Vec::new();
    loop {
        match link.receive().await? {
            Received::Content(chunk) if chunk.is_empty() => return Ok(blob),
            Received::Content(chunk) => {
                if blob.len() + chunk.len() > limit {
                    return Err(sync_failed("A sync listing was too large."));
                }
                blob.extend_from_slice(&chunk);
            }
            Received::Message(SyncMessage::Abort { message }) => return Err(sync_failed(message)),
            Received::Message(_) => return Err(unexpected()),
        }
    }
}

/// Streams the output of a git command as one blob.
async fn send_command_output(
    link: &mut impl Link,
    mut command: tokio::process::Command,
    input: String,
) -> Result<u64> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::child_start::spawn_async(&mut command)
        .map_err(|e| sync_failed(format!("git could not be started: {e}.")))?;
    let mut stdin = child.stdin.take().expect("piped");
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(input.as_bytes()).await;
    });
    let mut stdout = child.stdout.take().expect("piped");
    let mut total = 0u64;
    let mut buffer = vec![0u8; wire::CHUNK];
    loop {
        let n = stdout
            .read(&mut buffer)
            .await
            .map_err(|e| sync_failed(format!("git pack-objects failed: {e}.")))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        link.send_content(buffer[..n].to_vec()).await?;
    }
    let _ = writer.await;
    let status = child
        .wait()
        .await
        .map_err(|e| sync_failed(format!("git pack-objects failed: {e}.")))?;
    if !status.success() {
        link.send(SyncMessage::Abort {
            message: "The caller could not pack its history.".into(),
        })
        .await?;
        return Err(sync_failed("git pack-objects failed on the caller."));
    }
    link.send_content(Vec::new()).await?;
    Ok(total)
}

// ----- Caller --------------------------------------------------------------------

/// Brings `member`'s copy to the caller's working tree, with the refs the
/// consumer's `declared` patterns match, and returns what it cost.
pub async fn push(
    link: &mut impl Link,
    located: &Located,
    caches: &CacheFiles,
    member: &str,
    declared: &[RefPattern],
) -> Result<SyncStats> {
    let started = Instant::now();
    let format = {
        let located = located.clone();
        tokio::task::spawn_blocking(move || snapshot::object_format(&located))
            .await
            .expect("snapshots do not panic")?
    };
    // The tree is listed while HEAD is read, Begin is sent and the member checks
    // its copy.
    let tree = {
        let located = located.clone();
        let caches = caches.clone();
        tokio::task::spawn_blocking(move || {
            let listing = Instant::now();
            // Recorded before any state, so the state can be cleaned up later.
            caches.register();
            let mut cache = StatCache::load(&caches.stat());
            let tree = snapshot::tree(&located, format, &mut cache);
            if tree.is_ok() {
                cache.save(&caches.stat());
            }
            tree.map(|tree| (tree, since(listing)))
        })
    };
    let mut phases = CallerPhases::default();
    let repository = {
        let located = located.clone();
        let declared = declared.to_vec();
        tokio::task::spawn_blocking(move || snapshot::repository(&located, format, &declared))
            .await
            .expect("snapshots do not panic")?
    };
    link.send(SyncMessage::Begin {
        format,
        head: repository.head.clone(),
        refs: repository.refs.clone(),
        info_exclude: repository.info_exclude.clone(),
        global_excludes: repository.global_excludes.clone(),
    })
    .await?;
    let report = expect_message(link).await?;
    let reported = Instant::now();
    let SyncMessage::Copy {
        manifest: copy_manifest,
        index: copy_index,
        dirty,
        missing_commits,
        search,
        tips,
    } = report
    else {
        return Err(unexpected());
    };
    let head = &repository.head;
    let git = Git::caller(&located.root);
    let (missing_commits, tips) = if search {
        let bases = {
            let git = git.clone();
            let head = head.commit().cloned();
            tokio::task::spawn_blocking(move || bases(&git, head.as_ref()))
                .await
                .expect("git does not panic")
        };
        link.send(SyncMessage::History { bases }).await?;
        let SyncMessage::Tips {
            missing_commits,
            tips,
        } = expect_message(link).await?
        else {
            return Err(unexpected());
        };
        (missing_commits, tips)
    } else {
        (missing_commits, tips)
    };
    // The listing went on while the member looked for history.
    let (tree, listing) = tree.await.expect("snapshots do not panic")?;
    phases.listing = listing;
    let pushed_path = caches.pushed(member);
    let pushed = Pushed::load(&pushed_path, format);
    let manifest_blob = match &pushed {
        Some(pushed) if pushed.manifest.digest() == copy_manifest => {
            let mut base = pushed.manifest.clone();
            for path in &dirty {
                base.remove(path);
            }
            Some(base.delta_to(&tree.manifest).encode())
        }
        _ => None,
    };
    let index_blob = match &pushed {
        Some(pushed) if pushed.index.digest() == copy_index => {
            Some(pushed.index.delta_to(&tree.index).encode())
        }
        _ => None,
    };
    // HEAD's and its upstream's commits, then those only carried refs name.
    let mut revisions = head.commits();
    let (defaults, others) = repository.refs.objects();
    let carried: Vec<Oid> = defaults
        .into_iter()
        .chain(others)
        .filter(|oid| !revisions.contains(oid))
        .collect();
    revisions.extend(carried.iter().cloned());
    let pack = missing_commits && !revisions.is_empty();
    link.send(SyncMessage::Plan {
        manifest: tree.manifest.digest(),
        index: tree.index.digest(),
        manifest_delta: manifest_blob.is_some(),
        index_delta: index_blob.is_some(),
        pack,
    })
    .await?;
    let mut sent = 0;
    let mut refs_only_commits = None;
    sent += send_blob(
        link,
        &manifest_blob.unwrap_or_else(|| tree.manifest.encode()),
    )
    .await?;
    sent += send_blob(link, &index_blob.unwrap_or_else(|| tree.index.encode())).await?;
    if pack {
        let tips: Vec<Oid> = tips
            .into_iter()
            .filter(|t| t.fits(format))
            .take(MAXIMUM_TIPS)
            .collect();
        let held = {
            let git = git.clone();
            let tips = tips.clone();
            tokio::task::spawn_blocking(move || git.existing(&tips))
                .await
                .expect("git does not panic")?
        };
        let held: Vec<Oid> = tips
            .into_iter()
            .zip(held)
            .filter(|(_, held)| *held)
            .map(|(tip, _)| tip)
            .collect();
        // What carried refs cost: the commits they alone add, counted while the
        // pack is written.
        let counting = (!carried.is_empty()).then(|| {
            let git = git.clone();
            let mut input: String = carried.iter().map(|oid| format!("{oid}\n")).collect();
            for excluded in head.commits().iter().chain(&held) {
                input.push_str(&format!("^{excluded}\n"));
            }
            tokio::task::spawn_blocking(move || {
                let output = git
                    .output_with(&["rev-list", "--count", "--stdin"], Some(input.as_bytes()))
                    .ok()?;
                String::from_utf8_lossy(&output).trim().parse::<u64>().ok()
            })
        });
        let mut input = String::new();
        for revision in &revisions {
            input.push_str(&format!("{revision}\n"));
        }
        for tip in &held {
            input.push_str(&format!("^{tip}\n"));
        }
        let command = git.streaming([
            "pack-objects",
            "--stdout",
            "--revs",
            "--thin",
            "--delta-base-offset",
            "-q",
        ]);
        sent += send_command_output(link, command, input).await?;
        refs_only_commits = match counting {
            Some(counting) => counting.await.expect("git does not panic"),
            None => Some(0),
        };
    }
    phases.history = since(reported);

    let SyncMessage::Want { contents, objects } = expect_message(link).await? else {
        return Err(unexpected());
    };
    let wanted = Instant::now();
    let by_digest: HashMap<Digest, &WorkspacePath> = tree
        .manifest
        .entries()
        .iter()
        .filter_map(|(path, entry)| match entry {
            Entry::File { digest, .. } => Some((*digest, path)),
            Entry::Symlink { .. } => None,
        })
        .collect();
    let confined = Confined::open(&located.root)
        .map_err(|e| sync_failed(format!("The working tree could not be opened: {e}.")))?;
    for digest in &contents {
        let path = by_digest
            .get(digest)
            .ok_or_else(|| sync_failed("The member asked for content the caller does not have."))?;
        sent += send_file(link, &confined, path, *digest).await?;
    }
    if !objects.is_empty() {
        // The member may ask only for objects the caller's index names.
        let staged: HashSet<&Oid> = tree
            .index
            .paths()
            .values()
            .flatten()
            .map(|entry| &entry.oid)
            .collect();
        if objects.iter().any(|o| !staged.contains(o)) {
            return Err(sync_failed(
                "The member asked for objects outside the caller's index.",
            ));
        }
        let input: String = objects.iter().map(|o| format!("{o}\n")).collect();
        let command = git.streaming(["pack-objects", "--stdout", "-q"]);
        sent += send_command_output(link, command, input).await?;
    }
    phases.contents = since(wanted);
    let SyncMessage::Synced { member } = expect_message(link).await? else {
        return Err(unexpected());
    };
    Pushed::save(&pushed_path, &tree.manifest, &tree.index);
    Ok(SyncStats {
        duration_ms: started.elapsed().as_millis() as u64,
        sent_bytes: sent,
        hashed_files: tree.hashed_files,
        hashed_bytes: tree.hashed_bytes,
        phases,
        refs: repository.refs.names(),
        refs_only_commits,
        member,
    })
}

/// HEAD's first-parent ancestors at distances 1, 2, 4 and so on, nearest first:
/// candidates a member's object source may hold when HEAD itself is local. Without
/// any, history simply comes from the caller.
fn bases(git: &Git, head: Option<&Oid>) -> Vec<Oid> {
    let Some(head) = head else {
        return Vec::new();
    };
    // 14 candidates reach 8,192 commits back.
    const FARTHEST: usize = 1 << 13;
    let Some(output) = git.try_output(&[
        "rev-list",
        "--first-parent",
        &format!("--max-count={}", FARTHEST + 1),
        head.as_str(),
    ]) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output);
    at_distances(&text.lines().collect::<Vec<_>>())
}

/// The commits of `ancestry`, HEAD first, at distances 1, 2, 4 and so on.
fn at_distances(ancestry: &[&str]) -> Vec<Oid> {
    (0..)
        .map(|power| 1usize << power)
        .take_while(|distance| *distance < ancestry.len())
        .filter_map(|distance| Oid::parse(ancestry[distance]))
        .collect()
}

/// Sends a file's content as one blob, checking it still has the listed digest.
async fn send_file(
    link: &mut impl Link,
    confined: &Confined,
    path: &WorkspacePath,
    digest: Digest,
) -> Result<u64> {
    let failed = |e: std::io::Error| sync_failed(format!("{path} could not be read: {e}."));
    let mut file = confined
        .open_file(path)
        .map_err(failed)?
        .ok_or_else(|| sync_failed(format!("{path} disappeared during the sync; retry.")))?;
    let mut hasher = crate::workspace::digest::Hasher::default();
    let mut total = 0u64;
    loop {
        let (chunk, read) = tokio::task::block_in_place(|| {
            let mut buffer = vec![0u8; wire::CHUNK];
            let n = file.read(&mut buffer)?;
            buffer.truncate(n);
            Ok::<_, std::io::Error>((buffer, n))
        })
        .map_err(failed)?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk);
        total += read as u64;
        link.send_content(chunk).await?;
    }
    link.send_content(Vec::new()).await?;
    if hasher.finish() != digest {
        let message = format!("{path} changed during the sync; retry once it is written.");
        link.send(SyncMessage::Abort {
            message: message.clone(),
        })
        .await?;
        return Err(sync_failed(message));
    }
    Ok(total)
}

// ----- Member --------------------------------------------------------------------

/// Receives a sync into the copy at `location`, taking history from `sources` where
/// it can, and returns the copy, ready to run in.
pub async fn receive(
    link: &mut impl Link,
    location: &CopyLocation,
    sources: &ObjectSources,
) -> Result<(Copy, MemberStats)> {
    let SyncMessage::Begin {
        format,
        head,
        refs,
        info_exclude,
        global_excludes,
    } = expect_message(link).await?
    else {
        return Err(unexpected());
    };
    let excludes_fit = [&info_exclude, &global_excludes]
        .iter()
        .all(|e| e.as_ref().is_none_or(|e| e.len() <= MAXIMUM_EXCLUDES));
    if !head.valid(format)
        || !refs.valid(format)
        || !refs.agrees_with(head.upstream())
        || !excludes_fit
    {
        return Err(sync_failed("The caller's repository state is invalid."));
    }
    let verifying = Instant::now();
    // HEAD's and its upstream's commits, then the objects of carried refs.
    let base_commits = head.commits().len();
    let mut commits = head.commits();
    let (defaults, others) = refs.objects();
    for object in defaults {
        if !commits.contains(&object) {
            commits.push(object);
        }
    }
    // Commits before this are HEAD's, its upstream's and remotes' default
    // branches'; after it, those only declared refs name.
    let declared_start = commits.len();
    for object in others {
        if !commits.contains(&object) {
            commits.push(object);
        }
    }
    let (mut copy, verified, held) = block_in_place(|| -> Result<_> {
        let mut copy = Copy::open(location, format)?;
        let verified = copy.verify()?;
        let held = copy.has(&commits)?;
        Ok((copy, verified, held))
    })?;
    let missing_commits = held.iter().any(|held| !held);
    // Sources are searched only when the store lacks a commit the caller took
    // from a remote: the base of its work (the upstream, or HEAD without one), or
    // a carried ref's. Commits on top of a base the store holds come from the
    // caller, as small as they are.
    let base_missing = base_commits > 0 && !held[base_commits - 1];
    let carried_missing = held[base_commits..].iter().any(|held| !held);
    let search = !sources.is_empty() && (base_missing || carried_missing);
    let tips = if search {
        Vec::new()
    } else {
        block_in_place(|| copy.tips())?
    };
    let mut stats = MemberStats {
        dirty: verified.dirty.len(),
        ..Default::default()
    };
    stats.phases.verify = since(verifying);
    link.send(SyncMessage::Copy {
        manifest: verified.manifest,
        index: verified.index,
        dirty: verified.dirty.clone(),
        missing_commits,
        search,
        tips,
    })
    .await?;
    if search {
        let SyncMessage::History { bases } = expect_message(link).await? else {
            return Err(unexpected());
        };
        if bases.len() > MAXIMUM_BASES || !bases.iter().all(|b| b.fits(format)) {
            return Err(sync_failed("The caller's history is described invalidly."));
        }
        let fetching = Instant::now();
        // Missing commits of HEAD, its upstream and remotes' default branches,
        // then ancestors up to the first the store holds (its history is held
        // too, so nothing beyond can help), then declared refs' missing commits.
        let bases_held = block_in_place(|| copy.has(&bases))?;
        let missing = |range: std::ops::Range<usize>| {
            commits[range.clone()]
                .iter()
                .zip(&held[range])
                .filter(|(_, held)| !**held)
                .map(|(commit, _)| commit.clone())
                .collect::<Vec<_>>()
        };
        // Declared refs come last: they may lie far from HEAD's history.
        let candidates: Vec<Oid> = missing(0..declared_start)
            .into_iter()
            .chain(
                bases
                    .into_iter()
                    .zip(bases_held)
                    .take_while(|(_, held)| !held)
                    .map(|(base, _)| base),
            )
            .chain(missing(declared_start..commits.len()))
            .collect();
        // A source may hold more than one of the tips, as a member's clone holds
        // both a feature branch's upstream and the default branch: after each
        // import, the tips still missing are asked for again.
        let mut supplied = Vec::new();
        let mut candidates = candidates;
        while supplied.len() < MAXIMUM_IMPORTS {
            let Some(commit) = copy.supply(sources, &candidates, supplied.len()).await else {
                break;
            };
            supplied.push(commit);
            let tips = &commits[..declared_start];
            let held = block_in_place(|| copy.has(tips))?;
            candidates = tips
                .iter()
                .zip(held)
                .filter(|(_, held)| !held)
                .map(|(tip, _)| tip.clone())
                .collect();
            if candidates.is_empty() {
                break;
            }
        }
        stats.history_from_source = !supplied.is_empty();
        let (missing_commits, mut tips) = block_in_place(|| -> Result<_> {
            Ok((copy.has(&commits)?.iter().any(|held| !held), copy.tips()?))
        })?;
        // The caller sends history against the commits just supplied above all.
        tips.retain(|tip| !supplied.contains(tip));
        tips.splice(0..0, supplied);
        stats.phases.sources = since(fetching);
        link.send(SyncMessage::Tips {
            missing_commits,
            tips,
        })
        .await?;
    }

    let SyncMessage::Plan {
        manifest: manifest_digest,
        index: index_digest,
        manifest_delta,
        index_delta,
        pack,
    } = expect_message(link).await?
    else {
        return Err(unexpected());
    };
    stats.full_manifest = !manifest_delta;
    stats.full_index = !index_delta;
    let blob = receive_blob(link, MAXIMUM_LISTING).await?;
    let base = copy.manifest().clone();
    let target = if manifest_delta {
        let delta =
            Delta::decode(&blob).ok_or_else(|| sync_failed("The caller's manifest is invalid."))?;
        let mut target = base.clone();
        target.apply(&delta);
        target
    } else {
        Manifest::decode(&blob).ok_or_else(|| sync_failed("The caller's manifest is invalid."))?
    };
    if target.digest() != manifest_digest || !target.is_tree() {
        return Err(sync_failed(
            "The caller's manifest does not match its digest.",
        ));
    }
    let blob = receive_blob(link, MAXIMUM_LISTING).await?;
    let index = if index_delta {
        let delta = IndexDelta::decode(&blob, format)
            .ok_or_else(|| sync_failed("The caller's index is invalid."))?;
        let mut index = copy.index().clone();
        index.apply(&delta);
        index
    } else {
        IndexListing::decode(&blob, format)
            .ok_or_else(|| sync_failed("The caller's index is invalid."))?
    };
    if index.digest() != index_digest {
        return Err(sync_failed("The caller's index does not match its digest."));
    }
    if pack {
        let importing = Instant::now();
        stats.pack_bytes = import_pack(link, &copy, true).await?;
        if block_in_place(|| copy.has(&commits))?
            .iter()
            .any(|held| !held)
        {
            return Err(sync_failed("The caller's history did not arrive complete."));
        }
        stats.phases.pack = since(importing);
    }
    let staging_started = Instant::now();

    // Content the copy needs, from its own files first, then its object store,
    // and only then from the caller.
    let mut staging = block_in_place(|| copy.staging())?;
    let mut needed: Vec<(WorkspacePath, Digest)> = Vec::new();
    let mut seen = HashSet::new();
    for (path, entry) in target.entries() {
        if let Entry::File { digest, .. } = entry {
            let held = matches!(base.get(path), Some(Entry::File { digest: d, .. }) if d == digest);
            if !held && seen.insert(*digest) {
                needed.push((path.clone(), *digest));
            }
        }
    }
    let local: HashMap<Digest, &WorkspacePath> = base
        .entries()
        .iter()
        .filter_map(|(path, entry)| match entry {
            Entry::File { digest, .. } => Some((*digest, path)),
            Entry::Symlink { .. } => None,
        })
        .collect();
    let mut from_objects = Vec::new();
    block_in_place(|| -> Result<()> {
        for (path, digest) in &needed {
            if let Some(source) = local.get(digest)
                && copy.stage_local(&mut staging, source, *digest)?
            {
                stats.reused_local += 1;
            } else if let Some(staged) = index.resolved(path) {
                from_objects.push((staged.oid.clone(), *digest));
            }
        }
        stats.from_objects = copy.stage_objects(&mut staging, &from_objects)?.len();
        Ok(())
    })?;
    let contents: Vec<Digest> = needed
        .iter()
        .map(|(_, digest)| *digest)
        .filter(|digest| !staging.has(digest))
        .collect();
    let previous = copy.index().clone();
    let new_objects: Vec<Oid> = {
        let delta = previous.delta_to(&index);
        let mut unique: Vec<Oid> = delta.objects().cloned().collect();
        unique.sort();
        unique.dedup();
        unique
    };
    let held = block_in_place(|| copy.has(&new_objects))?;
    let objects: Vec<Oid> = new_objects
        .into_iter()
        .zip(held)
        .filter(|(_, held)| !held)
        .map(|(oid, _)| oid)
        .collect();
    stats.phases.staging = since(staging_started);
    link.send(SyncMessage::Want {
        contents: contents.clone(),
        objects: objects.clone(),
    })
    .await?;
    let receiving = Instant::now();
    let sizes: HashMap<Digest, u64> = target
        .entries()
        .values()
        .filter_map(|entry| match entry {
            Entry::File { digest, size, .. } => Some((*digest, *size)),
            Entry::Symlink { .. } => None,
        })
        .collect();
    for digest in &contents {
        let mut incoming = block_in_place(|| staging.begin(sizes[digest]))?;
        loop {
            match link.receive().await? {
                Received::Content(chunk) if chunk.is_empty() => break,
                Received::Content(chunk) => block_in_place(|| incoming.write(&chunk))?,
                Received::Message(SyncMessage::Abort { message }) => {
                    return Err(sync_failed(message));
                }
                Received::Message(_) => return Err(unexpected()),
            }
        }
        stats.received_bytes += incoming.size;
        if !block_in_place(|| staging.finish(incoming, *digest))? {
            // The caller follows a mismatch with its reason.
            return match link.receive().await? {
                Received::Message(SyncMessage::Abort { message }) => Err(sync_failed(message)),
                _ => Err(sync_failed("Content arrived with the wrong digest.")),
            };
        }
        stats.received_files += 1;
    }
    if !objects.is_empty() {
        stats.pack_bytes += import_pack(link, &copy, false).await?;
        if block_in_place(|| copy.has(&objects))?
            .iter()
            .any(|held| !held)
        {
            return Err(sync_failed("Staged objects did not arrive complete."));
        }
    }
    stats.phases.contents = since(receiving);
    let applying = Instant::now();
    let applied: Applied = block_in_place(|| {
        copy.apply(
            &target,
            &verified.dirty,
            &staging,
            (&info_exclude, &global_excludes),
            &index,
            &head,
            &refs,
        )
    })?;
    stats.written = applied.written;
    stats.removed = applied.removed;
    stats.extras_removed = applied.extras_removed;
    stats.content_bytes = target.content_bytes();
    stats.files = target.len();
    stats.refs = refs.names();
    stats.phases.apply = since(applying);
    link.send(SyncMessage::Synced {
        member: stats.clone(),
    })
    .await?;
    Ok((copy, stats))
}

/// Reads one pack blob into the copy's object store.
async fn import_pack(link: &mut impl Link, copy: &Copy, thin: bool) -> Result<u64> {
    let mut child = crate::child_start::spawn_async(&mut copy.pack_importer(thin))
        .map_err(|e| sync_failed(format!("git index-pack could not be started: {e}.")))?;
    let mut stdin = child.stdin.take().expect("piped");
    let mut total = 0u64;
    let mut failure = None;
    loop {
        match link.receive().await {
            Ok(Received::Content(chunk)) if chunk.is_empty() => break,
            Ok(Received::Content(chunk)) => {
                total += chunk.len() as u64;
                if total > MAXIMUM_PACK {
                    return Err(sync_failed(
                        "The caller's history is larger than a member accepts.",
                    ));
                }
                if failure.is_none() && stdin.write_all(&chunk).await.is_err() {
                    failure = Some(sync_failed(
                        "The member's git refused the caller's history.",
                    ));
                }
            }
            Ok(Received::Message(SyncMessage::Abort { message })) => {
                return Err(sync_failed(message));
            }
            Ok(Received::Message(_)) => return Err(unexpected()),
            Err(error) => return Err(error),
        }
    }
    drop(stdin);
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| sync_failed(format!("git index-pack failed: {e}.")))?;
    if let Some(failure) = failure {
        return Err(failure);
    }
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(sync_failed(format!(
            "The member's git refused the caller's history: {}",
            detail.lines().next().unwrap_or("").trim()
        )));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bases_lie_at_doubling_distances() {
        let ancestry: Vec<String> = (0..20).map(|i| format!("{i:040x}")).collect();
        let ancestry: Vec<&str> = ancestry.iter().map(String::as_str).collect();
        let distances = |n: usize| -> Vec<String> {
            at_distances(&ancestry[..n])
                .iter()
                .map(|oid| oid.as_str().trim_start_matches('0').to_owned())
                .collect()
        };
        assert_eq!(distances(20), ["1", "2", "4", "8", "10"]);
        assert_eq!(distances(2), ["1"]);
        assert!(distances(1).is_empty(), "a root commit has no ancestors");
        let full: Vec<String> = (0..=1usize << 13).map(|i| format!("{i:040x}")).collect();
        let full: Vec<&str> = full.iter().map(String::as_str).collect();
        let bases = at_distances(&full);
        assert_eq!(bases.len(), 14);
        assert!(bases.len() <= MAXIMUM_BASES);
    }
}
