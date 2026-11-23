//! The file descriptor budget of the Elsecall service process.

use super::sysctl;

pub(crate) const SERVICE_LIMIT: u64 = 65_536;

fn limits() -> Option<libc::rlimit> {
    let mut limits = std::mem::MaybeUninit::uninit();
    // SAFETY: getrlimit receives writable storage for a single rlimit.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limits.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: a successful getrlimit initializes both fields.
    Some(unsafe { limits.assume_init() })
}

pub fn soft_limit() -> Option<u64> {
    limits().map(|limits| limits.rlim_cur.min(kernel_limit().unwrap_or(u64::MAX)))
}

fn kernel_limit() -> Option<u64> {
    sysctl::integer(c"kern.maxfilesperproc").and_then(|limit| u64::try_from(limit).ok())
}

/// An upgraded service can still be launched by the previous build's plist.
/// Give it the same budget as a fresh install, within the host's hard limits.
pub fn raise_service_limit() {
    let Some(mut limits) = limits() else { return };
    let kernel_limit = kernel_limit().unwrap_or(SERVICE_LIMIT);
    let wanted = SERVICE_LIMIT.min(limits.rlim_max).min(kernel_limit);
    if wanted > limits.rlim_cur {
        limits.rlim_cur = wanted;
        // SAFETY: limits holds the existing hard limit and an allowed soft limit.
        // A refusal leaves the effective limit available through health and doctor.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) };
    }
}
