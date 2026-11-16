//! POSIX modes alone do not make macOS restoration storage private.
use super::{conflict, io};
use crate::failure::Result;
use std::{fs::File, os::fd::AsRawFd};
unsafe extern "C" {
    fn acl_init(count: i32) -> *mut libc::c_void;
    fn acl_get_fd_np(fd: i32, kind: i32) -> *mut libc::c_void;
    fn acl_set_fd_np(fd: i32, acl: *mut libc::c_void, kind: i32) -> i32;
    fn acl_free(object: *mut libc::c_void) -> i32;
}
const EXTENDED: i32 = 0x100;
pub(super) fn clear(file: &File) -> Result<()> {
    // SAFETY: allocate an empty ACL, apply it to a live descriptor, and free it.
    unsafe {
        let acl = acl_init(0);
        if acl.is_null() {
            return Err(io(std::io::Error::last_os_error()));
        }
        let result = acl_set_fd_np(file.as_raw_fd(), acl, EXTENDED);
        acl_free(acl);
        if result != 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
    }
    file.sync_all().map_err(io)?;
    check(file)
}
pub(super) fn check(file: &File) -> Result<()> {
    // SAFETY: inspect a descriptor ACL, then release any allocation. Darwin
    // reports no extended ACL with a null result and ENOENT.
    unsafe {
        let acl = acl_get_fd_np(file.as_raw_fd(), EXTENDED);
        if acl.is_null() {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ENOENT) {
                Ok(())
            } else {
                Err(io(error))
            };
        }
        acl_free(acl);
        Err(conflict())
    }
}
