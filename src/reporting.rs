//! `elsecall report`: how the fleet did over a period, how accurate scheduling's
//! estimates were, and the most notable problems.
//!
//! - `window` is the period a report covers and how finely it is compared;
//! - `facts` projects a request record onto what a report needs;
//! - `requests`, `load` and `accuracy` compute the metrics (`backtest` estimates
//!   runs again for `accuracy`), `statistics` the
//!   summaries they share;
//! - `leftovers` sums what placed commands left running after they exited;
//! - `blocked` finds when outside pressure kept a member's gate closed, and
//!   what queued behind it, and `outside` the processes members named for it;
//! - `outside_warnings` lists members' early warnings about outside processes
//!   that spun or grew;
//! - `findings` picks the notable problems out of the metrics;
//! - `document` assembles the report from gathered facts and load;
//! - `gathering` asks every member for its history through the
//!   [`gathering::MemberHistory`] port and makes the report.
//!
//! Everything but `gathering` is pure. README "Fleet reports" defines every metric;
//! a change to a definition changes [`document::DEFINITIONS`].
pub mod accuracy;
pub mod backtest;
pub mod blocked;
pub mod document;
pub mod facts;
pub mod findings;
pub mod gathering;
pub mod leftovers;
pub mod load;
pub mod outside;
pub mod outside_warnings;
pub mod requests;
pub mod run_phases;
pub mod statistics;
pub mod window;

#[cfg(test)]
pub(crate) mod fixtures;

pub mod tuning;
