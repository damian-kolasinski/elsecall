//! Running a caller's command on another member: the protocol both ends speak over
//! the run's secure stream, the link that carries it and notices silence, and the
//! member's side that executes the command. The caller's side is an executor of
//! `passthrough`, which keeps the caller-facing behaviour of `elsecall run`.
pub mod consistency;
pub mod environment;
pub mod fingerprint;
pub mod leftovers;
pub mod link;
mod output;
pub mod protocol;
pub mod runner;
