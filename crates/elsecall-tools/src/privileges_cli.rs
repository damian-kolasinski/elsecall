//! `elsecall tools privileges [install|remove]`: whether this account can take
//! Xcode's root steps, and the one-time step a person takes to let it.
//!
//! Installing and removing run a root shell script through [`RootShell`], the
//! port the binary composes with sudo; sudo asks a person for an administrator's
//! password on the terminal, or reads one piped in. Reading the status needs
//! no password.
use crate::xcode_privileges::{self, Status};
use elsecall_contract::failure::Result;
use serde_json::Value;

/// Runs a shell script as root, once an administrator's password allows it.
pub trait RootShell: Sync {
    fn run(&self, script: &str) -> Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::Subcommand)]
pub(crate) enum PrivilegesCommand {
    /// Install Elsecall's Xcode helper and a sudoers rule that lets this account
    /// run it, and nothing else, without a password. Asks for an administrator's
    /// password
    Install,
    /// Remove the helper and the rule. Xcodes they placed stay
    Remove,
}

pub(crate) async fn run(command: Option<PrivilegesCommand>, root: &dyn RootShell) -> Result<Value> {
    match command {
        None => {}
        Some(PrivilegesCommand::Install) => {
            let account = blocking(xcode_privileges::status).await?.account;
            root.run(&xcode_privileges::install_script(&account)?)?;
        }
        Some(PrivilegesCommand::Remove) => root.run(&xcode_privileges::remove_script())?,
    }
    let status: Status = blocking(xcode_privileges::status).await?;
    Ok(serde_json::to_value(status).expect("a status serializes"))
}

async fn blocking<T: Send + 'static>(
    read: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(read).await.map_err(|_| {
        elsecall_contract::failure::Failure::new(
            "internal_error",
            "Reading this account's privileges stopped unexpectedly.",
            "Run the command again.",
        )
    })?
}
