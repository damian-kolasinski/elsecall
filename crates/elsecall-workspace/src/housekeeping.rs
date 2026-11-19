//! Housekeeping of the workspace copies other members' placed runs leave on this
//! member, of the object stores they share, and of a caller's own sync state.
//!
//! Copies stay warm between runs, so they grow with their ignored build state. A
//! pass removes copies whose workspace the calling member deleted, as that member
//! alone reports, copies not used for the idle period and, while a volume's free
//! space is below its threshold, the least recently used idle copies; it removes
//! object stores no copy uses, and the caller-side state of deleted workspaces and
//! removed copies. A copy is never removed while a run uses it or one
//! is about to attach, and removing a copy is serialized with syncing to it (see
//! `workspace::copies`). Every removal is recorded with its reason.
pub mod adoption;
pub mod caller_state;
pub mod callers;
pub mod catalog;
pub mod clock;
pub mod exchange;
pub mod measure;
pub mod pass;
pub mod policy;
pub mod report;
pub mod schedule;
pub mod settings;
pub mod stores;
pub mod trash;
pub mod volumes;
