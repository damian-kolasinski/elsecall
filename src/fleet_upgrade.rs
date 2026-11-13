//! `elsecall fleet upgrade`: the master distributes its build to every member, and
//! each member verifies, installs and confirms it, rolling back on failure.
//! `protocol` is the frozen wire format, `master` the distributing side, `member`
//! the receiving side, `helper` the process that installs and restarts, and
//! `journal` the member's record that carries the outcome across the restart.
pub mod helper;
pub mod journal;
pub mod master;
pub mod member;
pub mod protocol;
