//! Descriptor-relative filesystem operations. No operation follows a symlink.
use super::{RelativePath, conflict, io};
use crate::failure::Result;
use std::{
    ffi::{CStr, CString},
    fs::{File, OpenOptions},
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

pub(super) struct Directory(pub File);
/// A successful unlink and its separate durability result. A failed sync must
/// never be interpreted as an object that still needs to be rolled back.
pub(super) struct Removal {
    pub durability: Result<()>,
}
impl Directory {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(io)?;
        Self::owned(file)
    }
    fn owned(file: File) -> Result<Self> {
        let meta = file.metadata().map_err(io)?;
        if !meta.is_dir()
            || meta.uid() != crate::local_access::user_id()
            || meta.mode() & 0o022 != 0
        {
            return Err(conflict());
        }
        Ok(Self(file))
    }
    pub fn parent(&self, path: &RelativePath) -> Result<(Self, CString)> {
        let mut directory = Self(self.0.try_clone().map_err(io)?);
        let components: Vec<_> = path.as_str().split('/').collect();
        for component in &components[..components.len() - 1] {
            let name = CString::new(*component).expect("validated path");
            let file = directory.open_node(&name, libc::O_DIRECTORY)?;
            directory = Self::owned(file)?;
        }
        Ok((
            directory,
            CString::new(*components.last().unwrap()).expect("validated path"),
        ))
    }
    pub fn open_node(&self, name: &CStr, flags: i32) -> Result<File> {
        // On Darwin O_NOFOLLOW refuses even O_SYMLINK. O_SYMLINK itself opens
        // the link inode, so both cases remain no-follow operations.
        let nofollow = if flags & libc::O_SYMLINK != 0 {
            0
        } else {
            libc::O_NOFOLLOW
        };
        // SAFETY: the descriptor and NUL-terminated name stay live for the call.
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | nofollow | libc::O_NONBLOCK | libc::O_CLOEXEC | flags,
            )
        };
        if fd < 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        // SAFETY: openat returned an owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    pub fn create_file(&self, name: &CStr, bytes: &[u8], mode: u16) -> Result<File> {
        // SAFETY: valid descriptor/name and a mode argument for O_CREAT.
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        // SAFETY: openat returned a fresh owned descriptor.
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(bytes).map_err(io)?;
        // umask must not change the intended installed mode.
        // SAFETY: live descriptor and an ordinary permission mode.
        if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        file.sync_all().map_err(io)?;
        self.sync()?;
        Ok(file)
    }
    pub fn mkdir(&self, name: &CStr) -> Result<()> {
        // SAFETY: live descriptor and NUL-terminated name.
        if unsafe { libc::mkdirat(self.0.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        self.sync()
    }
    pub fn symlink(&self, name: &CStr, target: &CStr) -> Result<()> {
        // SAFETY: live descriptor and NUL-terminated names.
        if unsafe { libc::symlinkat(target.as_ptr(), self.0.as_raw_fd(), name.as_ptr()) } != 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        self.sync()
    }
    pub fn names(&self) -> Result<Vec<Vec<u8>>> {
        // SAFETY: dup makes a descriptor owned by fdopendir, closed by closedir.
        let descriptor = unsafe { libc::dup(self.0.as_raw_fd()) };
        if descriptor < 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        // SAFETY: descriptor is a freshly duplicated directory descriptor.
        let stream = unsafe { libc::fdopendir(descriptor) };
        if stream.is_null() {
            // SAFETY: fdopendir did not consume a descriptor on failure.
            unsafe {
                libc::close(descriptor);
            }
            return Err(io(std::io::Error::last_os_error()));
        }
        let mut names = Vec::new();
        // SAFETY: stream stays open throughout iteration; each name is copied.
        unsafe {
            libc::rewinddir(stream);
            loop {
                *libc::__error() = 0;
                let entry = libc::readdir(stream);
                if entry.is_null() {
                    let error = *libc::__error();
                    libc::closedir(stream);
                    if error != 0 {
                        return Err(io(std::io::Error::from_raw_os_error(error)));
                    }
                    break;
                }
                let name = CStr::from_ptr((*entry).d_name.as_ptr()).to_bytes();
                if name != b"." && name != b".." {
                    names.push(name.to_vec());
                }
            }
        }
        Ok(names)
    }
    pub fn stat(&self, name: &CStr) -> Result<Option<libc::stat>> {
        let mut stat = std::mem::MaybeUninit::uninit();
        // SAFETY: stat is an output buffer; the descriptor and name are valid.
        if unsafe {
            libc::fstatat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            // SAFETY: fstatat initialized the buffer.
            return Ok(Some(unsafe { stat.assume_init() }));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(io(error))
        }
    }
    pub fn rename(&self, name: &CStr, destination: &Self, target: &CStr, swap: bool) -> Result<()> {
        // Exclusive publication never overwrites an object that appeared meanwhile.
        // Swap retains the complete prior inode, including ACLs and extended attributes.
        let flags = if swap {
            libc::RENAME_SWAP
        } else {
            libc::RENAME_EXCL
        };
        // SAFETY: both descriptors and names remain live for the call.
        if unsafe {
            libc::renameatx_np(
                self.0.as_raw_fd(),
                name.as_ptr(),
                destination.0.as_raw_fd(),
                target.as_ptr(),
                flags,
            )
        } != 0
        {
            return Err(io(std::io::Error::last_os_error()));
        }
        self.sync()?;
        destination.sync()
    }
    pub fn remove(&self, name: &CStr, directory: bool) -> Result<Removal> {
        // SAFETY: descriptor and name are valid. AT_REMOVEDIR only removes empty directories.
        if unsafe {
            libc::unlinkat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                if directory { libc::AT_REMOVEDIR } else { 0 },
            )
        } != 0
        {
            return Err(io(std::io::Error::last_os_error()));
        }
        Ok(Removal {
            durability: self.sync(),
        })
    }
    pub fn sync(&self) -> Result<()> {
        self.0.sync_all().map_err(io)
    }
}
