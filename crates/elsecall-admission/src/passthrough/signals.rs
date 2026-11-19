//! Forwards the caller's interrupt and termination signals to a local command,
//! and ends Elsecall the way the command ended.
use std::{
    ffi::c_int,
    sync::atomic::{AtomicI32, AtomicU32, Ordering},
};

/// Signals a caller uses to stop or poke a command.
const FORWARDED: [c_int; 6] = [
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
];

/// The command's PID while it runs; 0 before it starts, -1 once it was reaped.
static COMMAND: AtomicI32 = AtomicI32::new(0);
/// Signals that arrived before the command started, one bit per signal number.
static PENDING: AtomicU32 = AtomicU32::new(0);
/// Signals this process forwards, so only those are restored afterwards.
static INSTALLED: AtomicU32 = AtomicU32::new(0);

fn bit(signal: c_int) -> u32 {
    1 << signal
}

/// Sends every pending signal to the command, each at most once: whichever of
/// the handler and `target` swaps the bits out delivers them.
fn deliver_pending(command: i32) {
    let pending = PENDING.swap(0, Ordering::SeqCst);
    for signal in FORWARDED {
        if pending & bit(signal) != 0 {
            // SAFETY: kill is async-signal-safe.
            unsafe { libc::kill(command, signal) };
        }
    }
}

extern "C" fn forward(signal: c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // SAFETY: errno belongs to the interrupted thread; it is restored below.
    let saved = unsafe { *libc::__error() };
    // SAFETY: the kernel passes a valid siginfo with SA_SIGINFO.
    let sender = unsafe { (*info).si_pid() };
    match COMMAND.load(Ordering::SeqCst) {
        // A signal the command sent to its own process group already reached it.
        command if command > 0 && sender == command => (),
        command if command > 0 => {
            // SAFETY: kill is async-signal-safe.
            unsafe { libc::kill(command, signal) };
        }
        0 => {
            PENDING.fetch_or(bit(signal), Ordering::SeqCst);
            // The command may have started meanwhile, after `target` drained.
            let command = COMMAND.load(Ordering::SeqCst);
            if command > 0 {
                deliver_pending(command);
            }
        }
        _ => (),
    }
    // SAFETY: as above.
    unsafe { *libc::__error() = saved };
}

/// Starts forwarding. Signals the caller ignores stay ignored, so the command
/// inherits that, as it would when run directly.
pub fn install() {
    for signal in FORWARDED {
        // SAFETY: sigaction is called with valid pointers; the handler only uses
        // atomics and async-signal-safe calls.
        unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            libc::sigaction(signal, std::ptr::null(), &mut current);
            if current.sa_sigaction == libc::SIG_IGN {
                continue;
            }
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = forward as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
        INSTALLED.fetch_or(bit(signal), Ordering::SeqCst);
    }
}

/// Directs forwarded signals at the started command, including any that arrived
/// while it was starting.
pub fn target(pid: u32) {
    COMMAND.store(pid as i32, Ordering::SeqCst);
    deliver_pending(pid as i32);
}

/// The command was reaped: stop forwarding, so a later signal never reaches a
/// process that reused its PID, and acts on Elsecall as it normally would.
pub fn finished() {
    COMMAND.store(-1, Ordering::SeqCst);
    let installed = INSTALLED.swap(0, Ordering::SeqCst);
    for signal in FORWARDED {
        if installed & bit(signal) != 0 {
            // SAFETY: restoring the default action is a plain syscall.
            unsafe { libc::signal(signal, libc::SIG_DFL) };
        }
    }
}

/// Signals whose default action dumps core; re-raising them would make Elsecall
/// itself look like it crashed, so those end with 128 + signal instead.
fn dumps_core(signal: c_int) -> bool {
    matches!(
        signal,
        libc::SIGQUIT
            | libc::SIGILL
            | libc::SIGTRAP
            | libc::SIGABRT
            | libc::SIGEMT
            | libc::SIGFPE
            | libc::SIGBUS
            | libc::SIGSEGV
            | libc::SIGSYS
    )
}

/// Ends this process as the command ended: killed by the same signal, so the
/// caller's shell sees the same status.
pub fn exit_by(signal: c_int) -> ! {
    if !dumps_core(signal) {
        // SAFETY: restoring the default action and raising are plain syscalls.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, signal);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::raise(signal);
        }
    }
    std::process::exit(128 + signal);
}
