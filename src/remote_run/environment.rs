//! The environment of placed work. The caller forwards only the variables its
//! consumer configuration allows; machine-identity variables always come from the
//! member, and Elsecall sets its own `ELSECALL_*` variables. Values exist only in
//! transit and in the running command: nothing here logs or stores them.
use crate::{consumer_configuration::ConsumerConfiguration, remote_run::protocol::Variable};
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
};

/// Variables that describe the machine and account a command runs as. They always
/// come from the member, even when the consumer allows them.
pub const IDENTITY: [&str; 13] = [
    "HOME",
    "USER",
    "LOGNAME",
    "PATH",
    "TMPDIR",
    "SHELL",
    "PWD",
    "OLDPWD",
    "__CF_USER_TEXT_ENCODING",
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "XPC_FLAGS",
    "XPC_SERVICE_NAME",
];

fn identity(name: &[u8]) -> bool {
    IDENTITY.iter().any(|i| i.as_bytes() == name)
}
fn elsecall(name: &[u8]) -> bool {
    name.starts_with(b"ELSECALL_")
}

/// The caller's variables its configuration forwards, in the caller's order.
pub fn forwarded(
    configuration: &ConsumerConfiguration,
    variables: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<Variable> {
    variables
        .into_iter()
        .filter(|(name, _)| {
            let bytes = name.as_bytes();
            !identity(bytes)
                && !elsecall(bytes)
                && name.to_str().is_some_and(|n| configuration.forwards(n))
        })
        .map(|(name, value)| Variable {
            name: name.into_vec(),
            value: value.into_vec(),
        })
        .collect()
}

/// What placed work runs with on this member.
pub struct Placed<'a> {
    /// The member's own environment: its service's.
    pub target: Vec<(OsString, OsString)>,
    /// The member's configured PATH for placed work, if any.
    pub path: Option<&'a OsStr>,
    pub forwarded: &'a [Variable],
    /// `ELSECALL_*` variables and their values.
    pub elsecall: Vec<(&'static str, OsString)>,
    pub working_directory: &'a OsStr,
}

impl Placed<'_> {
    pub fn build(self) -> Vec<(OsString, OsString)> {
        let mut environment: Vec<(OsString, OsString)> = self
            .target
            .into_iter()
            .filter(|(name, _)| !elsecall(name.as_bytes()) && name != "PWD" && name != "OLDPWD")
            .collect();
        let mut set = |name: OsString, value: OsString| {
            environment.retain(|(existing, _)| *existing != name);
            environment.push((name, value));
        };
        if let Some(path) = self.path {
            set("PATH".into(), path.into());
        }
        for variable in self.forwarded {
            // The member enforces the rules too: the caller is not trusted to.
            if !identity(&variable.name) && !elsecall(&variable.name) {
                set(
                    OsString::from_vec(variable.name.clone()),
                    OsString::from_vec(variable.value.clone()),
                );
            }
        }
        set("PWD".into(), self.working_directory.into());
        for (name, value) in self.elsecall {
            set(name.into(), value);
        }
        environment
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(n, v)| ((*n).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn forwards_only_the_allowlist_and_keeps_the_members_identity() {
        let configuration = ConsumerConfiguration::parse(
            "[environment]\nnames = [\"CI\", \"HOME\"]\nprefixes = [\"APP_\"]\n",
        )
        .unwrap();
        let forwarded = forwarded(
            &configuration,
            os(&[
                ("CI", "1"),
                ("APP_TOKEN", "t"),
                ("SECRET", "s"),
                ("HOME", "/Users/caller"),
                ("ELSECALL_RUN_ID", "forged"),
            ]),
        );
        let names: Vec<&[u8]> = forwarded.iter().map(|v| v.name.as_slice()).collect();
        assert_eq!(names, [b"CI".as_slice(), b"APP_TOKEN"]);

        let environment = Placed {
            target: os(&[
                ("HOME", "/Users/devbox"),
                ("PATH", "/usr/bin"),
                ("ELSECALL_TAILNET_NAME", "x"),
                ("PWD", "/somewhere"),
            ]),
            path: Some(OsStr::new("/opt/homebrew/bin:/usr/bin")),
            forwarded: &[
                Variable {
                    name: b"CI".to_vec(),
                    value: b"1".to_vec(),
                },
                Variable {
                    name: b"HOME".to_vec(),
                    value: b"/Users/caller".to_vec(),
                },
            ],
            elsecall: vec![("ELSECALL_RUN_ID", "r-1".into())],
            working_directory: OsStr::new("/copy/src"),
        }
        .build();
        let get = |name: &str| {
            environment
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.to_str().unwrap().to_owned())
        };
        assert_eq!(get("HOME").as_deref(), Some("/Users/devbox"));
        assert_eq!(get("PATH").as_deref(), Some("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(get("CI").as_deref(), Some("1"));
        assert_eq!(get("PWD").as_deref(), Some("/copy/src"));
        assert_eq!(get("ELSECALL_RUN_ID").as_deref(), Some("r-1"));
        assert_eq!(get("ELSECALL_TAILNET_NAME"), None);
    }
}
