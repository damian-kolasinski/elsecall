//! Synced paths on this member's disk: finding where one really lives, reading its
//! tree without following links inside it, and replacing it atomically.
//!
//! Directories above a synced path are this account's own and may be links, such
//! as a `~/.config` kept in a dotfiles repository; they are followed, and where the
//! path really resolves is checked against the same rules as the declaration.
//! Nothing below the synced path is ever followed: links and special files there
//! are skipped when reading and replaced when writing.
use crate::{
    config_sync::{
        fingerprint::{Fingerprint, SyncKey},
        manifest::{
            FileKey, FileNode, MAXIMUM_ENTRIES, MAXIMUM_FILE_BYTES, MAXIMUM_TOTAL_BYTES, Node, Tree,
        },
        synced_path::SyncedPath,
    },
    failure::{Failure, Result},
    workspace::{
        confined::{self, Confined, Create, Kind, Status},
        path::WorkspacePath,
    },
};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::{CStr, CString},
    fs::File,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use zeroize::Zeroizing;

/// A synced path's place: the directory holding it, open, and its name there.
pub struct Location {
    parent: Arc<OwnedFd>,
    name: CString,
}

fn io(action: &str) -> impl Fn(std::io::Error) -> Failure + '_ {
    move |e| Failure::io(action, e)
}

fn unsafe_path(path: &SyncedPath, why: &str) -> Failure {
    Failure::new(
        "unsafe_sync_path",
        format!("{path} cannot be synced on this member: {why}."),
        "Check the directories above it on this member (links to forbidden places are refused), or disable the path with elsecall sync disable.",
    )
}

fn open_directory(path: &Path) -> std::io::Result<OwnedFd> {
    let text = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: a valid C string; the descriptor returned is owned below.
    let fd = unsafe {
        libc::open(
            text.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: open returned a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Where the open directory really is, after every link.
fn real_path(directory: &OwnedFd) -> std::io::Result<PathBuf> {
    let mut buffer = vec![0u8; libc::PATH_MAX as usize + 1];
    // SAFETY: F_GETPATH writes at most MAXPATHLEN bytes into a buffer that large.
    let result = unsafe {
        libc::fcntl(
            directory.as_raw_fd(),
            libc::F_GETPATH,
            buffer.as_mut_ptr().cast::<libc::c_char>(),
        )
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let length = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
    buffer.truncate(length);
    Ok(PathBuf::from(String::from_utf8_lossy(&buffer).into_owned()))
}

/// What a member must also keep sync away from, beyond the fixed rules: its
/// workspace root, for one.
#[derive(Clone, Debug)]
pub struct Home {
    pub home: PathBuf,
    pub real_home: PathBuf,
    /// Further home-relative locations synced paths must not overlap.
    pub forbidden: Vec<String>,
}

/// Resolves links in existing ancestors even when a future workspace root's
/// final directories have not been created yet.
pub fn resolved_location(path: &Path) -> std::io::Result<PathBuf> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                for component in missing.into_iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(ancestor).is_ok_and(|m| m.file_type().is_symlink()) {
                    return Err(error);
                }
                let Some(name) = ancestor.file_name() else {
                    return Err(error);
                };
                missing.push(name.to_owned());
                ancestor = ancestor.parent().expect("absolute location has a parent");
            }
            Err(error) => return Err(error),
        }
    }
}

impl Home {
    pub fn new(home: &Path, forbidden_absolute: &[PathBuf]) -> Self {
        let real_home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_owned());
        let forbidden = forbidden_absolute
            .iter()
            .filter_map(|path| {
                let path = resolved_location(path).unwrap_or_else(|_| path.clone());
                let relative = path
                    .strip_prefix(&real_home)
                    .or_else(|_| path.strip_prefix(home))
                    .ok()?;
                relative.to_str().map(str::to_owned)
            })
            .filter(|relative| !relative.is_empty())
            .collect();
        Self {
            home: home.to_owned(),
            real_home,
            forbidden,
        }
    }

    /// Checks `path` against this member's own rules, lexically.
    pub fn check(&self, path: &SyncedPath) -> Result<()> {
        match path.forbidden_overlap(self.forbidden.iter().map(String::as_str)) {
            Some(location) => Err(unsafe_path(
                path,
                &format!("it overlaps ~/{location}, which this member keeps for Elsecall"),
            )),
            None => Ok(()),
        }
    }

    /// Where the open `directory` really is, below home, after every link.
    fn real_relative(&self, path: &SyncedPath, directory: &OwnedFd) -> Result<String> {
        let real = real_path(directory).map_err(io("Resolve a synced path's directory"))?;
        let inside = real
            .strip_prefix(&self.real_home)
            .map_err(|_| unsafe_path(path, "it resolves outside home"))?;
        Ok(inside.to_string_lossy().into_owned())
    }

    /// Refuses `relative` (below home, as the disk names it) when it is, lies
    /// inside or contains a forbidden location.
    fn check_resolved(&self, path: &SyncedPath, relative: &str) -> Result<()> {
        match SyncedPath::resolved_overlap(relative, &self.forbidden) {
            None => Ok(()),
            Some(location) if location.is_empty() => {
                Err(unsafe_path(path, "it resolves to home itself"))
            }
            Some(location) => Err(unsafe_path(
                path,
                &format!("it resolves to a place overlapping ~/{location}"),
            )),
        }
    }

    /// Opens the directory that holds `path`, creating missing directories
    /// owner-only when `create`. `None` when it is missing and not created.
    ///
    /// Existing directories above the path are followed, and where they really are
    /// is checked before anything is created; missing ones are then created below
    /// the checked directory without following anything. Finally the path itself,
    /// if present, is checked as the file system names it, since a case-insensitive
    /// file system may resolve a name to a different, forbidden one.
    pub fn locate(&self, path: &SyncedPath, create: bool) -> Result<Option<Location>> {
        self.check(path)?;
        let components = path.parent_components();
        let mut existing = self.home.clone();
        let mut missing_from = components.len();
        for (index, component) in components.iter().enumerate() {
            let next = existing.join(component);
            match std::fs::metadata(&next) {
                Ok(meta) if meta.is_dir() => existing = next,
                Ok(_) => return Err(unsafe_path(path, "a file is in the way")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    missing_from = index;
                    break;
                }
                Err(e) => return Err(Failure::io("Inspect a synced path's directory", e)),
            }
        }
        let mut directory =
            open_directory(&existing).map_err(io("Open a synced path's directory"))?;
        let base = self.real_relative(path, &directory)?;
        let mut planned = base.clone();
        for component in components[missing_from..].iter().chain([&path.file_name()]) {
            planned = format!("{planned}/{component}");
        }
        self.check_resolved(path, &planned)?;
        if missing_from < components.len() && !create {
            return Ok(None);
        }
        for component in &components[missing_from..] {
            let name =
                CString::new(*component).map_err(|_| unsafe_path(path, "it contains NUL"))?;
            // SAFETY: valid descriptor and C string.
            if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(Failure::io("Create a synced path's directory", error));
                }
            }
            directory = Confined::open_at(&directory, &name)
                .map_err(|_| {
                    unsafe_path(path, "a directory above it changed while it was created")
                })?
                .root()
                .try_clone()
                .map_err(io("Open a synced path's directory"))?;
        }
        let parent = self.real_relative(path, &directory)?;
        self.check_resolved(path, &format!("{parent}/{}", path.file_name()))?;
        let name =
            CString::new(path.file_name()).map_err(|_| unsafe_path(path, "it contains NUL"))?;
        if let Some(real) = real_entry(&directory, &name).map_err(io("Inspect a synced path"))? {
            let inside = real
                .strip_prefix(&self.real_home)
                .map_err(|_| unsafe_path(path, "it resolves outside home"))?;
            self.check_resolved(path, &inside.to_string_lossy())?;
        }
        Ok(Some(Location {
            parent: Arc::new(directory),
            name,
        }))
    }
}

/// Where the entry `name` in `directory` really is, as the file system names it,
/// without following it if it is a link. `None` when nothing is there.
fn real_entry(directory: &OwnedFd, name: &CStr) -> std::io::Result<Option<PathBuf>> {
    // SAFETY: valid descriptor and C string. O_EVTONLY needs no read permission,
    // O_SYMLINK opens a link itself, and O_NONBLOCK keeps a FIFO from blocking.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_EVTONLY | libc::O_SYMLINK | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(None),
            _ => Err(error),
        };
    }
    // SAFETY: openat returned a new descriptor nothing else owns.
    let entry = unsafe { OwnedFd::from_raw_fd(fd) };
    real_path(&entry).map(Some)
}

/// Opens the place `path` would have below an overrides root, without following
/// any link below the root. `None` when nothing is there.
pub fn locate_inside(root: &Confined, path: &SyncedPath) -> Result<Option<Location>> {
    let parent = match path.parent_components().join("/") {
        parent if parent.is_empty() => root.root().try_clone().map(Arc::new),
        parent => {
            let Some(parent) = WorkspacePath::parse(&parent) else {
                return Ok(None);
            };
            match root
                .directory(Some(&parent), Create::No)
                .map_err(io("Open overrides"))?
            {
                Some(directory) => directory.try_clone().map(Arc::new),
                None => return Ok(None),
            }
        }
    }
    .map_err(io("Open overrides"))?;
    Ok(Some(Location {
        parent,
        name: CString::new(path.file_name()).expect("checked component"),
    }))
}

/// What reading a tree found: the tree, and entries it skipped (links, special
/// files, names Elsecall cannot carry), by their path inside it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LiveTree {
    pub tree: Tree,
    pub skipped: Vec<String>,
}

/// Remembers fingerprints by file identity and change times, so an unchanged
/// file is never read again.
#[derive(Default)]
pub struct FingerprintCache(Mutex<HashMap<(u64, u64), (Signature, Fingerprint)>>);

#[derive(Clone, Copy, PartialEq, Eq)]
struct Signature {
    size: u64,
    modified_ns: i64,
    changed_ns: i64,
    /// Which key the fingerprint is under.
    key: [u8; 8],
}

const CACHED_FINGERPRINTS: usize = 20_000;

impl FingerprintCache {
    fn get(&self, device_inode: (u64, u64), signature: Signature) -> Option<Fingerprint> {
        let cache = self.0.lock().expect("cache lock");
        cache
            .get(&device_inode)
            .filter(|(seen, _)| *seen == signature)
            .map(|(_, fingerprint)| *fingerprint)
    }
    fn put(&self, device_inode: (u64, u64), signature: Signature, fingerprint: Fingerprint) {
        let mut cache = self.0.lock().expect("cache lock");
        if cache.len() >= CACHED_FINGERPRINTS {
            cache.clear();
        }
        cache.insert(device_inode, (signature, fingerprint));
    }
}

fn too_large() -> Failure {
    Failure::new(
        "sync_path_too_large",
        format!(
            "A synced path holds more than {MAXIMUM_ENTRIES} entries, a file over {} MiB, or more than {} MiB in all.",
            MAXIMUM_FILE_BYTES >> 20,
            MAXIMUM_TOTAL_BYTES >> 20
        ),
        "Sync a smaller directory, or disable the path with elsecall sync disable.",
    )
}

struct Reader<'a> {
    key: &'a SyncKey,
    cache: &'a FingerprintCache,
    entries: usize,
    bytes: u64,
}

impl Reader<'_> {
    fn file(&mut self, file: File, status: Status) -> Result<FileNode> {
        self.entries += 1;
        self.bytes += status.size;
        if status.size > MAXIMUM_FILE_BYTES
            || self.bytes > MAXIMUM_TOTAL_BYTES
            || self.entries > MAXIMUM_ENTRIES
        {
            return Err(too_large());
        }
        let device = confined::fstat(&file).map_err(io("Inspect a synced file"))?;
        let identity = (device_of(&file), device.inode);
        let signature = Signature {
            size: device.size,
            modified_ns: device.modified_ns,
            changed_ns: device.changed_ns,
            key: self.key.digest_labelled("cache", &[]).0[..8]
                .try_into()
                .expect("8 bytes"),
        };
        let fingerprint = match self.cache.get(identity, signature) {
            Some(fingerprint) => fingerprint,
            None => {
                let content = read_bounded(file)?;
                let fingerprint = self.key.fingerprint(&content);
                self.cache.put(identity, signature, fingerprint);
                fingerprint
            }
        };
        Ok(FileNode {
            mode: status.mode & 0o700,
            size: device.size,
            fingerprint,
        })
    }
}

fn device_of(file: &File) -> u64 {
    use std::os::unix::fs::MetadataExt;
    file.metadata().map(|m| m.dev()).unwrap_or_default()
}

fn read_bounded(file: File) -> Result<Zeroizing<Vec<u8>>> {
    let mut content = Zeroizing::new(Vec::new());
    file.take(MAXIMUM_FILE_BYTES + 1)
        .read_to_end(&mut content)
        .map_err(io("Read a synced file"))?;
    if content.len() as u64 > MAXIMUM_FILE_BYTES {
        return Err(too_large());
    }
    Ok(content)
}

/// Reads the tree at `location`, fingerprinting files under `key`.
pub fn read_tree(location: &Location, key: &SyncKey, cache: &FingerprintCache) -> Result<LiveTree> {
    let mut reader = Reader {
        key,
        cache,
        entries: 0,
        bytes: 0,
    };
    let status = confined::stat_at(location.parent.as_raw_fd(), &location.name)
        .map_err(io("Inspect a synced path"))?;
    match status.map(|s| (s.kind, s)) {
        None => Ok(LiveTree::default()),
        Some((Kind::File, status)) => {
            let file = Confined::open_child_file(&location.parent, &location.name)
                .map_err(io("Open a synced file"))?;
            Ok(LiveTree {
                tree: Tree::File(reader.file(file, status)?),
                skipped: Vec::new(),
            })
        }
        Some((Kind::Directory, _)) => {
            let root = Confined::open_at(&location.parent, &location.name)
                .map_err(io("Open a synced directory"))?;
            let mut entries = BTreeMap::new();
            let mut skipped = Vec::new();
            let mut pending: Vec<Option<WorkspacePath>> = vec![None];
            while let Some(directory) = pending.pop() {
                let names = root
                    .list(directory.as_ref())
                    .map_err(io("List a synced directory"))?
                    .unwrap_or_default();
                for name in names {
                    // A write in progress, or one a crash interrupted, is not content.
                    if is_temporary(name.to_bytes()) {
                        continue;
                    }
                    let text = name.to_str().ok();
                    let path = text.and_then(|text| match &directory {
                        Some(directory) => directory.join(text),
                        None => WorkspacePath::parse(text),
                    });
                    let Some(path) = path else {
                        let prefix = directory.as_ref().map(|d| format!("{}/", d.as_str()));
                        skipped.push(format!(
                            "{}{}",
                            prefix.unwrap_or_default(),
                            name.to_string_lossy()
                        ));
                        continue;
                    };
                    let status = root.stat(&path).map_err(io("Inspect a synced entry"))?;
                    match status.map(|s| (s.kind, s)) {
                        Some((Kind::Directory, _)) => {
                            reader.entries += 1;
                            if reader.entries > MAXIMUM_ENTRIES {
                                return Err(too_large());
                            }
                            entries.insert(path.clone(), Node::Directory);
                            pending.push(Some(path));
                        }
                        Some((Kind::File, status)) => {
                            match root.open_file(&path).map_err(io("Open a synced file"))? {
                                Some(file) => {
                                    entries.insert(path, Node::File(reader.file(file, status)?));
                                }
                                None => skipped.push(path.as_str().to_owned()),
                            }
                        }
                        // Gone meanwhile, a link, or a special file.
                        None => (),
                        Some(_) => skipped.push(path.as_str().to_owned()),
                    }
                }
            }
            if skipped.len() > MAXIMUM_ENTRIES {
                return Err(too_large());
            }
            skipped.sort();
            Ok(LiveTree {
                tree: Tree::Directory(entries),
                skipped,
            })
        }
        // The path itself is a link or special file: nothing synced is there.
        Some(_) => Ok(LiveTree {
            tree: Tree::Absent,
            skipped: vec![String::new()],
        }),
    }
}

/// Reads one file of the tree at `location`, never through a link.
pub fn read_file(location: &Location, key: &FileKey) -> Result<Zeroizing<Vec<u8>>> {
    let file = match key {
        None => Confined::open_child_file(&location.parent, &location.name)
            .map_err(io("Read a synced file"))?,
        Some(path) => Confined::open_at(&location.parent, &location.name)
            .and_then(|root| {
                root.open_file(path)?
                    .ok_or_else(|| std::io::Error::other("not a regular file"))
            })
            .map_err(io("Read a synced file"))?,
    };
    read_bounded(file)
}

fn temporary() -> Result<CString> {
    confined::temporary_name().map_err(io("Name a temporary file"))
}

fn rename_at(parent: &OwnedFd, from: &CStr, to: &CStr) -> std::io::Result<()> {
    // SAFETY: valid descriptor and C strings.
    let result = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn chmod(fd: &impl AsRawFd, mode: u32) -> std::io::Result<()> {
    // SAFETY: valid descriptor.
    if unsafe { libc::fchmod(fd.as_raw_fd(), mode as libc::mode_t) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn write_content(file: &mut File, content: &[u8], mode: u32) -> std::io::Result<()> {
    file.write_all(content)?;
    chmod(file, mode)?;
    file.sync_all()
}

/// Where a written file's content comes from.
pub type Content<'a> = dyn Fn(&FileKey, &FileNode) -> Result<Zeroizing<Vec<u8>>> + 'a;

/// Whether `name` is one of the temporary names Elsecall writes beside or inside
/// synced paths, such as `.elsecall-3f9a0c41d2b7e811`.
pub fn is_temporary(name: &[u8]) -> bool {
    name.strip_prefix(b".elsecall-")
        .is_some_and(|rest| rest.len() == 16 && rest.iter().all(|b| b.is_ascii_hexdigit()))
}

/// Removes temporary trees a crash left beside the synced path.
fn remove_leftovers(parent: &OwnedFd) {
    let Ok(Some(listing)) = Confined::open_at(parent, c".").and_then(|d| d.list(None)) else {
        return;
    };
    for name in listing {
        if is_temporary(name.to_bytes()) {
            let _ = confined::remove_at(parent, &name);
        }
    }
}

/// What to do with the tree a replacement swaps out.
pub enum Previous<'a> {
    /// Remove it: everything of it worth keeping was backed up.
    Remove,
    /// Keep it whole at this path, owner-only, as it holds entries sync cannot
    /// carry (links, special files, `.git`).
    KeepAt(&'a Path),
}

/// Replaces whatever is at `location` with `desired`, owner-only, in one atomic
/// step: the new tree is built beside it and swapped in, so readers see the old
/// tree or the new one, and a failure leaves the old one in place. `before_swap`
/// runs once the new tree is complete, just before it replaces the old one; an
/// error there leaves the old one in place too. Returns where the previous tree was
/// kept, when it was.
pub fn write_tree(
    location: &Location,
    desired: &Tree,
    content: &Content,
    before_swap: &mut dyn FnMut() -> Result<()>,
    previous: Previous,
) -> Result<Option<PathBuf>> {
    let parent = &location.parent;
    remove_leftovers(parent);
    let staging = temporary()?;
    let built: Result<bool> = (|| {
        let swapped = match desired {
            Tree::Absent => {
                before_swap()?;
                // Move it aside in one step, then remove it.
                match rename_at(parent, &location.name, &staging) {
                    Ok(()) => true,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                    Err(e) => return Err(Failure::io("Remove a synced path", e)),
                }
            }
            Tree::File(node) => {
                let mut file =
                    Confined::create_at(parent, &staging).map_err(io("Create a synced file"))?;
                write_content(&mut file, &content(&None, node)?, node.mode)
                    .map_err(io("Write a synced file"))?;
                before_swap()?;
                Confined::swap_at(parent, &staging, &location.name)
                    .map_err(io("Replace a synced file"))?
            }
            Tree::Directory(entries) => {
                // SAFETY: valid descriptor and C string.
                if unsafe { libc::mkdirat(parent.as_raw_fd(), staging.as_ptr(), 0o700) } < 0 {
                    return Err(Failure::io(
                        "Create a synced directory",
                        std::io::Error::last_os_error(),
                    ));
                }
                let tree =
                    Confined::open_at(parent, &staging).map_err(io("Open a synced directory"))?;
                chmod(tree.root(), 0o700).map_err(io("Protect a synced directory"))?;
                // Parents sort before their children.
                for (path, node) in entries {
                    match node {
                        Node::Directory => {
                            let made = tree
                                .directory(Some(path), Create::Directories)
                                .map_err(io("Create a synced directory"))?
                                .ok_or_else(|| {
                                    Failure::io(
                                        "Create a synced directory",
                                        std::io::Error::other("blocked"),
                                    )
                                })?;
                            chmod(&*made, 0o700).map_err(io("Protect a synced directory"))?;
                        }
                        Node::File(file_node) => {
                            let (mut file, holder, name) = tree
                                .temporary(path, Create::Directories)
                                .map_err(io("Create a synced file"))?
                                .ok_or_else(|| {
                                    Failure::io(
                                        "Create a synced file",
                                        std::io::Error::other("blocked"),
                                    )
                                })?;
                            write_content(
                                &mut file,
                                &content(&Some(path.clone()), file_node)?,
                                file_node.mode,
                            )
                            .map_err(io("Write a synced file"))?;
                            let placed = tree
                                .place(&holder, &name, path, Create::Directories)
                                .map_err(io("Place a synced file"))?;
                            if !placed {
                                return Err(Failure::io(
                                    "Place a synced file",
                                    std::io::Error::other("a directory is in the way"),
                                ));
                            }
                        }
                    }
                }
                before_swap()?;
                Confined::swap_at(parent, &staging, &location.name)
                    .map_err(io("Replace a synced directory"))?
            }
        };
        Ok(swapped)
    })();
    // Whatever is left at the staging name is the old tree or a failed build.
    let mut kept = None;
    if let (Ok(true), Previous::KeepAt(target)) = (&built, previous) {
        let target_name = CString::new(target.as_os_str().as_encoded_bytes()).map_err(|_| {
            Failure::io(
                "Keep a synced path",
                std::io::ErrorKind::InvalidInput.into(),
            )
        })?;
        // SAFETY: valid descriptor and C strings; the target is absolute.
        let moved = unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                staging.as_ptr(),
                libc::AT_FDCWD,
                target_name.as_ptr(),
            )
        };
        if moved == 0 {
            kept = Some(target.to_owned());
        } else {
            // Another volume: keep it beside the path, under a name that says so.
            let name = format!(
                "{}.elsecall-kept-{}",
                location.name.to_string_lossy(),
                &staging.to_string_lossy()[".elsecall-".len()..]
            );
            let beside = CString::new(name.clone()).expect("no NUL");
            rename_at(parent, &staging, &beside).map_err(io("Keep a synced path"))?;
            let at = real_path(parent)
                .map_err(io("Keep a synced path"))?
                .join(name);
            return built.map(|_| Some(at));
        }
    }
    let _ = confined::remove_at(parent, &staging);
    built.map(|_| kept)
}

fn conflict() -> Failure {
    Failure::new(
        "override_conflict",
        "An override on the master does not fit the shape of the master's own files (a file where a directory is, or the reverse).",
        "Change the override so it names a file where the master has a file or nothing, then wait a second.",
    )
}

/// Writes one file of the tree at `location` in place, atomically, never
/// replacing a directory. A master lays its own overrides this way, so nothing
/// else in its tree, the source, is touched.
pub fn write_file(
    location: &Location,
    key: &FileKey,
    node: &FileNode,
    content: &[u8],
) -> Result<()> {
    let Some(path) = key else {
        if matches!(
            confined::stat_at(location.parent.as_raw_fd(), &location.name)
                .map_err(io("Inspect a synced path"))?,
            Some(Status {
                kind: Kind::Directory,
                ..
            })
        ) {
            return Err(conflict());
        }
        let staging = temporary()?;
        let written = (|| {
            let mut file = Confined::create_at(&location.parent, &staging)
                .map_err(io("Create a synced file"))?;
            write_content(&mut file, content, node.mode).map_err(io("Write a synced file"))?;
            rename_at(&location.parent, &staging, &location.name)
                .map_err(io("Replace a synced file"))
        })();
        if written.is_err() {
            let _ = confined::remove_at(&location.parent, &staging);
        }
        return written;
    };
    let tree = Confined::open_at(&location.parent, &location.name).map_err(|_| conflict())?;
    let (mut file, holder, name) = tree
        .temporary(path, Create::Directories)
        .map_err(io("Create a synced file"))?
        .ok_or_else(conflict)?;
    let placed = write_content(&mut file, content, node.mode)
        .map_err(io("Write a synced file"))
        .and_then(|()| {
            tree.place(&holder, &name, path, Create::Directories)
                .map_err(io("Place a synced file"))
        });
    match placed {
        Ok(true) => Ok(()),
        Ok(false) => {
            let _ = confined::remove_at(&holder, &name);
            Err(conflict())
        }
        Err(error) => {
            let _ = confined::remove_at(&holder, &name);
            Err(error)
        }
    }
}

/// Removes one regular file of the tree at `location`; never a directory.
pub fn remove_file(location: &Location, key: &FileKey) -> Result<()> {
    match key {
        None => match confined::stat_at(location.parent.as_raw_fd(), &location.name)
            .map_err(io("Inspect a synced path"))?
        {
            Some(Status {
                kind: Kind::File, ..
            }) => confined::remove_at(&location.parent, &location.name)
                .map_err(io("Remove a synced file")),
            None => Ok(()),
            Some(_) => Err(conflict()),
        },
        Some(path) => {
            let Ok(tree) = Confined::open_at(&location.parent, &location.name) else {
                return Ok(());
            };
            match tree.stat(path).map_err(io("Inspect a synced file"))? {
                Some(Status {
                    kind: Kind::File, ..
                }) => tree.remove(path).map_err(io("Remove a synced file")),
                None => Ok(()),
                Some(_) => Err(conflict()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions_resolve_symlink_parents_before_future_directories_exist() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(home.join(".config/demo")).unwrap();
        std::os::unix::fs::symlink(home.join(".config/demo"), home.join("alias")).unwrap();
        let root = home.join("alias/not-created/workspaces");
        assert_eq!(
            resolved_location(&root).unwrap(),
            home.join(".config/demo/not-created/workspaces")
        );
        let checked = Home::new(&home, &[root]);
        assert!(
            checked
                .check(&SyncedPath::parse("~/.config/demo").unwrap())
                .is_err()
        );
        assert!(!home.join(".config/demo/not-created").exists());
    }

    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn locates_reads_and_replaces_trees_atomically_and_owner_only() {
        let home = tempfile::tempdir().unwrap();
        let key = SyncKey::generate().unwrap();
        let cache = FingerprintCache::default();
        let place = Home::new(home.path(), &[]);
        let path = SyncedPath::parse("~/.config/demo").unwrap();
        assert!(place.locate(&path, false).unwrap().is_none());
        let location = place.locate(&path, true).unwrap().unwrap();
        assert_eq!(mode(&home.path().join(".config")), 0o700);
        assert_eq!(
            read_tree(&location, &key, &cache).unwrap().tree,
            Tree::Absent
        );

        let target = home.path().join(".config/demo");
        std::fs::create_dir_all(target.join("sub")).unwrap();
        std::fs::write(target.join("sub/a"), "alpha").unwrap();
        std::fs::write(target.join("b"), "beta").unwrap();
        std::fs::set_permissions(target.join("b"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", target.join("link")).unwrap();
        let live = read_tree(&location, &key, &cache).unwrap();
        assert_eq!(live.skipped, ["link"]);
        let files = live.tree.files();
        let b = Some(WorkspacePath::parse("b").unwrap());
        assert_eq!(files[&b].mode, 0o700, "owner bits only");
        assert_eq!(files[&b].fingerprint, key.fingerprint(b"beta"));
        assert_eq!(&*read_file(&location, &b).unwrap(), b"beta");

        // Replace with a new tree: links go, modes are owner-only.
        let new = Tree::Directory(BTreeMap::from([(
            WorkspacePath::parse("c").unwrap(),
            Node::File(FileNode {
                mode: 0o600,
                size: 5,
                fingerprint: key.fingerprint(b"gamma"),
            }),
        )]));
        // A failing step before the swap leaves the previous tree in place.
        let refused = write_tree(
            &location,
            &new,
            &|_, _| Ok(Zeroizing::new(b"gamma".to_vec())),
            &mut || Err(crate::config_sync::store::mismatch()),
            Previous::Remove,
        );
        assert!(refused.is_err());
        assert_eq!(std::fs::read(target.join("b")).unwrap(), b"beta");
        // The previous tree, with its link, is kept whole where asked.
        let kept_at = home.path().join("kept");
        let mut swapped = false;
        let kept = write_tree(
            &location,
            &new,
            &|_, _| Ok(Zeroizing::new(b"gamma".to_vec())),
            &mut || {
                swapped = true;
                Ok(())
            },
            Previous::KeepAt(&kept_at),
        )
        .unwrap();
        assert!(swapped);
        assert_eq!(kept.as_deref(), Some(kept_at.as_path()));
        assert!(std::fs::symlink_metadata(kept_at.join("link")).is_ok());
        assert_eq!(std::fs::read(kept_at.join("b")).unwrap(), b"beta");
        assert_eq!(std::fs::read(target.join("c")).unwrap(), b"gamma");
        assert!(!target.join("b").exists());
        assert!(std::fs::symlink_metadata(target.join("link")).is_err());
        assert_eq!(mode(&target), 0o700);
        assert_eq!(mode(&target.join("c")), 0o600);
        assert_eq!(read_tree(&location, &key, &cache).unwrap().tree, new);
        let leftovers: Vec<_> = std::fs::read_dir(home.path().join(".config"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(leftovers, ["demo"]);

        // A failing content source leaves the previous tree in place.
        let other = Tree::Directory(BTreeMap::from([(
            WorkspacePath::parse("d").unwrap(),
            Node::File(FileNode {
                mode: 0o600,
                size: 1,
                fingerprint: key.fingerprint(b"d"),
            }),
        )]));
        assert!(
            write_tree(
                &location,
                &other,
                &|_, _| Err(crate::config_sync::store::mismatch()),
                &mut || Ok(()),
                Previous::Remove,
            )
            .is_err()
        );
        assert_eq!(read_tree(&location, &key, &cache).unwrap().tree, new);

        // One file in place: written atomically, then removed; directories never.
        let note = Some(WorkspacePath::parse("notes/n").unwrap());
        let node = FileNode {
            mode: 0o600,
            size: 1,
            fingerprint: key.fingerprint(b"n"),
        };
        write_file(&location, &note, &node, b"n").unwrap();
        assert_eq!(std::fs::read(target.join("notes/n")).unwrap(), b"n");
        assert_eq!(
            write_file(
                &location,
                &Some(WorkspacePath::parse("notes").unwrap()),
                &node,
                b"n"
            )
            .unwrap_err()
            .code,
            "override_conflict"
        );
        remove_file(&location, &note).unwrap();
        assert!(!target.join("notes/n").exists());
        assert_eq!(
            remove_file(&location, &Some(WorkspacePath::parse("notes").unwrap()))
                .unwrap_err()
                .code,
            "override_conflict"
        );

        // A single file, then nothing.
        let single = Tree::File(FileNode {
            mode: 0o400,
            size: 1,
            fingerprint: key.fingerprint(b"x"),
        });
        write_tree(
            &location,
            &single,
            &|_, _| Ok(Zeroizing::new(b"x".to_vec())),
            &mut || Ok(()),
            Previous::Remove,
        )
        .unwrap();
        assert_eq!(mode(&target), 0o400);
        write_tree(
            &location,
            &Tree::Absent,
            &|_, _| unreachable!(),
            &mut || Ok(()),
            Previous::Remove,
        )
        .unwrap();
        assert!(!target.exists());
        // A crash's leftovers beside the path are cleaned up, and never read.
        std::fs::create_dir_all(home.path().join(".config/.elsecall-0123456789abcdef")).unwrap();
        write_tree(
            &location,
            &Tree::Absent,
            &|_, _| unreachable!(),
            &mut || Ok(()),
            Previous::Remove,
        )
        .unwrap();
        assert!(
            !home
                .path()
                .join(".config/.elsecall-0123456789abcdef")
                .exists()
        );
        assert!(is_temporary(b".elsecall-0123456789abcdef"));
        assert!(!is_temporary(b".elsecall-kept-0123"));
    }

    #[test]
    fn refuses_directories_that_resolve_to_forbidden_places() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
        std::os::unix::fs::symlink(home.path().join(".ssh"), home.path().join(".config")).unwrap();
        let place = Home::new(home.path(), &[home.path().join("work/copies")]);
        let path = SyncedPath::parse("~/.config/demo").unwrap();
        assert_eq!(
            place.locate(&path, true).err().unwrap().code,
            "unsafe_sync_path"
        );
        let workspace = SyncedPath::parse("~/work").unwrap();
        assert_eq!(
            place.locate(&workspace, true).err().unwrap().code,
            "unsafe_sync_path"
        );
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), home.path().join("away")).unwrap();
        let away = SyncedPath::parse("~/away/demo").unwrap();
        assert_eq!(
            place.locate(&away, true).err().unwrap().code,
            "unsafe_sync_path"
        );
        // A link to an ordinary directory inside home is followed.
        std::fs::create_dir_all(home.path().join("dotfiles/config")).unwrap();
        std::os::unix::fs::symlink(
            home.path().join("dotfiles/config"),
            home.path().join(".cfg"),
        )
        .unwrap();
        let linked = SyncedPath::parse("~/.cfg/demo").unwrap();
        assert!(place.locate(&linked, true).unwrap().is_some());
    }
}
