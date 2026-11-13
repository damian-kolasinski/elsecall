//! Configuration and secret file sync: the master mirrors declared files and
//! directories to every member, which hold exact replicas so any of them can take
//! over the role, and lay their own overrides over them.
//!
//! - `synced_path`: which paths may be synced, and the rules that refuse others;
//! - `fingerprint`: keyed fingerprints, so output never reveals short secrets;
//! - `manifest`: the synced set as trees of fingerprints, validated on arrival;
//! - `store`: the replica store port and its owner-only file adapter;
//! - `disk`: reading and atomically replacing synced paths on this member;
//! - `compare`: overrides laid over mirrored trees, and per-file differences;
//! - `state` and `history`: what this member holds and records of what synced;
//! - `pins`: store content a push in progress still reads;
//! - `member_locks`: one exchange at a time per member;
//! - `replica`: issuing, applying, planning and status on this member;
//! - `report`: what plans and status show;
//! - `protocol`: the sealed channel between the master and a member;
//! - `service`: the running roles: the master's scans and pushes, a member's
//!   sessions and upkeep, plans, enabling, and the handover before a master move.
pub mod compare;
pub mod disk;
pub mod fingerprint;
pub mod history;
pub mod manifest;
pub mod member_locks;
pub mod pins;
pub mod protocol;
pub mod replica;
pub mod report;
pub mod service;
pub mod state;
pub mod store;
pub mod synced_path;

mod activity;
