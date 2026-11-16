//! Exact identities for cleanup; equal bytes at a reused pathname do not prove ownership.
use super::{conflict, directory::Directory, io};
use crate::{build_identity::Digest, failure::Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    ffi::{CStr, CString},
    fs::File,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Identity {
    pub device: i32,
    pub inode: u64,
    uid: u32,
    gid: u32,
    mode: u16,
    flags: u32,
    created: (i64, i64),
    pub node: Node,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Node {
    File {
        digest: Digest,
        attributes: Digest,
        modified: (i64, i64),
    },
    Directory {
        attributes: Digest,
    },
    Link {
        target: Vec<u8>,
        attributes: Digest,
        modified: (i64, i64),
    },
}
impl Identity {
    pub fn is_directory(&self) -> bool {
        matches!(self.node, Node::Directory { .. })
    }
}

pub(super) fn inspect(directory: &Directory, name: &CStr) -> Result<Option<Identity>> {
    let Some(stat) = directory.stat(name)? else {
        return Ok(None);
    };
    if stat.st_uid != crate::local_access::user_id() {
        return Err(conflict());
    }
    let modified = (stat.st_mtime, stat.st_mtime_nsec);
    let node = match stat.st_mode & libc::S_IFMT {
        libc::S_IFREG => {
            // Replacing a multiply-linked file would change the meaning of restoration.
            if stat.st_nlink != 1 || stat.st_size > 256 * 1024 * 1024 {
                return Err(conflict());
            }
            let mut file = directory.open_node(name, 0)?;
            let before = file.metadata().map_err(io)?;
            let digest = content_digest(&mut file, 256 * 1024 * 1024)?;
            let attributes = attributes(&file)?;
            let after = file.metadata().map_err(io)?;
            if before.ino() != stat.st_ino
                || before.dev() != stat.st_dev as u64
                || before.mtime() != after.mtime()
                || before.mtime_nsec() != after.mtime_nsec()
                || before.ctime() != after.ctime()
                || before.ctime_nsec() != after.ctime_nsec()
            {
                return Err(conflict());
            }
            Node::File {
                digest,
                attributes,
                modified,
            }
        }
        libc::S_IFDIR => {
            let file = directory.open_node(name, libc::O_DIRECTORY)?;
            let metadata = file.metadata().map_err(io)?;
            if metadata.ino() != stat.st_ino || metadata.dev() != stat.st_dev as u64 {
                return Err(conflict());
            }
            Node::Directory {
                attributes: attributes(&file)?,
            }
        }
        libc::S_IFLNK => {
            let mut target = vec![0u8; 4096];
            // SAFETY: writable buffer, valid name and descriptor.
            let count = unsafe {
                libc::readlinkat(
                    directory.0.as_raw_fd(),
                    name.as_ptr(),
                    target.as_mut_ptr().cast(),
                    target.len(),
                )
            };
            if count < 0 || count as usize == target.len() {
                return Err(conflict());
            }
            target.truncate(count as usize);
            let file = directory.open_node(name, libc::O_SYMLINK)?;
            let metadata = file.metadata().map_err(io)?;
            if metadata.ino() != stat.st_ino || metadata.dev() != stat.st_dev as u64 {
                return Err(conflict());
            }
            Node::Link {
                target,
                attributes: attributes(&file)?,
                modified,
            }
        }
        _ => return Err(conflict()),
    };
    let current = directory.stat(name)?.ok_or_else(conflict)?;
    if current.st_dev != stat.st_dev
        || current.st_ino != stat.st_ino
        || current.st_ctime != stat.st_ctime
        || current.st_ctime_nsec != stat.st_ctime_nsec
    {
        return Err(conflict());
    }
    Ok(Some(from_stat(stat, node)))
}

fn content_digest(reader: impl std::io::Read, limit: u64) -> Result<Digest> {
    let mut hash = Sha256::new();
    let count = std::io::copy(&mut reader.take(limit + 1), &mut hash).map_err(io)?;
    if count > limit {
        return Err(conflict());
    }
    Ok(Digest::of(&hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_growing_reader_is_bounded_even_when_it_never_reaches_eof() {
        struct Growing {
            total: usize,
        }
        impl std::io::Read for Growing {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.total += buffer.len();
                assert!(
                    self.total <= 1025,
                    "read exceeded the boundary plus one byte"
                );
                buffer.fill(42);
                Ok(buffer.len())
            }
        }
        let mut reader = Growing { total: 0 };
        assert!(content_digest(&mut reader, 1024).is_err());
        assert_eq!(reader.total, 1025);
        assert!(content_digest(&[42; 1024][..], 1024).is_ok());
    }
}
fn from_stat(stat: libc::stat, node: Node) -> Identity {
    Identity {
        device: stat.st_dev,
        inode: stat.st_ino,
        uid: stat.st_uid,
        gid: stat.st_gid,
        mode: stat.st_mode,
        flags: stat.st_flags,
        created: (stat.st_birthtime, stat.st_birthtime_nsec),
        node,
    }
}
pub(super) fn directory_identity(directory: &Directory) -> Result<Identity> {
    let mut stat = std::mem::MaybeUninit::uninit();
    // SAFETY: live descriptor and a writable stat buffer.
    if unsafe { libc::fstat(directory.0.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io(std::io::Error::last_os_error()));
    }
    // SAFETY: fstat initialized the buffer.
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR || stat.st_uid != crate::local_access::user_id()
    {
        return Err(conflict());
    }
    Ok(from_stat(
        stat,
        Node::Directory {
            attributes: attributes(&directory.0)?,
        },
    ))
}

fn attributes(file: &File) -> Result<Digest> {
    // ACLs are independent of extended attributes on macOS.
    unsafe extern "C" {
        fn acl_get_fd_np(fd: i32, kind: i32) -> *mut libc::c_void;
        fn acl_to_text(acl: *mut libc::c_void, length: *mut libc::ssize_t) -> *mut libc::c_char;
        fn acl_free(object: *mut libc::c_void) -> i32;
    }
    let mut hash = Sha256::new();
    // SAFETY: valid descriptor; returned allocations are checked and freed below.
    unsafe {
        let acl = acl_get_fd_np(file.as_raw_fd(), 0x100); // ACL_TYPE_EXTENDED
        if acl.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENOENT) {
                return Err(io(error));
            }
            hash.update(0u64.to_le_bytes());
        } else {
            let mut length = 0;
            let text = acl_to_text(acl, &mut length);
            if text.is_null() {
                acl_free(acl);
                return Err(io(std::io::Error::last_os_error()));
            }
            if !(0..=1024 * 1024).contains(&length) {
                acl_free(text.cast());
                acl_free(acl);
                return Err(conflict());
            }
            hash.update((length as u64).to_le_bytes());
            hash.update(std::slice::from_raw_parts(
                text.cast::<u8>(),
                length as usize,
            ));
            acl_free(text.cast());
            acl_free(acl);
        }
    }
    // SAFETY: null buffer requests the size of the attribute name list.
    let size = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0, 0) };
    if !(0..=1024 * 1024).contains(&size) {
        return Err(conflict());
    }
    let mut names = vec![0u8; size as usize];
    // SAFETY: sized writable name-list buffer.
    let count =
        unsafe { libc::flistxattr(file.as_raw_fd(), names.as_mut_ptr().cast(), names.len(), 0) };
    if count != size {
        return Err(conflict());
    }
    let mut names: Vec<_> = names.split(|b| *b == 0).filter(|n| !n.is_empty()).collect();
    names.sort_unstable();
    for name in names {
        let name = CString::new(name).map_err(|_| conflict())?;
        // SAFETY: null buffer requests this attribute's size.
        let size = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                0,
            )
        };
        if !(0..=8 * 1024 * 1024).contains(&size) {
            return Err(conflict());
        }
        let mut value = vec![0u8; size as usize];
        // SAFETY: sized writable attribute buffer.
        let count = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        if count != size {
            return Err(conflict());
        }
        hash.update((name.as_bytes().len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(&value);
    }
    Ok(Digest::of(&hash.finalize()))
}
