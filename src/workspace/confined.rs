//! File operations confined to one directory tree. Every path is walked component
//! by component from an open handle on the root, refusing symbolic links, so no
//! operation reaches outside the tree, even through a link planted inside it.
use crate::workspace::path::WorkspacePath;
use std::{
    collections::HashMap,
    ffi::{CStr, CString},
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
    sync::{Arc, Mutex},
};

/// How a walk treats missing or non-directory components.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Create {
    /// Missing directories are an absence.
    No,
    /// Missing directories are created; anything else in the way is refused.
    Directories,
    /// Missing directories are created, and files or links in the way removed. Only
    /// a member's own copy uses this, where the caller decides what exists.
    ReplacingOthers,
}

pub struct Confined {
    root: Arc<OwnedFd>,
    directories: Mutex<HashMap<String, Arc<OwnedFd>>>,
}

/// What a confined path holds, from `lstat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub kind: Kind,
    pub mode: u32,
    pub size: u64,
    pub inode: u64,
    pub modified_ns: i64,
    pub changed_ns: i64,
}

fn name(component: &str) -> io::Result<CString> {
    CString::new(component).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn check(result: libc::c_int) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn open_at(
    directory: RawFd,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> io::Result<OwnedFd> {
    loop {
        // SAFETY: `name` is a valid C string and `directory` an open descriptor.
        let fd = unsafe { libc::openat(directory, name.as_ptr(), flags | libc::O_CLOEXEC, mode) };
        if fd >= 0 {
            // SAFETY: openat returned a new descriptor that nothing else owns.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub fn stat_at(directory: RawFd, name: &CStr) -> io::Result<Option<Status>> {
    // SAFETY: an all-zero stat is a valid output buffer.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor, C string and output buffer.
    let result = unsafe {
        libc::fstatat(
            directory,
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT) | Some(libc::ENOTDIR) => Ok(None),
            _ => Err(error),
        };
    }
    Ok(Some(status(&stat)))
}

pub fn status(stat: &libc::stat) -> Status {
    let kind = match stat.st_mode & libc::S_IFMT {
        libc::S_IFREG => Kind::File,
        libc::S_IFDIR => Kind::Directory,
        libc::S_IFLNK => Kind::Symlink,
        _ => Kind::Other,
    };
    Status {
        kind,
        mode: (stat.st_mode as u32) & 0o7777,
        size: stat.st_size as u64,
        inode: stat.st_ino,
        modified_ns: stat.st_mtime * 1_000_000_000 + stat.st_mtime_nsec,
        changed_ns: stat.st_ctime * 1_000_000_000 + stat.st_ctime_nsec,
    }
}

pub fn fstat(file: &impl AsRawFd) -> io::Result<Status> {
    // SAFETY: as in `stat_at`.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor and output buffer.
    check(unsafe { libc::fstat(file.as_raw_fd(), &mut stat) })?;
    Ok(status(&stat))
}

/// The names in a directory, without `.` and `..`.
fn entries(directory: &OwnedFd) -> io::Result<Vec<CString>> {
    // SAFETY: dup of an open descriptor; fdopendir takes ownership of the copy.
    let copy = unsafe { libc::dup(directory.as_raw_fd()) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `copy` is an open directory descriptor this function owns.
    let stream = unsafe { libc::fdopendir(copy) };
    if stream.is_null() {
        // SAFETY: fdopendir failed, so the copy is still ours to close.
        unsafe { libc::close(copy) };
        return Err(io::Error::last_os_error());
    }
    // SAFETY: rewind a stream we own, so a reused descriptor lists from the start.
    unsafe { libc::rewinddir(stream) };
    let mut names = Vec::new();
    loop {
        // SAFETY: `stream` is a valid open DIR.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: readdir returned a valid entry whose name is NUL-terminated.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    }
    // SAFETY: closes the stream and the copied descriptor.
    unsafe { libc::closedir(stream) };
    Ok(names)
}

/// Removes `name` in `directory`, whatever it is; directories are removed with
/// everything in them, never following links.
pub fn remove_at(directory: &OwnedFd, name: &CStr) -> io::Result<()> {
    match stat_at(directory.as_raw_fd(), name)? {
        None => Ok(()),
        Some(Status {
            kind: Kind::Directory,
            ..
        }) => {
            let inner = open_at(
                directory.as_raw_fd(),
                name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            )?;
            // A read-only directory, as some tools make, must be writable to empty.
            if let Ok(status) = fstat(&inner)
                && status.mode & 0o700 != 0o700
            {
                // SAFETY: valid descriptor.
                unsafe { libc::fchmod(inner.as_raw_fd(), (status.mode | 0o700) as libc::mode_t) };
            }
            for entry in entries(&inner)? {
                remove_at(&inner, &entry)?;
            }
            // SAFETY: valid descriptor and C string.
            check(unsafe {
                libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR)
            })
        }
        Some(_) => {
            // SAFETY: valid descriptor and C string.
            check(unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) })
        }
    }
}

/// Directories whose handles stay open for reuse; a walk of a larger tree reopens
/// the rest, so huge trees never exhaust descriptors.
const CACHED_DIRECTORIES: usize = 256;

/// A fresh name for a temporary file, such as `.elsecall-3f9a0c41d2b7e811`.
pub fn temporary_name() -> io::Result<CString> {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|_| io::Error::other("no random numbers"))?;
    name(&format!(
        ".elsecall-{}",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ))
}

impl Confined {
    /// Opens `root`, which must be a directory and not itself a link.
    pub fn open(root: &Path) -> io::Result<Self> {
        let path = CString::new(root.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let root = open_at(
            libc::AT_FDCWD,
            &path,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )?;
        Ok(Self::from_directory(root))
    }

    fn from_directory(root: OwnedFd) -> Self {
        Self {
            root: Arc::new(root),
            directories: Mutex::new(HashMap::new()),
        }
    }

    /// The directory `path` inside this tree as a tree of its own, walked without
    /// following links; `None` if it is missing (and not created) or blocked.
    pub fn subtree(&self, path: &WorkspacePath, create: Create) -> io::Result<Option<Confined>> {
        Ok(match self.directory(Some(path), create)? {
            Some(directory) => Some(Self::from_directory(directory.try_clone()?)),
            None => None,
        })
    }

    /// Opens the directory named `component` in this tree's root, which may be a
    /// name a workspace path never uses, such as `.git`. `None` if it is missing,
    /// not a directory or a link.
    pub fn child(&self, component: &str) -> io::Result<Option<Confined>> {
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
        match open_at(self.root.as_raw_fd(), &name(component)?, flags, 0) {
            Ok(fd) => Ok(Some(Self::from_directory(fd))),
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::ENOENT | libc::ELOOP | libc::ENOTDIR)
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Replaces the file at `path` with `bytes`, atomically.
    pub fn write_file(
        &self,
        path: &WorkspacePath,
        bytes: &[u8],
        create: Create,
    ) -> io::Result<bool> {
        use std::io::Write;
        let Some((mut file, parent, temporary)) = self.temporary(path, create)? else {
            return Ok(false);
        };
        let written = file.write_all(bytes).and_then(|_| {
            // SAFETY: valid descriptor.
            check(unsafe { libc::fchmod(file.as_raw_fd(), 0o644) })
        });
        drop(file);
        match written.and_then(|_| self.place(&parent, &temporary, path, create)) {
            Ok(placed) => Ok(placed),
            Err(error) => {
                let _ = remove_at(&parent, &temporary);
                Err(error)
            }
        }
    }

    pub fn root(&self) -> &OwnedFd {
        &self.root
    }

    /// The directory at `path` (the root for `None`), walked without following
    /// links. `None` when it does not exist and `create` is `No`, or when a
    /// component is not a directory and may not be replaced.
    pub fn directory(
        &self,
        path: Option<&WorkspacePath>,
        create: Create,
    ) -> io::Result<Option<Arc<OwnedFd>>> {
        let Some(path) = path else {
            return Ok(Some(self.root.clone()));
        };
        if let Some(cached) = self
            .directories
            .lock()
            .expect("cache lock")
            .get(path.as_str())
        {
            return Ok(Some(cached.clone()));
        }
        let mut current = match path.parent() {
            Some(parent) => match self.directory(Some(&parent), create)? {
                Some(parent) => parent,
                None => return Ok(None),
            },
            None => self.root.clone(),
        };
        let component = name(path.file_name())?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
        let next = match open_at(current.as_raw_fd(), &component, flags, 0) {
            Ok(fd) => fd,
            Err(error) => {
                let missing = error.raw_os_error() == Some(libc::ENOENT);
                // O_NOFOLLOW on a link fails with ELOOP; a file gives ENOTDIR.
                let blocked = matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR));
                match (create, missing, blocked) {
                    (Create::No, true, _) | (Create::No, _, true) => return Ok(None),
                    (Create::Directories, _, true) => return Ok(None),
                    (Create::ReplacingOthers, _, true) => {
                        remove_at(&current, &component)?;
                        self.make(&current, &component)?
                    }
                    (_, true, _) => self.make(&current, &component)?,
                    _ => return Err(error),
                }
            }
        };
        current = Arc::new(next);
        let mut cache = self.directories.lock().expect("cache lock");
        if cache.len() >= CACHED_DIRECTORIES {
            cache.clear();
        }
        cache.insert(path.as_str().to_owned(), current.clone());
        Ok(Some(current))
    }

    fn make(&self, parent: &OwnedFd, component: &CStr) -> io::Result<OwnedFd> {
        // SAFETY: valid descriptor and C string.
        let made = unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o755) };
        if made < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EEXIST) {
                return Err(error);
            }
        }
        open_at(
            parent.as_raw_fd(),
            component,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
    }

    /// The parent directory of `path` and the C name of its last component.
    fn locate(
        &self,
        path: &WorkspacePath,
        create: Create,
    ) -> io::Result<Option<(Arc<OwnedFd>, CString)>> {
        let parent = self.directory(path.parent().as_ref(), create)?;
        Ok(match parent {
            Some(parent) => Some((parent, name(path.file_name())?)),
            None => None,
        })
    }

    /// What `path` holds; `None` if it, or a directory on its way, is missing or
    /// a link.
    pub fn stat(&self, path: &WorkspacePath) -> io::Result<Option<Status>> {
        match self.locate(path, Create::No)? {
            Some((parent, name)) => stat_at(parent.as_raw_fd(), &name),
            None => Ok(None),
        }
    }

    /// Whether a directory on the way to `path` is a link, so that `path` would
    /// resolve outside the tree if links were followed.
    pub fn link_on_the_way(&self, path: &WorkspacePath) -> bool {
        let mut prefix: Option<WorkspacePath> = None;
        let components: Vec<&str> = path.components().collect();
        for component in &components[..components.len() - 1] {
            let next = match &prefix {
                None => WorkspacePath::parse(component),
                Some(prefix) => prefix.join(component),
            };
            let Some(next) = next else { return false };
            match self.stat(&next) {
                Ok(Some(status)) if status.kind == Kind::Directory => prefix = Some(next),
                Ok(Some(status)) => return status.kind == Kind::Symlink,
                _ => return false,
            }
        }
        false
    }

    /// Opens a regular file for reading, never through a link.
    pub fn open_file(&self, path: &WorkspacePath) -> io::Result<Option<File>> {
        let Some((parent, name)) = self.locate(path, Create::No)? else {
            return Ok(None);
        };
        match open_at(
            parent.as_raw_fd(),
            &name,
            libc::O_RDONLY | libc::O_NOFOLLOW,
            0,
        ) {
            Ok(fd) => {
                let file = File::from(fd);
                Ok((fstat(&file)?.kind == Kind::File).then_some(file))
            }
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::ENOENT | libc::ELOOP | libc::ENOTDIR)
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    pub fn read_link(&self, path: &WorkspacePath) -> io::Result<Option<Vec<u8>>> {
        let Some((parent, name)) = self.locate(path, Create::No)? else {
            return Ok(None);
        };
        let mut buffer = vec![0u8; 4097];
        // SAFETY: valid descriptor, C string and buffer of the given length.
        let length = unsafe {
            libc::readlinkat(
                parent.as_raw_fd(),
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if length < 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ENOENT | libc::EINVAL | libc::ENOTDIR) => Ok(None),
                _ => Err(error),
            };
        }
        buffer.truncate(length as usize);
        Ok(Some(buffer))
    }

    /// Removes whatever is at `path`. Directories go with their content.
    pub fn remove(&self, path: &WorkspacePath) -> io::Result<()> {
        if let Some((parent, name)) = self.locate(path, Create::No)? {
            self.forget(path);
            remove_at(&parent, &name)?;
        }
        Ok(())
    }

    fn forget(&self, path: &WorkspacePath) {
        self.directories
            .lock()
            .expect("cache lock")
            .retain(|cached, _| {
                WorkspacePath::parse(cached).is_none_or(|cached| !cached.is_within(path))
            });
    }

    /// Creates a new private temporary file in the directory that will hold
    /// `path`, for writing before it is moved into place.
    pub fn temporary(
        &self,
        path: &WorkspacePath,
        create: Create,
    ) -> io::Result<Option<(File, Arc<OwnedFd>, CString)>> {
        let Some((parent, _)) = self.locate(path, create)? else {
            return Ok(None);
        };
        for _ in 0..16 {
            let temporary = temporary_name()?;
            match open_at(
                parent.as_raw_fd(),
                &temporary,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                0o600,
            ) {
                Ok(fd) => return Ok(Some((File::from(fd), parent, temporary))),
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("no temporary name was free"))
    }

    /// Moves `name` in `from` to `path`, replacing whatever is there.
    pub fn place(
        &self,
        from: &OwnedFd,
        name: &CStr,
        path: &WorkspacePath,
        create: Create,
    ) -> io::Result<bool> {
        let Some((parent, target)) = self.locate(path, create)? else {
            return Ok(false);
        };
        if let Some(Status {
            kind: Kind::Directory,
            ..
        }) = stat_at(parent.as_raw_fd(), &target)?
        {
            if create != Create::ReplacingOthers {
                return Ok(false);
            }
            self.forget(path);
            remove_at(&parent, &target)?;
        }
        // SAFETY: valid descriptors and C strings.
        check(unsafe {
            libc::renameat(
                from.as_raw_fd(),
                name.as_ptr(),
                parent.as_raw_fd(),
                target.as_ptr(),
            )
        })?;
        Ok(true)
    }

    /// Creates a symbolic link at `path`, replacing whatever is there.
    pub fn symlink(&self, path: &WorkspacePath, target: &str, create: Create) -> io::Result<bool> {
        let Some((parent, _)) = self.locate(path, create)? else {
            return Ok(false);
        };
        let target = name(target)?;
        let temporary = temporary_name()?;
        // SAFETY: valid C strings and descriptor.
        check(unsafe { libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), temporary.as_ptr()) })?;
        let parent_fd: &OwnedFd = &parent;
        self.place(parent_fd, &temporary, path, create)
            .inspect_err(|_| {
                // SAFETY: valid descriptor and C string.
                unsafe { libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0) };
            })
    }

    /// The directory `name` in `parent` as a tree of its own, never through a link.
    pub fn open_at(parent: &OwnedFd, name: &CStr) -> io::Result<Confined> {
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
        open_at(parent.as_raw_fd(), name, flags, 0).map(Self::from_directory)
    }

    /// Opens the regular file `name` in `parent` for reading, never through a link.
    pub fn open_child_file(parent: &OwnedFd, name: &CStr) -> io::Result<File> {
        let file = File::from(open_at(
            parent.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW,
            0,
        )?);
        if fstat(&file)?.kind != Kind::File {
            return Err(io::Error::other("not a regular file"));
        }
        Ok(file)
    }

    /// The directory that will hold `path`, created as needed, and its last name.
    /// Fails if a link or file is in the way.
    pub fn locate_for_writing(&self, path: &WorkspacePath) -> io::Result<(Arc<OwnedFd>, CString)> {
        self.locate(path, Create::Directories)?
            .ok_or_else(|| io::Error::other("a directory on the way is a link or a file"))
    }

    /// Creates the new private file `name` in `parent`, never through a link.
    pub fn create_at(parent: &OwnedFd, name: &CStr) -> io::Result<File> {
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW;
        open_at(parent.as_raw_fd(), name, flags, 0o600).map(File::from)
    }

    /// Puts `replacement` in `parent` at `name`, atomically. Whatever was at `name`
    /// ends up at `replacement`'s old name, for the caller to remove; `false` when
    /// nothing was there.
    pub fn swap_at(parent: &OwnedFd, replacement: &CStr, name: &CStr) -> io::Result<bool> {
        // SAFETY: valid descriptor and C strings.
        let swapped = unsafe {
            libc::renameatx_np(
                parent.as_raw_fd(),
                replacement.as_ptr(),
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::RENAME_SWAP,
            )
        };
        if swapped == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOENT) {
            return Err(error);
        }
        // SAFETY: valid descriptor and C strings.
        check(unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                replacement.as_ptr(),
                parent.as_raw_fd(),
                name.as_ptr(),
            )
        })?;
        Ok(false)
    }

    /// The names in the directory at `path` (the root for `None`); `None` if it is
    /// missing or blocked by a link.
    pub fn list(&self, path: Option<&WorkspacePath>) -> io::Result<Option<Vec<CString>>> {
        match self.directory(path, Create::No)? {
            Some(directory) => entries(&directory).map(Some),
            None => Ok(None),
        }
    }

    /// Sets the permission bits of the regular file at `path`.
    pub fn set_mode(&self, path: &WorkspacePath, mode: u32) -> io::Result<()> {
        let Some((parent, name)) = self.locate(path, Create::No)? else {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        };
        let file = File::from(open_at(
            parent.as_raw_fd(),
            &name,
            libc::O_RDONLY | libc::O_NOFOLLOW,
            0,
        )?);
        // SAFETY: valid descriptor.
        check(unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn path(text: &str) -> WorkspacePath {
        WorkspacePath::parse(text).unwrap()
    }

    #[test]
    fn never_follows_links_out_of_the_tree() {
        let outside = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        std::fs::write(outside.path().join("secret"), "s").unwrap();
        let confined = Confined::open(root.path()).unwrap();
        assert!(confined.stat(&path("escape/secret")).unwrap().is_none());
        assert!(
            confined
                .open_file(&path("escape/secret"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            confined.stat(&path("escape")).unwrap().unwrap().kind,
            Kind::Symlink
        );
        // A caller-side write refuses the link; a member's copy replaces it.
        assert!(
            confined
                .temporary(&path("escape/x"), Create::Directories)
                .unwrap()
                .is_none()
        );
        let (mut file, parent, name) = confined
            .temporary(&path("escape/x"), Create::ReplacingOthers)
            .unwrap()
            .unwrap();
        file.write_all(b"inside").unwrap();
        assert!(
            confined
                .place(&parent, &name, &path("escape/x"), Create::ReplacingOthers)
                .unwrap()
        );
        assert_eq!(
            std::fs::read(root.path().join("escape/x")).unwrap(),
            b"inside"
        );
        assert!(outside.path().join("secret").exists(), "outside untouched");
        assert!(!outside.path().join("x").exists());
        confined.remove(&path("escape")).unwrap();
        assert!(!root.path().join("escape").exists());
    }

    #[test]
    fn links_modes_and_recursive_removal() {
        let root = tempfile::tempdir().unwrap();
        let confined = Confined::open(root.path()).unwrap();
        assert!(
            confined
                .symlink(&path("a/link"), "../../etc", Create::ReplacingOthers)
                .unwrap()
        );
        assert_eq!(
            confined.read_link(&path("a/link")).unwrap().unwrap(),
            b"../../etc"
        );
        std::fs::write(root.path().join("a/f"), "x").unwrap();
        confined.set_mode(&path("a/f"), 0o755).unwrap();
        assert_eq!(confined.stat(&path("a/f")).unwrap().unwrap().mode, 0o755);
        std::fs::create_dir_all(root.path().join("a/deep/er")).unwrap();
        confined.remove(&path("a")).unwrap();
        assert!(!root.path().join("a").exists());
    }
}
