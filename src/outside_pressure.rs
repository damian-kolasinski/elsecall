//! Outside pressure: when something Elsecall did not start holds this member's
//! admission gate closed, say so, since when, and name the processes behind it;
//! and warn early about outside processes that have spun or grown for hours
//! before they close it.
//!
//! - `sampling` is the port to the machine's processes, and the rates between
//!   two samples;
//! - `attribution` tells outside processes from Elsecall's own work;
//! - `episode` decides when a closed gate becomes a blocked member and when it
//!   stops being one;
//! - `store` keeps blocked episodes for reports;
//! - `detector` runs them against the gate each second;
//! - `remedy` says what the operator can do about it;
//! - `warning` is an early warning about one outside process;
//! - `trend` decides from periodic rounds which processes warrant one;
//! - `trend_store` keeps the rounds and the warnings;
//! - `watcher` takes the rounds and raises and ends warnings.
//!
//! Elsecall never stops an outside process: it names the process and the remedy,
//! and the operator acts. Processes are recorded by name only, never with their
//! arguments, environment or paths.
pub mod attribution;
pub mod blocked;
pub mod detector;
pub mod episode;
pub mod remedy;
pub mod sampling;
pub mod store;
pub mod trend;
pub mod trend_store;
pub mod warning;
pub mod watcher;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod warning_tests;

pub use blocked::{Blocked, OutsideProcess, Reason};
pub use detector::{BlockedReport, Detector, GateSource, Observation};
pub use sampling::ProcessSampler;
pub use warning::Warning;
pub use watcher::{WarningReport, Watcher};
