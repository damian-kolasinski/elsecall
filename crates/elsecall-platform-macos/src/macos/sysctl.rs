//! Reads integer kernel values by name.
use std::ffi::CStr;

/// Reads a 32- or 64-bit integer sysctl. `None` when the name does not exist on
/// this machine or has another type.
pub fn integer(name: &CStr) -> Option<i64> {
    let mut value = [0u8; 8];
    let mut size = value.len();
    // SAFETY: the name is NUL-terminated, the buffer and its size are valid and
    // writable, and no new value is set.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    match (status, size) {
        (0, 4) => Some(i32::from_ne_bytes(value[..4].try_into().unwrap()).into()),
        (0, 8) => Some(i64::from_ne_bytes(value)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_known_and_rejects_unknown_names() {
        assert!(super::integer(c"hw.logicalcpu").is_some_and(|cpus| cpus > 0));
        assert_eq!(super::integer(c"hw.elsecall_missing"), None);
    }
}
