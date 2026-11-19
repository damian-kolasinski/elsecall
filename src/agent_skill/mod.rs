//! The skill coding agents use to learn Elsecall. The guide is embedded in the
//! binary, so it always matches the installed build; what `elsecall skill setup`
//! writes into an agent's skills folder only points at `elsecall skill show`.
mod agents;
pub mod cli;
mod folder;
mod guide;
