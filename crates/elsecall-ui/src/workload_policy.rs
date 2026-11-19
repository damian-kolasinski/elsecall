//! Read-only access to personal workload policy, independent of the fleet service.

use std::path::Path;

use elsecall_host::paths::Paths;
use elsecall_runs::workload_configuration::{self, Configuration, FILE_NAME};
use elsecall_runs::workload_settings::WorkloadSettings;

pub(crate) struct Policy {
    pub(crate) source: Option<String>,
    pub(crate) global: WorkloadSettings,
    pub(crate) has_directory_policies: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadError {
    Unreadable,
    Invalid,
}

impl Policy {
    pub(crate) fn personal() -> std::result::Result<Self, ReadError> {
        let paths = Paths::current().map_err(|_| ReadError::Unreadable)?;
        Self::read(&paths.data.join(FILE_NAME))
    }

    fn read(path: &Path) -> std::result::Result<Self, ReadError> {
        let source = workload_configuration::read(path).map_err(|error| {
            if error.code == "invalid_request" {
                ReadError::Invalid
            } else {
                ReadError::Unreadable
            }
        })?;
        Self::parse(source)
    }

    pub(crate) fn parse(source: Option<String>) -> std::result::Result<Self, ReadError> {
        let configuration = Configuration::parse(source.as_deref().unwrap_or_default())
            .map_err(|_| ReadError::Invalid)?;
        Ok(Self {
            global: configuration.global().clone(),
            has_directory_policies: configuration.has_directory_policies(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const CONFIG: &str = r#"
[admission]
when_unavailable = "run"
[[labels]]
match = "acme test Checkout"
size = 0.625
hold_limit = "15m"
[[labels]]
match = "acme test *"
placement = "placeable"
[[labels]]
match = "acme warm *"
urgency = "deferrable"
deadline = "30m"
at_deadline = "skip"
[[directories."/Users/me/acme".labels]]
match = "*"
size = 8
"#;

    #[test]
    fn reads_global_rules_in_file_order_and_keeps_the_source() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join(FILE_NAME);
        std::fs::write(&path, CONFIG).unwrap();
        let policy = Policy::read(&path).unwrap();
        assert_eq!(policy.source.as_deref(), Some(CONFIG));
        assert!(policy.has_directory_policies);

        let patterns = policy
            .global
            .labels
            .iter()
            .map(|rule| rule.pattern.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            patterns,
            ["acme test Checkout", "acme test *", "acme warm *"]
        );
    }

    #[test]
    fn distinguishes_missing_and_unusable_policy() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join(FILE_NAME);
        let policy = Policy::read(&path).unwrap();
        assert!(policy.source.is_none());
        assert!(policy.global.labels.is_empty());
        std::fs::write(&path, b"[invalid]\nvalue = 'example'").unwrap();
        assert_eq!(Policy::read(&path).err(), Some(ReadError::Invalid));
    }
}
