//! The options a mise pin carries to the installer.
//!
//! A mise tool table can hold options next to its version, and a tool's key can
//! hold them in brackets (`"ubi:cli/cli[exe=gh]"`). Most of them change how mise
//! behaves rather than which artifact it installs, and some run code:
//! `postinstall` is a shell command, and `install_env` sets the environment a
//! build runs in. Elsecall carries only the options that pick which published
//! artifact a version is and how mise verifies it, so a member installs the
//! build the pins describe, and nothing else:
//!
//! - which release and asset: `version_prefix`, `asset_pattern`, `matching`;
//! - where its executables are: `bin`, `bin_path`, `exe`, `rename_exe`,
//!   `strip_components`;
//! - who must have signed it: `slsa_signer_identity`, `slsa_signer_issuer`.
//!
//! Every other option is dropped, `postinstall` and `install_env` included,
//! and so are `api_url`, which would send the download to another server, and
//! `os`, which the reader already applied. A value is literal text, as a
//! version is: a template is an error, not something to render. The installer
//! passes options in mise's bracket syntax, so a value never holds what would
//! end one there (`,`, `=`, brackets, quotes, a backslash or white space), and a
//! path an option names stays inside the tool's own install: relative, without
//! `..`.
use crate::pins::invalid;
use elsecall_contract::failure::Result;
use std::collections::BTreeMap;

/// The options Elsecall carries, in name order.
pub(crate) const CARRIED: [&str; 10] = [
    "asset_pattern",
    "bin",
    "bin_path",
    "exe",
    "matching",
    "rename_exe",
    "slsa_signer_identity",
    "slsa_signer_issuer",
    "strip_components",
    "version_prefix",
];

/// The carried options that name a path inside the tool's install.
const PATHS: [&str; 4] = ["bin", "bin_path", "exe", "rename_exe"];

/// How many characters an option's value may have.
const MAX_VALUE: usize = 256;

/// A pin's carried options, by name. Empty for a pin without any, and for every
/// pin that is not mise's.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct InstallOptions(BTreeMap<&'static str, String>);

impl InstallOptions {
    /// The carried options of a mise tool table, together with those its key
    /// held in brackets; the rest are dropped unread. One option set both ways
    /// is an error. `file` and `key` name the table in errors, which never hold
    /// a value.
    pub(crate) fn from_table(
        file: &str,
        key: &str,
        table: &toml::Table,
        bracketed: &Bracketed,
    ) -> Result<Self> {
        let mut options = bracketed.0.clone();
        for name in CARRIED {
            let Some(value) = table.get(name) else {
                continue;
            };
            let place = format!("{key}.{name}");
            let text = match value {
                toml::Value::String(text) => text.clone(),
                toml::Value::Integer(number) => number.to_string(),
                toml::Value::Boolean(flag) => flag.to_string(),
                _ => {
                    return Err(invalid(
                        file,
                        &place,
                        "an install option is a string, a number or a boolean",
                    ));
                }
            };
            let value = value_of(name, &text).map_err(|rule| invalid(file, &place, rule))?;
            if options.insert(name, value).is_some() {
                return Err(invalid(
                    file,
                    &place,
                    "the option is set both in the tool's brackets and in its table",
                ));
            }
        }
        Ok(Self(options))
    }

    /// Options as a member reported them: only carried names, and values by the
    /// same rules as a file's. The error is the broken rule.
    pub(crate) fn from_wire(
        options: BTreeMap<String, String>,
    ) -> std::result::Result<Self, &'static str> {
        let mut accepted = BTreeMap::new();
        for (name, text) in options {
            let name = carried(&name).ok_or("an install option is not one Elsecall carries")?;
            accepted.insert(name, value_of(name, &text)?);
        }
        Ok(Self(accepted))
    }

    /// Only the options a tool's key held in brackets, for a version written
    /// as a plain string.
    pub(crate) fn from_brackets(bracketed: &Bracketed) -> Self {
        Self(bracketed.0.clone())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.0.iter().map(|(name, value)| (*name, value.as_str()))
    }

    /// The options as a JSON object, for output and the wire.
    pub(crate) fn to_map(&self) -> BTreeMap<String, String> {
        self.iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect()
    }
}

/// The carried options a tool's key holds in brackets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Bracketed(BTreeMap<&'static str, String>);

/// A mise tool key split into its name and the options in its brackets:
/// `ubi:cli/cli[exe=gh]` is `ubi:cli/cli` with `exe`. The error is the broken
/// rule, which names no value.
pub(crate) fn split_key(key: &str) -> std::result::Result<(&str, Bracketed), &'static str> {
    let Some(open) = key.find('[') else {
        return if key.contains(']') {
            Err("a tool name holds a bracket that opens no options")
        } else {
            Ok((key, Bracketed::default()))
        };
    };
    let inner = key[open + 1..]
        .strip_suffix(']')
        .filter(|inner| !inner.contains(['[', ']']))
        .ok_or(
            "options in a tool name are option=value pairs in one pair of brackets at its end",
        )?;
    let mut options = BTreeMap::new();
    for pair in inner.split(',').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair
            .split_once('=')
            .ok_or("options in a tool name are option=value pairs")?;
        if let Some(name) = carried(name) {
            options.insert(name, value_of(name, value)?);
        }
    }
    Ok((&key[..open], Bracketed(options)))
}

fn carried(name: &str) -> Option<&'static str> {
    CARRIED.iter().find(|carried| **carried == name).copied()
}

/// A value as written: literal, bounded, nothing that would end it in mise's
/// bracket syntax, and a path inside the tool's install.
fn value_of(name: &str, text: &str) -> std::result::Result<String, &'static str> {
    if ["{{", "{%", "{#"].iter().any(|open| text.contains(open)) {
        return Err("install options must be literal; Elsecall does not render templates");
    }
    if text.is_empty() || text.chars().count() > MAX_VALUE {
        return Err("an install option is 1 to 256 characters");
    }
    if text.chars().any(|c| {
        c.is_control()
            || c.is_whitespace()
            || matches!(c, ',' | '=' | '[' | ']' | '"' | '\'' | '\\')
    }) {
        return Err(
            "an install option holds no white space, control characters, commas, equals signs, brackets, quotes or backslashes",
        );
    }
    if PATHS.contains(&name)
        && (text.starts_with('/')
            || text.starts_with('~')
            || text.split('/').any(|part| part == ".."))
    {
        return Err("an install option's path is relative to the tool's install, without ..");
    }
    Ok(text.to_owned())
}

impl serde::Serialize for InstallOptions {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use crate::pins::install_options::*;

    fn read(text: &str) -> Result<InstallOptions> {
        let table: toml::Table = toml::from_str(text).unwrap();
        InstallOptions::from_table("mise.toml", "tools.nextest", &table, &Bracketed::default())
    }

    #[test]
    fn only_options_that_pick_the_artifact_are_carried() {
        let options = read(
            r#"
            version = "0.9.150"
            version_prefix = "cargo-nextest-"
            strip_components = 1
            postinstall = "touch /tmp/never"
            install_env = { CC = "/tmp/never" }
            api_url = "https://example.com"
            matching_regex = "x"
            os = ["macos"]
            "#,
        )
        .unwrap();
        assert_eq!(
            options.iter().collect::<Vec<_>>(),
            [
                ("strip_components", "1"),
                ("version_prefix", "cargo-nextest-")
            ]
        );
    }

    #[test]
    fn values_are_literal_and_errors_never_echo_them() {
        for (text, rule) in [
            ("bin = \"{{ env.SECRET }}\"", "must be literal"),
            ("bin = \"\"", "1 to 256 characters"),
            (
                &format!("bin = \"{}\"", "S".repeat(257)),
                "1 to 256 characters",
            ),
            ("bin = \"a\\nSECRET\"", "control characters"),
            ("bin = \"rg,postinstall=SECRET\"", "commas"),
            ("bin = \"rg SECRET\"", "white space"),
            ("bin = \"/SECRET/bin\"", "relative to the tool's install"),
            ("bin = \"../SECRET\"", "without .."),
            ("bin = [\"SECRET\"]", "a string, a number or a boolean"),
            ("bin = 1.5", "a string, a number or a boolean"),
        ] {
            let error = read(text).unwrap_err();
            assert_eq!(error.code, "pin_source_invalid");
            assert!(
                error.message.contains("tools.nextest.bin"),
                "{}",
                error.message
            );
            assert!(error.message.contains(rule), "{}", error.message);
            assert!(!error.message.contains("SECRET"), "{}", error.message);
        }
        assert!(read(&format!("bin = \"{}\"", "a".repeat(256))).is_ok());
        assert_eq!(read("bin = true").unwrap().to_map()["bin"], "true");
        assert!(read("asset_pattern = \"/tool_*_macos.tar.gz\"").is_ok());
    }

    #[test]
    fn options_in_a_tool_key_are_split_off_and_checked_alike() {
        let (name, bracketed) = split_key("ubi:cli/cli[exe=gh,postinstall=touch /x]").unwrap();
        assert_eq!(name, "ubi:cli/cli");
        let table: toml::Table = toml::from_str("version = \"2.0.0\"").unwrap();
        let options =
            InstallOptions::from_table("mise.toml", "tools.x", &table, &bracketed).unwrap();
        assert_eq!(options.iter().collect::<Vec<_>>(), [("exe", "gh")]);
        let both: toml::Table = toml::from_str("version = \"2.0.0\"\nexe = \"gh\"").unwrap();
        assert!(InstallOptions::from_table("mise.toml", "tools.x", &both, &bracketed).is_err());
        assert_eq!(split_key("node").unwrap().0, "node");
        for key in [
            "a]",
            "a[b=c",
            "a[b=c]d",
            "a[b]",
            "a[b=[c]]",
            "a[exe=/bin/sh]",
        ] {
            assert!(split_key(key).is_err(), "{key}");
        }
    }

    #[test]
    fn a_member_may_report_only_carried_options() {
        let carried = BTreeMap::from([("version_prefix".to_owned(), "v".to_owned())]);
        assert_eq!(
            InstallOptions::from_wire(carried).unwrap().to_map()["version_prefix"],
            "v"
        );
        for (name, value) in [
            ("postinstall", "touch"),
            ("bin", "{{ x }}"),
            ("bin_path", "/tmp/elsewhere"),
            ("exe", "gh,postinstall=x"),
        ] {
            let reported = BTreeMap::from([(name.to_owned(), value.to_owned())]);
            assert!(InstallOptions::from_wire(reported).is_err(), "{name}");
        }
    }
}
