//! A member's persistent copy of one caller workspace.
//!
//! The copy is a git working tree whose history lives in the caller repository's
//! shared object store. Its record says what Elsecall last wrote. A run may change
//! the copy; the next sync finds what it changed by file status alone and puts back
//! the caller's state, never touching files git ignores.
use crate::{
    failure::Result,
    workspace::{
        carried_refs::CarriedRefs,
        confined::{Confined, Create, Kind, stat_at},
        copy_record::{CopyRecord, Written},
        digest::Digest,
        git::{Git, ObjectFormat, Oid},
        head::Head,
        index::IndexListing,
        location::{CopyLocation, Setting},
        manifest::{Entry, Manifest},
        object_sources::{Destination, ObjectSources},
        output_staging::OutputStaging,
        path::WorkspacePath,
        staging::Staging,
        sync_failed,
    },
};
use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    },
    path::Path,
    process::Stdio,
};

/// Inside the copy's git directory: whose copy this is, written before anything else.
const MARKER: &str = "elsecall-copy";
/// Written once the copy's repository is fully set up.
const READY: &str = "elsecall-ready";
const STAGING: &str = "elsecall-staging";
/// Each run's snapshot of its outputs at exit, while they are returned.
const OUTPUT_STAGING: &str = "elsecall-outputs";
const EXCLUDES: &str = "elsecall-excludes";
/// The largest pack the member accepts in one sync.
pub const MAXIMUM_PACK: u64 = 16 << 30;

pub struct Copy {
    location: CopyLocation,
    format: ObjectFormat,
    confined: Confined,
    git: Git,
    store: Git,
    record: CopyRecord,
    /// Untracked files git does not ignore, listed once `verify` restored the index,
    /// while the caller prepares its changes.
    untracked: Option<std::thread::JoinHandle<Result<Vec<u8>>>>,
    /// The copy's remote-tracking refs and tags, listed once `verify` ran.
    refs: Option<std::thread::JoinHandle<Result<Vec<u8>>>>,
}

/// What a sync found the copy to hold before changing it.
pub struct Verified {
    /// The digest of the manifest last written, which the caller may still hold.
    pub manifest: Digest,
    pub index: Digest,
    /// Paths a run changed, created or removed since; their content is unknown.
    pub dirty: Vec<WorkspacePath>,
}

/// What applying a sync changed.
#[derive(Clone, Copy, Debug, Default)]
pub struct Applied {
    pub written: usize,
    pub removed: usize,
    pub extras_removed: usize,
}

fn failed(action: &str) -> impl Fn(io::Error) -> crate::failure::Failure + '_ {
    move |e| sync_failed(format!("{action} on the member failed: {e}."))
}

fn private_directory(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(failed("Creating a workspace directory"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(failed("Restricting a workspace directory"))
}

fn path(text: &str) -> WorkspacePath {
    WorkspacePath::parse(text).expect("a valid constant path")
}

impl Copy {
    /// Opens the copy at `location`, creating it and its object store if needed.
    /// A same-path location that holds anything but this calling member's copy of
    /// this workspace is refused, untouched.
    pub fn open(location: &CopyLocation, format: ObjectFormat) -> Result<Self> {
        let root = &location.root;
        let marker = format!("{}\n{}\n", location.calling_member, location.caller_root);
        if let Some(parent) = location.store.parent() {
            private_directory(parent)?;
        }
        match location.setting {
            Setting::Mapped => {
                private_directory(root.parent().expect("mapped copies have parents"))?
            }
            Setting::SamePath => fs::create_dir_all(root.parent().expect("absolute"))
                .map_err(failed("Creating the copy's parent"))?,
        }
        let existing = match fs::symlink_metadata(root) {
            Ok(meta) if meta.is_dir() => true,
            Ok(_) => {
                return Err(sync_failed(format!(
                    "{} exists on the member and is not a directory; Elsecall never overwrites it.",
                    root.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(failed("Inspecting the copy")(e)),
        };
        if existing {
            let confined = Confined::open(root).map_err(failed("Opening the copy"))?;
            let recorded = read_in_git(&confined, MARKER);
            match (recorded, location.setting) {
                (Some(recorded), _) if recorded == marker.as_bytes() => (),
                (Some(_), _) => {
                    return Err(sync_failed(format!(
                        "{} is another calling member's copy of a workspace at this path; this member holds one copy per path, so place the run elsewhere or switch it to mapped locations.",
                        root.display()
                    )));
                }
                // Mapped locations are Elsecall's own: an unmarked one is a copy
                // whose first setup was interrupted.
                (None, Setting::Mapped) => {
                    fs::remove_dir_all(root).map_err(failed("Clearing an unfinished copy"))?;
                }
                (None, Setting::SamePath) => {
                    return Err(sync_failed(format!(
                        "{} exists on the member and is not an Elsecall copy of this workspace; Elsecall never overwrites it. Remove it there to let Elsecall copy the workspace to it.",
                        root.display()
                    )));
                }
            }
        }
        if !root.exists() {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(root)
                .map_err(failed("Creating the copy"))?;
        }
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))
            .map_err(failed("Restricting the copy"))?;
        let confined = Confined::open(root).map_err(failed("Opening the copy"))?;
        if read_in_git(&confined, MARKER).is_none() {
            // Mark the directory before anything else, so an interrupted setup is
            // still recognized as this copy.
            fs::DirBuilder::new()
                .mode(0o700)
                .create(root.join(".git"))
                .map_err(failed("Creating the copy's repository"))?;
            write_in_git(&confined, MARKER, marker.as_bytes())?;
        }

        let store = Git::member(location.store.parent().expect("stores have parents"));
        if !location.store.join("objects").is_dir() {
            store.output(&[
                "init".as_ref(),
                "--bare".as_ref(),
                "-q".as_ref(),
                format!("--object-format={}", format.name()).as_ref(),
                location.store.as_os_str(),
            ])?;
            Git::member(&location.store).output(&["config", "gc.auto", "0"])?;
        }
        let store = Git::member(&location.store);
        if store.line(&["rev-parse", "--show-object-format"])? != format.name() {
            return Err(sync_failed(
                "The member's object store for this repository uses another hash.",
            ));
        }
        let git = Git::member(root);
        if read_in_git(&confined, READY).is_none() {
            git.output(&[
                "init".to_owned(),
                "-q".to_owned(),
                format!("--object-format={}", format.name()),
            ])?;
            let excludes = root.join(".git").join(EXCLUDES);
            for (key, value) in [
                ("gc.auto", "0"),
                ("core.untrackedCache", "true"),
                ("advice.detachedHead", "false"),
                ("core.excludesFile", &excludes.to_string_lossy()),
            ] {
                git.output(&["config", key, value])?;
            }
            write_in_git(
                &confined,
                "objects/info/alternates",
                format!("{}\n", location.store.join("objects").display()).as_bytes(),
            )?;
            write_in_git(&confined, READY, b"")?;
        }
        let meta = fs::symlink_metadata(root).map_err(failed("Inspecting the copy"))?;
        let identity = (meta.dev(), meta.ino());
        let record = CopyRecord::load(&location.state_file, format)
            .filter(|record| record.identity == identity)
            .unwrap_or(CopyRecord {
                identity,
                ..Default::default()
            });
        Ok(Self {
            confined,
            location: location.clone(),
            format,
            git,
            store,
            record,
            untracked: None,
            refs: None,
        })
    }

    pub fn root(&self) -> &Path {
        &self.location.root
    }

    pub fn location(&self) -> &CopyLocation {
        &self.location
    }

    pub fn manifest(&self) -> &Manifest {
        &self.record.manifest
    }

    pub fn index(&self) -> &IndexListing {
        &self.record.index
    }

    /// Finds what runs changed since the last sync, forgets it, and puts the index
    /// back as it was written.
    pub fn verify(&mut self) -> Result<Verified> {
        let manifest = self.record.manifest.digest();
        let mut dirty = Vec::new();
        for (path, entry) in self.record.manifest.entries() {
            let status = self
                .confined
                .stat(path)
                .map_err(failed("Inspecting the copy"))?;
            let intact = match (entry, status) {
                (Entry::File { .. }, Some(status)) if status.kind == Kind::File => {
                    self.record.written.get(path) == Some(&Written::from(&status))
                }
                (Entry::Symlink { target }, Some(status)) if status.kind == Kind::Symlink => self
                    .confined
                    .read_link(path)
                    .map_err(failed("Inspecting the copy"))?
                    .is_some_and(|read| read == target.as_bytes()),
                _ => false,
            };
            if !intact {
                dirty.push(path.clone());
            }
        }
        for path in &dirty {
            self.record.manifest.remove(path);
            self.record.written.remove(path);
        }
        if self.index_file() != self.record.index_file {
            let index = self.record.index.clone();
            self.set_index(&index)?;
        }
        let git = self.git.clone();
        self.untracked = Some(std::thread::spawn(move || {
            git.output(&["ls-files", "-z", "--others", "--exclude-standard"])
        }));
        let git = self.git.clone();
        self.refs = Some(std::thread::spawn(move || CarriedRefs::list_copy(&git)));
        Ok(Verified {
            manifest,
            index: self.record.index.digest(),
            dirty,
        })
    }

    /// The index as git has it now; `None` if a run staged something this version
    /// cannot represent.
    fn current_index(&self) -> Result<Option<IndexListing>> {
        let output = self.git.output(&["ls-files", "-s", "-z"])?;
        Ok(IndexListing::parse_git(&output, self.format))
    }

    fn index_file(&self) -> Option<Written> {
        let status = stat_at(self.confined.root().as_raw_fd(), c".git/index")
            .ok()
            .flatten()?;
        Some(Written::from(&status))
    }

    /// Makes git's index exactly `target`, touching only entries that differ.
    pub fn set_index(&mut self, target: &IndexListing) -> Result<()> {
        if *target == self.record.index
            && self.record.index_file.is_some()
            && self.index_file() == self.record.index_file
        {
            return Ok(());
        }
        let current = match self.current_index()? {
            Some(current) => current,
            None => {
                self.git.output(&["read-tree", "--empty"])?;
                IndexListing::default()
            }
        };
        let delta = current.delta_to(target);
        if !delta.is_empty() {
            self.git.output_with(
                &["update-index", "-z", "--index-info"],
                Some(&delta.index_info(self.format)),
            )?;
            if self.current_index()?.as_ref().map(IndexListing::digest) != Some(target.digest()) {
                return Err(sync_failed(
                    "The copy's index could not be set to the caller's.",
                ));
            }
        }
        self.record.index = target.clone();
        self.record.index_file = self.index_file();
        Ok(())
    }

    /// Which of `oids` the copy can read.
    pub fn has(&self, oids: &[Oid]) -> Result<Vec<bool>> {
        self.git.existing(oids)
    }

    /// Commits this member holds for the repository, newest first, for the caller
    /// to send only what is missing.
    pub fn tips(&self) -> Result<Vec<Oid>> {
        let output = self.store.output(&[
            "for-each-ref",
            "--sort=-committerdate",
            "--count=64",
            "--format=%(objectname)",
            "refs/elsecall/",
        ])?;
        Ok(String::from_utf8_lossy(&output)
            .lines()
            .filter_map(Oid::parse)
            .collect())
    }

    /// Fetches the first of `candidates` that one of `sources` holds into the object
    /// store, under this copy's own reference for the sync's `nth` import, so it
    /// becomes one of the tips.
    pub async fn supply(
        &self,
        sources: &ObjectSources,
        candidates: &[Oid],
        nth: usize,
    ) -> Option<Oid> {
        let name = match nth {
            0 => "source".to_owned(),
            n => format!("source-{n}"),
        };
        let destination = Destination {
            store: &self.location.store,
            format: self.format,
            reference: &self.reference(&name),
            copy_id: &self.copy_id(),
        };
        sources.supply(&destination, candidates).await
    }

    /// Names this copy among those sharing its object store.
    fn copy_id(&self) -> String {
        copy_id(&self.location.root)
    }

    /// This copy's reference `name` in the shared object store.
    fn reference(&self, name: &str) -> String {
        format!("refs/elsecall/{}/{name}", self.copy_id())
    }

    /// git reading a pack into the object store from its standard input.
    pub fn pack_importer(&self, thin: bool) -> tokio::process::Command {
        let limit = format!("--max-input-size={MAXIMUM_PACK}");
        let mut args = vec!["index-pack", "--stdin", &limit];
        if thin {
            args.push("--fix-thin");
        }
        let mut command = self.store.streaming(args);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command
    }

    /// A private staging area inside the copy's git directory, emptied first.
    pub fn staging(&self) -> Result<Staging> {
        let git = self.git_directory()?;
        git.remove(&path(STAGING))
            .map_err(failed("Clearing the staging area"))?;
        let directory = git
            .subtree(&path(STAGING), Create::Directories)
            .map_err(failed("Creating the staging area"))?
            .ok_or_else(|| sync_failed("The staging area could not be created."))?;
        Ok(Staging::new(directory))
    }

    /// A private area inside the copy's git directory for the snapshot of `run`'s
    /// outputs at exit; removed when dropped.
    pub fn output_staging(&self, run: &str) -> Result<OutputStaging> {
        let git = self.git_directory()?;
        let parent = git
            .subtree(&path(OUTPUT_STAGING), Create::Directories)
            .map_err(failed("Creating the outputs' staging area"))?
            .ok_or_else(|| sync_failed("The outputs' staging area could not be created."))?;
        OutputStaging::create(parent, run).map_err(failed("Creating the outputs' staging area"))
    }

    fn git_directory(&self) -> Result<Confined> {
        self.confined
            .child(".git")
            .map_err(failed("Opening the copy's repository"))?
            .ok_or_else(|| {
                sync_failed(
                    "The copy's .git is no longer a directory; remove the copy on the member.",
                )
            })
    }

    /// Stages the content the copy already holds at `path`, which `verify` found
    /// intact, by cloning it where the file system allows.
    pub fn stage_local(
        &self,
        staging: &mut Staging,
        path: &WorkspacePath,
        digest: Digest,
    ) -> Result<bool> {
        let Some(file) = self
            .confined
            .open_file(path)
            .map_err(failed("Reading the copy"))?
        else {
            return Ok(false);
        };
        staging.clone_in(&file, digest)?;
        Ok(true)
    }

    /// Reads blobs from the object store, keeping those whose content has the
    /// digest the caller listed; filters and line endings can make them differ.
    pub fn stage_objects(
        &self,
        staging: &mut Staging,
        wanted: &[(Oid, Digest)],
    ) -> Result<Vec<Digest>> {
        let mut found = Vec::new();
        if wanted.is_empty() {
            return Ok(found);
        }
        let mut command = self.git.command(["cat-file", "--batch"]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child =
            crate::child_start::spawn(&mut command).map_err(failed("Reading objects"))?;
        let mut input = child.stdin.take().expect("piped");
        let requests: String = wanted.iter().map(|(oid, _)| format!("{oid}\n")).collect();
        let writer = std::thread::spawn(move || {
            let _ = input.write_all(requests.as_bytes());
        });
        let mut output = io::BufReader::with_capacity(1 << 20, child.stdout.take().expect("piped"));
        let result = (|| -> Result<()> {
            for (_, digest) in wanted {
                let mut header = String::new();
                io::BufRead::read_line(&mut output, &mut header)
                    .map_err(failed("Reading objects"))?;
                let fields: Vec<&str> = header.split_whitespace().collect();
                let size: u64 = match fields.as_slice() {
                    [_, "missing"] => continue,
                    [_, kind, size] => {
                        let size = size
                            .parse()
                            .map_err(|_| sync_failed("git cat-file answered unexpectedly."))?;
                        if *kind != "blob" {
                            io::copy(&mut (&mut output).take(size + 1), &mut io::sink())
                                .map_err(failed("Reading objects"))?;
                            continue;
                        }
                        size
                    }
                    _ => return Err(sync_failed("git cat-file answered unexpectedly.")),
                };
                let staged = staging.write_from(&mut (&mut output).take(size), *digest)?;
                let mut newline = [0u8; 1];
                output
                    .read_exact(&mut newline)
                    .map_err(failed("Reading objects"))?;
                if staged {
                    found.push(*digest);
                }
            }
            Ok(())
        })();
        drop(output);
        let _ = child.kill();
        let _ = writer.join();
        let _ = child.wait();
        result.map(|_| found)
    }

    /// Changes the tree to `target`: removes what the caller no longer has and what
    /// runs changed, places staged content, links and modes, then removes untracked
    /// files git does not ignore that the caller does not have. HEAD and the carried
    /// refs are set as the caller has them.
    #[allow(
        clippy::too_many_arguments,
        reason = "each is one part of the caller's state"
    )]
    pub fn apply(
        &mut self,
        target: &Manifest,
        dirty: &[WorkspacePath],
        staging: &Staging,
        excludes: (&Option<Vec<u8>>, &Option<Vec<u8>>),
        index: &IndexListing,
        head: &Head,
        refs: &CarriedRefs,
    ) -> Result<Applied> {
        let mut applied = Applied::default();
        let base = self.record.manifest.clone();
        let mut removals: Vec<&WorkspacePath> = base
            .entries()
            .keys()
            .filter(|path| target.get(path).is_none())
            .collect();
        removals.extend(dirty.iter().filter(|path| target.get(path).is_none()));
        if !removals.is_empty() || !base.delta_to(target).is_empty() {
            // Until the record is saved again, a failure must not leave a record
            // that claims the old content.
            let _ = fs::remove_file(&self.location.state_file);
        }
        for path in removals {
            self.confined
                .remove(path)
                .map_err(failed("Removing a file"))?;
            self.record.written.remove(path);
            applied.removed += 1;
        }
        let same_content = |path: &WorkspacePath, digest: &Digest| matches!(base.get(path), Some(Entry::File { digest: d, .. }) if d == digest);
        let mut uses: HashMap<Digest, usize> = HashMap::new();
        for (path, entry) in target.entries() {
            if let Entry::File { digest, .. } = entry
                && base.get(path) != Some(entry)
                && !same_content(path, digest)
            {
                *uses.entry(*digest).or_default() += 1;
            }
        }
        for (path, entry) in target.entries() {
            if base.get(path) == Some(entry) {
                continue;
            }
            match entry {
                Entry::Symlink { target: link } => {
                    if !self
                        .confined
                        .symlink(path, link, Create::ReplacingOthers)
                        .map_err(failed("Writing a link"))?
                    {
                        return Err(sync_failed(format!(
                            "{path} could not be written on the member."
                        )));
                    }
                    self.record.written.remove(path);
                }
                Entry::File { mode, digest, .. } => {
                    if same_content(path, digest) {
                        self.confined
                            .set_mode(path, *mode)
                            .map_err(failed("Setting a mode"))?;
                    } else {
                        let left = uses.get_mut(digest).expect("counted above");
                        *left -= 1;
                        let name = staging.take(digest, *mode, *left == 0)?;
                        if !self
                            .confined
                            .place(staging.directory(), &name, path, Create::ReplacingOthers)
                            .map_err(failed("Writing a file"))?
                        {
                            return Err(sync_failed(format!(
                                "{path} could not be written on the member."
                            )));
                        }
                    }
                    let status = self
                        .confined
                        .stat(path)
                        .map_err(failed("Inspecting a written file"))?
                        .ok_or_else(|| {
                            sync_failed(format!("{path} vanished while it was written."))
                        })?;
                    self.record
                        .written
                        .insert(path.clone(), Written::from(&status));
                }
            }
            applied.written += 1;
        }
        self.record.manifest = target.clone();
        // Submodules are not copied, but each keeps an empty directory, as in a
        // fresh clone, so git sees them as not checked out rather than deleted.
        for (path, entries) in index.paths() {
            if entries
                .iter()
                .any(|e| e.mode == crate::workspace::index::GITLINK)
                && self
                    .confined
                    .directory(Some(path), Create::ReplacingOthers)
                    .map_err(failed("Creating a submodule's directory"))?
                    .is_none()
            {
                return Err(sync_failed(format!(
                    "{path} could not be created on the member."
                )));
            }
        }
        // The early listing of untracked files holds unless this sync changed
        // what git ignores.
        let ignores_changed = self.write_excludes(excludes)?
            || base
                .delta_to(target)
                .set
                .iter()
                .map(|(path, _)| path)
                .chain(base.delta_to(target).remove.iter())
                .chain(dirty)
                .any(|path| path.file_name() == ".gitignore");
        let index_before = self.record.index_file;
        self.set_index(index)?;
        // Refs first: a run may have left a symbolic ref where HEAD's upstream goes.
        let refs_changed = self.set_refs(refs, head)?;
        let head_changed = self.set_head(head)?;
        applied.extras_removed = self.remove_extras(target, !ignores_changed)?;
        let changed = applied.written + applied.removed + applied.extras_removed > 0
            || head_changed
            || refs_changed
            || index_before != self.record.index_file;
        if changed || !self.location.state_file.exists() {
            // Record the written files' status in the index, so git does not read
            // them again when the command asks it what changed.
            let _ = self.git.try_output(&["update-index", "-q", "--refresh"]);
            self.record.index_file = self.index_file();
            self.save()?;
        }
        Ok(applied)
    }

    /// Writes the caller's ignore rules; returns whether they changed.
    fn write_excludes(&self, (info, global): (&Option<Vec<u8>>, &Option<Vec<u8>>)) -> Result<bool> {
        let git = self.git_directory()?;
        let mut changed = false;
        for (name, content) in [("info/exclude", info), (EXCLUDES, global)] {
            let content = content.as_deref().unwrap_or_default();
            if read_in_git(&self.confined, name).as_deref() != Some(content) {
                write_confined(&git, name, content)?;
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Points HEAD, the branch and its upstream where the caller's are. Returns
    /// whether anything had to change.
    fn set_head(&mut self, head: &Head) -> Result<bool> {
        if self.record.head.as_ref() == Some(head) && head.is_current(&self.git) {
            return Ok(false);
        }
        head.apply(&self.git, self.record.head.as_ref())?;
        for (name, commit) in [
            ("head", head.commit()),
            ("upstream", head.upstream().map(|u| &u.commit)),
        ] {
            let reference = self.reference(name);
            match commit {
                Some(commit) => {
                    self.store
                        .output(&["update-ref", &reference, commit.as_str()])?;
                }
                None => {
                    let _ = self.store.try_output(&["update-ref", "-d", &reference]);
                }
            }
        }
        self.record.head = Some(head.clone());
        Ok(true)
    }

    /// Makes the copy's remote-tracking refs and tags the caller's, and keeps their
    /// objects in the object store under this copy's references, so later syncs
    /// send history against them. Returns whether anything had to change.
    fn set_refs(&mut self, refs: &CarriedRefs, head: &Head) -> Result<bool> {
        let early = self.refs.take().and_then(|listing| listing.join().ok());
        let current = match early {
            Some(Ok(current)) => current,
            _ => CarriedRefs::list_copy(&self.git)?,
        };
        let copy_changed = match refs.apply(&self.git, head.upstream(), &current) {
            Ok(changed) => changed,
            // A run can leave what git does not list but still blocks a ref, such
            // as a symbolic ref to nothing where a carried ref needs a directory.
            // These namespaces hold only what the caller sends, so they start over.
            Err(_) => {
                let git = self.git_directory()?;
                for namespace in ["refs/remotes", "refs/tags"] {
                    git.remove(&path(namespace))
                        .map_err(failed("Clearing the copy's refs"))?;
                }
                let current = CarriedRefs::list_copy(&self.git)?;
                refs.apply(&self.git, head.upstream(), &current)?;
                true
            }
        };
        if self.record.refs.as_ref() == Some(refs) {
            return Ok(copy_changed);
        }
        // Store references are Elsecall's own: they change only when the refs do.
        let prefix = self.reference("carried/");
        let listed = self.store.output(&[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)",
            &prefix,
        ])?;
        let listed = String::from_utf8_lossy(&listed);
        let wanted: HashMap<String, &Oid> = refs
            .entries()
            .map(|(name, oid)| {
                let name = name.as_str().strip_prefix("refs/").expect("full names");
                (format!("{prefix}{name}"), oid)
            })
            .collect();
        let mut deletions = String::new();
        let mut current = std::collections::HashSet::new();
        for line in listed.lines() {
            let Some((name, object)) = line.split_once('\0') else {
                continue;
            };
            match wanted.get(name) {
                Some(oid) if oid.as_str() == object => {
                    current.insert(name.to_owned());
                }
                Some(_) => (),
                None => deletions.push_str(&format!("delete {name}\n")),
            }
        }
        let updates: String = wanted
            .iter()
            .filter(|(name, _)| !current.contains(*name))
            .map(|(name, oid)| format!("update {name} {oid}\n"))
            .collect();
        // Removals first and on their own, so a ref may replace a directory of refs.
        for input in [deletions, updates] {
            if !input.is_empty() {
                self.store
                    .output_with(&["update-ref", "--stdin"], Some(input.as_bytes()))?;
            }
        }
        self.record.refs = Some(refs.clone());
        Ok(true)
    }

    /// Removes untracked files git does not ignore that the caller does not have, as
    /// a run may leave. Ignored files and nested repositories stay.
    fn remove_extras(&mut self, target: &Manifest, early: bool) -> Result<usize> {
        let listed = self
            .untracked
            .take()
            .and_then(|listing| listing.join().ok());
        let output = match listed {
            Some(Ok(output)) if early => output,
            _ => self
                .git
                .output(&["ls-files", "-z", "--others", "--exclude-standard"])?,
        };
        let mut removed = 0;
        for record in output.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            if record.ends_with(b"/") {
                continue;
            }
            let Some(path) = std::str::from_utf8(record)
                .ok()
                .and_then(WorkspacePath::parse)
            else {
                continue;
            };
            // The early listing may name what this sync already removed.
            let exists = self
                .confined
                .stat(&path)
                .map_err(failed("Inspecting the copy"))?
                .is_some();
            if exists && target.get(&path).is_none() {
                self.confined
                    .remove(&path)
                    .map_err(failed("Removing a file"))?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.location.state_file.parent() {
            private_directory(parent)?;
        }
        self.record
            .save(&self.location.state_file)
            .map_err(failed("Recording the copy"))
    }
}

/// The name a copy's references carry in its object store.
pub fn copy_id(root: &Path) -> String {
    Digest::of(root.as_os_str().as_encoded_bytes()).hex()[..16].to_owned()
}

/// Whose copy the directory at `root` is, from the mark written before anything
/// else: the calling member and the caller's path. `None` for anything that is not
/// a directory marked as a copy, including a link.
pub fn owner(root: &Path) -> Option<(String, String)> {
    let marker = read_in_git(&Confined::open(root).ok()?, MARKER)?;
    let text = String::from_utf8(marker).ok()?;
    let mut lines = text.strip_suffix('\n')?.split('\n');
    let (member, caller_root) = (lines.next()?, lines.next()?);
    lines
        .next()
        .is_none()
        .then(|| (member.to_owned(), caller_root.to_owned()))
}

/// Whether the directory at `root` is the copy `location` names.
pub fn is_copy(root: &Path, location: &CopyLocation) -> bool {
    owner(root).is_some_and(|(member, caller_root)| {
        member == location.calling_member && caller_root == location.caller_root
    })
}

/// The object store the copy at `root` reads its history from.
pub fn store_of(root: &Path) -> Option<std::path::PathBuf> {
    let alternates = read_in_git(&Confined::open(root).ok()?, "objects/info/alternates")?;
    let text = String::from_utf8(alternates).ok()?;
    let objects = Path::new(text.lines().next()?);
    if objects.file_name()? != "objects" {
        return None;
    }
    objects.parent().map(Path::to_owned)
}

/// Copies the store at `store` keeps references for, by the name `copy_id` gives.
pub fn referencing_copies(store: &Path) -> Result<std::collections::BTreeSet<String>> {
    let output =
        Git::member(store).output(&["for-each-ref", "--format=%(refname)", "refs/elsecall/"])?;
    Ok(String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|name| name.strip_prefix("refs/elsecall/")?.split('/').next())
        .map(str::to_owned)
        .collect())
}

/// Drops the store's references for the copy that lived at `root`, so the store
/// no longer counts it.
pub fn release_store(store: &Path, root: &Path) -> Result<()> {
    let git = Git::member(store);
    let prefix = format!("refs/elsecall/{}/", copy_id(root));
    let output = git.output(&["for-each-ref", "--format=%(refname)", &prefix])?;
    for name in String::from_utf8_lossy(&output).lines() {
        git.output(&["update-ref", "-d", name])?;
    }
    Ok(())
}

/// Reads a small file inside the copy's git directory, never through a link.
fn read_in_git(copy: &Confined, name: &str) -> Option<Vec<u8>> {
    let git = copy.child(".git").ok()??;
    let mut file = git.open_file(&WorkspacePath::parse(name)?).ok()??;
    let mut content = Vec::new();
    (&mut file)
        .take(super::snapshot::MAXIMUM_EXCLUDES as u64 + 1)
        .read_to_end(&mut content)
        .ok()?;
    Some(content)
}

fn write_in_git(copy: &Confined, name: &str, content: &[u8]) -> Result<()> {
    let git = copy
        .child(".git")
        .map_err(failed("Opening the copy's repository"))?
        .ok_or_else(|| sync_failed("The copy's .git is not a directory."))?;
    write_confined(&git, name, content)
}

fn write_confined(git: &Confined, name: &str, content: &[u8]) -> Result<()> {
    if git
        .write_file(&path(name), content, Create::Directories)
        .map_err(failed("Writing inside the copy's repository"))?
    {
        Ok(())
    } else {
        Err(sync_failed(
            "A directory inside the copy's .git was replaced; remove the copy on the member.",
        ))
    }
}
