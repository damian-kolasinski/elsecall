//! The environment of placed work. The caller forwards only the variables its
//! consumer configuration allows; machine-identity variables always come from the
//! member, and Elsecall sets its own `ELSECALL_*` variables. Values exist only in
//! transit and in the running command: nothing here logs or stores them.
use crate::remote_run::protocol::Variable;
use elsecall_workload_config::workload_settings::WorkloadSettings;
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

/// The directories whose `mise.toml` mise reads without asking, separated by `:`.
/// A copy is new to the member, so mise would refuse its configuration, and the
/// tools the project pins with it, with a prompt nobody can answer. The command
/// already runs the caller's code in the copy, so trusting its configuration
/// grants nothing more.
const MISE_TRUSTED: &str = "MISE_TRUSTED_CONFIG_PATHS";

fn identity(name: &[u8]) -> bool {
    IDENTITY.iter().any(|i| i.as_bytes() == name)
}
fn elsecall(name: &[u8]) -> bool {
    name.starts_with(b"ELSECALL_")
}

/// The caller's variables its configuration forwards, in the caller's order.
pub fn forwarded(
    configuration: &WorkloadSettings,
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
    /// The root of the copy the command runs in: `ELSECALL_WORKSPACE_COPY`, and
    /// trusted by mise.
    pub copy: &'a OsStr,
}

fn set(environment: &mut Vec<(OsString, OsString)>, name: OsString, value: OsString) {
    environment.retain(|(existing, _)| *existing != name);
    environment.push((name, value));
}

/// `trusted` with `copy` appended, or unchanged when the copy's path holds the
/// separator and would be read as two paths.
fn trusting(trusted: Option<&OsStr>, copy: &OsStr) -> Option<OsString> {
    if copy.as_bytes().contains(&b':') {
        return trusted.map(OsStr::to_owned);
    }
    Some(match trusted {
        Some(paths) if !paths.is_empty() => {
            let mut joined = paths.to_owned();
            joined.push(":");
            joined.push(copy);
            joined
        }
        _ => copy.to_owned(),
    })
}

impl Placed<'_> {
    pub fn build(self) -> Vec<(OsString, OsString)> {
        let mut environment: Vec<(OsString, OsString)> = self
            .target
            .into_iter()
            .filter(|(name, _)| !elsecall(name.as_bytes()) && name != "PWD" && name != "OLDPWD")
            .collect();
        if let Some(path) = self.path {
            set(&mut environment, "PATH".into(), path.into());
        }
        for variable in self.forwarded {
            // The member enforces the rules too: the caller is not trusted to.
            if !identity(&variable.name) && !elsecall(&variable.name) {
                set(
                    &mut environment,
                    OsString::from_vec(variable.name.clone()),
                    OsString::from_vec(variable.value.clone()),
                );
            }
        }
        let trusted = environment
            .iter()
            .find(|(name, _)| name == MISE_TRUSTED)
            .map(|(_, value)| value.as_os_str());
        if let Some(trusted) = trusting(trusted, self.copy) {
            set(&mut environment, MISE_TRUSTED.into(), trusted);
        }
        set(
            &mut environment,
            "PWD".into(),
            self.working_directory.into(),
        );
        for (name, value) in self.elsecall {
            set(&mut environment, name.into(), value);
        }
        set(
            &mut environment,
            "ELSECALL_WORKSPACE_COPY".into(),
            self.copy.into(),
        );
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
        let configuration = WorkloadSettings::parse(
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
            copy: OsStr::new("/copy"),
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
        assert_eq!(get(MISE_TRUSTED).as_deref(), Some("/copy"));
        assert_eq!(get("ELSECALL_WORKSPACE_COPY").as_deref(), Some("/copy"));
    }

    fn trusted_with(target: &[(&str, &str)], forwarded: &[Variable], copy: &str) -> Option<String> {
        Placed {
            target: os(target),
            path: None,
            forwarded,
            elsecall: Vec::new(),
            working_directory: OsStr::new(copy),
            copy: OsStr::new(copy),
        }
        .build()
        .into_iter()
        .find(|(name, _)| name == MISE_TRUSTED)
        .map(|(_, value)| value.into_string().unwrap())
    }

    #[test]
    fn the_copy_is_appended_to_the_paths_mise_already_trusts() {
        assert_eq!(
            trusted_with(&[(MISE_TRUSTED, "/Users/devbox/work")], &[], "/copy").as_deref(),
            Some("/Users/devbox/work:/copy"),
            "the member's own trust stays"
        );
        assert_eq!(
            trusted_with(&[(MISE_TRUSTED, "")], &[], "/copy").as_deref(),
            Some("/copy")
        );
        let forwarded = [Variable {
            name: MISE_TRUSTED.as_bytes().to_vec(),
            value: b"/Users/me/acme".to_vec(),
        }];
        assert_eq!(
            trusted_with(&[(MISE_TRUSTED, "/Users/devbox/work")], &forwarded, "/copy").as_deref(),
            Some("/Users/me/acme:/copy"),
            "a forwarded value replaces the member's, as forwarding does"
        );
    }

    #[test]
    fn a_copy_whose_path_holds_the_separator_is_not_trusted() {
        assert_eq!(trusted_with(&[], &[], "/Users/me/a:b"), None);
        assert_eq!(
            trusted_with(
                &[(MISE_TRUSTED, "/Users/devbox/work")],
                &[],
                "/Users/me/a:b"
            )
            .as_deref(),
            Some("/Users/devbox/work")
        );
    }
}
