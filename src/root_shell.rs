//! The root shell `elsecall tools privileges` runs its scripts in: sudo, with an
//! administrator's password typed on the terminal or piped in.
use elsecall_contract::failure::Result;
use elsecall_macos::macos::sudo::{self, Password};
use std::io::IsTerminal;

pub struct Sudo;

impl elsecall_tools::privileges_cli::RootShell for Sudo {
    fn run(&self, script: &str) -> Result<()> {
        let stdin = std::io::stdin();
        let password = if stdin.is_terminal() {
            Password::Terminal {
                why: "sudo asks for an administrator's password to let this account place, select and license Xcode.",
            }
        } else {
            Password::Given(crate::piped_password::read(&mut stdin.lock(), false)?)
        };
        sudo::run(script, password).map(drop)
    }
}
