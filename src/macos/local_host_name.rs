//! This Mac's Bonjour name on the local network (System Settings > General >
//! Sharing > Local hostname), which other Macs see it by.
use std::ffi::{c_char, c_void};

#[link(name = "SystemConfiguration", kind = "framework")]
unsafe extern "C" {
    fn SCDynamicStoreCopyLocalHostName(store: *const c_void) -> *const c_void;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringGetCString(
        string: *const c_void,
        buffer: *mut c_char,
        size: isize,
        encoding: u32,
    ) -> u8;
    fn CFRelease(object: *const c_void);
}

const UTF8: u32 = 0x0800_0100;
/// A DNS label is at most 63 bytes.
const LIMIT: usize = 64;

/// The local host name without `.local`, such as `MacBook-Pro`; `None` when unset.
pub fn local_host_name() -> Option<String> {
    // SAFETY: a null store asks the dynamic store for the value directly; the
    // returned string is owned here and released once copied.
    unsafe {
        let name = SCDynamicStoreCopyLocalHostName(std::ptr::null());
        if name.is_null() {
            return None;
        }
        let mut buffer = [0 as c_char; LIMIT];
        let copied = CFStringGetCString(name, buffer.as_mut_ptr(), LIMIT as isize, UTF8) != 0;
        CFRelease(name);
        copied
            .then(|| {
                std::ffi::CStr::from_ptr(buffer.as_ptr())
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| !name.is_empty())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn this_mac_has_a_local_host_name_that_is_a_dns_label() {
        let name = super::local_host_name().expect("a Mac always has one");
        assert!(name.len() <= 63, "{name}");
        assert!(
            name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{name}"
        );
    }
}
