//! The installed files and service: installing, restarting and confirming a build,
//! the lock every lifecycle operation holds, and the ports through which other
//! slices take part in an uninstall.
pub mod installation;
pub mod lifecycle_holder;
pub mod window_release;
