//! Read-only access to personal workload policy, independent of the fleet service.

use std::path::Path;

use elsecall::admission_cli::{
    environment::CallerEnvironment,
    options::Declared,
    resolution::{self, Layers, Resolved},
};
use elsecall::failure::Result;
use elsecall::paths::Paths;
use elsecall::scheduling::{request::Kind, timestamp::Timestamp};
use elsecall::workload_configuration::{self, Configuration, FILE_NAME};
use elsecall::workload_settings::WorkloadSettings;

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

    pub(crate) fn preview(&self, label: &str, now: Timestamp) -> Result<Resolved> {
        let options = Declared {
            label: (!label.is_empty()).then(|| label.to_owned()),
            ..Declared::default()
        };
        resolution::resolve(
            Layers {
                kind: Kind::Run,
                holder_pid: None,
                cli: &options,
                placement: None,
                when_unavailable: None,
                environment: &CallerEnvironment::default(),
                configuration: &self.global,
            },
            now,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use elsecall::scheduling::{
        capacity::Cores,
        request::{AtDeadline, Placement, Urgency},
    };
    use elsecall::workload_settings::WhenUnavailable;

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
    fn preview_resolves_global_rules_in_file_order() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join(FILE_NAME);
        std::fs::write(&path, CONFIG).unwrap();
        let policy = Policy::read(&path).unwrap();
        let now = Timestamp::from_millis(10_000);
        assert_eq!(policy.source.as_deref(), Some(CONFIG));
        assert!(policy.has_directory_policies);

        let exact = policy.preview("acme test Checkout", now).unwrap();
        assert_eq!(exact.when_unavailable, WhenUnavailable::Run);
        assert_eq!(exact.request.size, Some(Cores::from_millis(625)));
        assert_eq!(exact.request.placement, Placement::Pinned);
        assert_eq!(
            exact.request.urgency,
            Urgency::Interactive {
                hold_limit_ms: Some(900_000)
            }
        );
        assert_eq!(exact.request.settings.unwrap().rule.unwrap().number, 1);

        let wildcard = policy.preview("acme test Search", now).unwrap().request;
        assert_eq!(wildcard.size, None);
        assert_eq!(wildcard.placement, Placement::Placeable);
        assert_eq!(wildcard.settings.unwrap().rule.unwrap().number, 2);

        let warm = policy.preview("acme warm Cache", now).unwrap().request;
        assert_eq!(warm.settings.unwrap().rule.unwrap().number, 3);
        assert_eq!(
            warm.urgency,
            Urgency::Deferrable {
                deadline: Timestamp::from_millis(1_810_000),
                at_deadline: AtDeadline::Skip,
            }
        );
        let unlabelled = policy.preview("", now).unwrap().request;
        assert_eq!(unlabelled.label, None);
        assert!(unlabelled.settings.unwrap().rule.is_none());
        assert!(policy.preview(&"x".repeat(201), now).is_err());
    }

    #[test]
    fn distinguishes_missing_and_unusable_policy() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join(FILE_NAME);
        let policy = Policy::read(&path).unwrap();
        assert!(policy.source.is_none());
        assert!(policy.global.labels.is_empty());
        assert_eq!(
            policy
                .preview("test", Timestamp::from_millis(0))
                .unwrap()
                .when_unavailable,
            WhenUnavailable::Fail
        );
        std::fs::write(&path, b"[invalid]\nvalue = 'example'").unwrap();
        assert_eq!(Policy::read(&path).err(), Some(ReadError::Invalid));
    }
}
