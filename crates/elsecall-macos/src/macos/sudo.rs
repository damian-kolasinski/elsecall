//! Running a root shell script on this Mac through sudo, for the one-time steps
//! that need an administrator.
//!
//! The script gets no standard input and an environment of its own, with system
//! directories only on its PATH, so nothing from the calling account's
//! environment reaches root.
use elsecall_contract::failure::{Failure, Result};
use std::{
    io::Write,
    process::{Command, Stdio},
};
use zeroize::Zeroizing;

/// The directories root looks commands up in.
const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// How sudo gets the administrator's password.
pub enum Password {
    /// sudo uses the credentials it cached, or a rule that needs none, and
    /// otherwise asks on the terminal, after `why` says what it is for.
    Terminal { why: &'static str },
    /// The password a program passed on standard input.
    Given(Zeroizing<String>),
}

/// sudo running `script` as root with `options`, its output captured.
fn sudo(options: &[&str], script: &str) -> Command {
    let mut command = Command::new("/usr/bin/sudo");
    command
        .args(options)
        .args(["--", "/usr/bin/env", "-i"])
        .arg(format!("PATH={SYSTEM_PATH}"))
        .args(["HOME=/var/root", "/bin/sh", "-c"])
        .arg(format!("exec </dev/null; {script}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn finished(ran: std::process::Output) -> Result<Vec<u8>> {
    if ran.status.success() {
        Ok(ran.stdout)
    } else {
        Err(refusal(&String::from_utf8_lossy(&ran.stderr)))
    }
}

/// Runs `script` as root and returns its standard output once it succeeds.
pub fn run(script: &str, password: Password) -> Result<Vec<u8>> {
    let spawn = |error| Failure::io("Run sudo", error);
    match password {
        Password::Terminal { why } => {
            // Without a prompt first, so nothing waits for typing when sudo
            // needs no password.
            let ran = sudo(&["-n"], script)
                .stdin(Stdio::null())
                .output()
                .map_err(spawn)?;
            if !String::from_utf8_lossy(&ran.stderr).contains("a password is required") {
                return finished(ran);
            }
            eprintln!("elsecall: {why}");
            // The person typing sees what sudo tells them, such as a wrong password.
            let ran = sudo(&[], script)
                .stdin(Stdio::inherit())
                .stderr(Stdio::inherit())
                .output()
                .map_err(spawn)?;
            if ran.status.success() {
                Ok(ran.stdout)
            } else {
                Err(Failure::new(
                    "sudo_failed",
                    "The administrator step did not finish.",
                    "sudo printed why above; run the command again with an administrator's password.",
                ))
            }
        }
        Password::Given(password) => {
            let mut child = sudo(&["-S", "-k", "-p", ""], script)
                .stdin(Stdio::piped())
                .spawn()
                .map_err(spawn)?;
            let mut input = Zeroizing::new(password.as_bytes().to_vec());
            input.push(b'\n');
            // sudo may exit before reading, as when the account cannot use it.
            let _ = child.stdin.take().expect("piped stdin").write_all(&input);
            finished(child.wait_with_output().map_err(spawn)?)
        }
    }
}

/// Why sudo or the script failed, from its error output, which never holds the
/// password.
fn refusal(stderr: &str) -> Failure {
    if stderr.contains("incorrect password") || stderr.contains("Sorry, try again") {
        Failure::new(
            "sudo_refused",
            "sudo did not accept the password.",
            "Use the password of an administrator account on this Mac.",
        )
    } else if stderr.contains("not in the sudoers") || stderr.contains("may not run sudo") {
        Failure::new(
            "not_administrator",
            "This account may not use sudo.",
            "Run the command from an administrator account, or have an administrator run Add Mac for this Mac from the master.",
        )
    } else if stderr.contains("a password is required") || stderr.contains("no tty present") {
        Failure::new(
            "password_required",
            "sudo needs an administrator's password.",
            "Run the command in a terminal, or pipe the password in on standard input.",
        )
    } else {
        let lines: Vec<&str> = stderr.trim().lines().collect();
        Failure::new(
            "sudo_failed",
            "The administrator step failed.",
            "Read details.stderr, fix the cause, then run the command again.",
        )
        .with_details(
            serde_json::json!({ "stderr": lines[lines.len().saturating_sub(5)..].join("\n") }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_are_told_apart() {
        assert_eq!(refusal("Sorry, try again.\n").code, "sudo_refused");
        assert_eq!(
            refusal("devbox is not in the sudoers file.\n").code,
            "not_administrator"
        );
        assert_eq!(
            refusal("sudo: a password is required\n").code,
            "password_required"
        );
        let failed = refusal("one\ntwo\nthree\nfour\nfive\nsix\n");
        assert_eq!(failed.code, "sudo_failed");
        assert_eq!(
            failed.details.unwrap()["stderr"],
            "two\nthree\nfour\nfive\nsix"
        );
    }
}
