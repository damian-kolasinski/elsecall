//! Content gathered for a sync before the copy's tree changes: blobs in a private
//! directory inside the copy's git directory, named by order and kept by digest,
//! then moved into place in one quick pass.
use crate::{
    failure::Result,
    workspace::{
        confined::{Confined, remove_at, stat_at, temporary_name},
        digest::{Digest, Hasher},
        path::WorkspacePath,
        sync_failed,
    },
};
use std::{
    collections::HashMap,
    ffi::CString,
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

pub struct Staging {
    confined: Confined,
    blobs: HashMap<Digest, CString>,
    next: u64,
}

fn failed(action: &str) -> impl Fn(io::Error) -> crate::failure::Failure + '_ {
    move |e| sync_failed(format!("{action} on the member failed: {e}."))
}

impl Staging {
    /// Stages in `directory`, which must be empty and private.
    pub fn new(directory: Confined) -> Self {
        Self {
            confined: directory,
            blobs: HashMap::new(),
            next: 0,
        }
    }

    pub fn directory(&self) -> &OwnedFd {
        self.confined.root()
    }

    fn new_name(&mut self) -> CString {
        self.next += 1;
        CString::new(format!("s{}", self.next)).expect("no NUL")
    }

    pub fn has(&self, digest: &Digest) -> bool {
        self.blobs.contains_key(digest)
    }

    /// Writes content from `reader`, keeping it only if its digest is `expected`.
    /// Returns whether it was kept.
    pub fn write_from(&mut self, reader: &mut impl Read, expected: Digest) -> Result<bool> {
        let name = self.new_name();
        let mut file = self.create(&name)?;
        let mut hasher = Hasher::default();
        let mut buffer = vec![0u8; 256 * 1024];
        loop {
            let n = reader
                .read(&mut buffer)
                .map_err(failed("Staging content"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            file.write_all(&buffer[..n])
                .map_err(failed("Staging content"))?;
        }
        if hasher.finish() != expected {
            self.unlink(&name);
            return Ok(false);
        }
        self.blobs.insert(expected, name);
        Ok(true)
    }

    /// Starts a blob that arrives in chunks and may be at most `limit` bytes.
    pub fn begin(&mut self, limit: u64) -> Result<Incoming> {
        let name = self.new_name();
        let file = self.create(&name)?;
        Ok(Incoming {
            name,
            file,
            hasher: Hasher::default(),
            size: 0,
            limit,
        })
    }

    /// Keeps a received blob if it has the digest that was asked for.
    pub fn finish(&mut self, incoming: Incoming, expected: Digest) -> Result<bool> {
        let digest = incoming.hasher.finish();
        drop(incoming.file);
        if digest != expected {
            self.unlink(&incoming.name);
            return Ok(false);
        }
        self.blobs.insert(digest, incoming.name);
        Ok(true)
    }

    /// Stages a copy of `file`, by cloning it where the file system allows.
    pub fn clone_in(&mut self, file: &File, digest: Digest) -> Result<()> {
        let name = self.new_name();
        self.clone_to(file, &name)?;
        self.blobs.insert(digest, name);
        Ok(())
    }

    fn clone_to(&self, source: &File, name: &CString) -> Result<()> {
        // SAFETY: valid descriptors and C string.
        let cloned = unsafe {
            libc::fclonefileat(
                source.as_raw_fd(),
                self.directory().as_raw_fd(),
                name.as_ptr(),
                0,
            )
        };
        if cloned < 0 {
            let mut target = self.create(name)?;
            io::copy(&mut &*source, &mut target).map_err(failed("Copying content"))?;
        }
        Ok(())
    }

    fn create(&self, name: &CString) -> Result<File> {
        // SAFETY: valid descriptor and C string.
        let fd = unsafe {
            libc::openat(
                self.directory().as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(failed("Staging content")(io::Error::last_os_error()));
        }
        // SAFETY: a new descriptor nothing else owns.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn unlink(&self, name: &CString) {
        let _ = remove_at(self.directory(), name);
    }

    /// The staged name holding `digest`, with `mode`, to be moved into place.
    /// Unless this is its last use, a clone is made so the content stays staged.
    pub fn take(&self, digest: &Digest, mode: u32, last: bool) -> Result<CString> {
        let name = self
            .blobs
            .get(digest)
            .ok_or_else(|| sync_failed("Content the copy needs was not received."))?;
        let name = if last {
            name.clone()
        } else {
            let source = self
                .confined
                .open_file(&WorkspacePath::parse(name.to_str().expect("ASCII")).expect("valid"))
                .map_err(failed("Staging content"))?
                .ok_or_else(|| sync_failed("Staged content vanished."))?;
            let copy = temporary_name().map_err(failed("Staging content"))?;
            self.clone_to(&source, &copy)?;
            copy
        };
        if stat_at(self.directory().as_raw_fd(), &name)
            .map_err(failed("Staging content"))?
            .is_none()
        {
            return Err(sync_failed("Staged content vanished."));
        }
        // SAFETY: valid descriptor and C string; staged files are never links.
        let result = unsafe {
            libc::fchmodat(
                self.directory().as_raw_fd(),
                name.as_ptr(),
                mode as libc::mode_t,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            return Err(failed("Setting a mode")(io::Error::last_os_error()));
        }
        Ok(name)
    }
}

/// A blob arriving in chunks from the caller.
pub struct Incoming {
    name: CString,
    file: File,
    hasher: Hasher,
    pub size: u64,
    limit: u64,
}
impl Incoming {
    pub fn write(&mut self, chunk: &[u8]) -> Result<()> {
        self.size += chunk.len() as u64;
        if self.size > self.limit {
            return Err(sync_failed(
                "Content arrived larger than the caller listed it.",
            ));
        }
        self.hasher.update(chunk);
        self.file
            .write_all(chunk)
            .map_err(failed("Staging content"))
    }
}
