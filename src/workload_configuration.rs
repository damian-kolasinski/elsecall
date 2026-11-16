//! Personal workload policy, independent of repositories and service generations.
use crate::{
    consumer_configuration::ConsumerConfiguration,
    failure::{Failure, Result},
    paths::Paths,
};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
};

pub const FILE_NAME: &str = "workloads.toml";
pub const LIMIT: u64 = 1024 * 1024;
const MAXIMUM_DIRECTORIES: usize = 128;

#[derive(Debug, Default)]
pub struct Configuration {
    defaults: ConsumerConfiguration,
    directories: BTreeMap<PathBuf, ConsumerConfiguration>,
}

pub fn invalid() -> Failure {
    Failure::new(
        "invalid_request",
        "The personal workload configuration is invalid.",
        "Fix workloads.toml in Elsecall's state directory (README, \"Workload configuration\"). Values are omitted from diagnostics.",
    )
}

/// Missing configuration means defaults. Never inspects repository files.
pub fn read(path: &Path) -> Result<Option<String>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Failure::io("Read workload configuration", e)),
    };
    let mut text = String::new();
    file.take(LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|_| invalid())?;
    if text.len() as u64 > LIMIT {
        return Err(invalid());
    }
    Ok(Some(text))
}

pub fn load(paths: &Paths, directory: &Path) -> Result<ConsumerConfiguration> {
    let text = read(&paths.data.join(FILE_NAME))?.unwrap_or_default();
    let configuration = Configuration::parse(&text)?;
    let directory = directory
        .canonicalize()
        .map_err(|e| Failure::io("Resolve workload directory", e))?;
    Ok(configuration.for_directory(&directory).clone())
}

impl Configuration {
    pub fn parse(text: &str) -> Result<Self> {
        if text.len() as u64 > LIMIT {
            return Err(invalid());
        }
        let mut table: toml::Table = toml::from_str(text).map_err(|_| invalid())?;
        let directories = match table.remove("directories") {
            Some(toml::Value::Table(table)) => table,
            None => toml::Table::new(),
            _ => return Err(invalid()),
        };
        if directories.len() > MAXIMUM_DIRECTORIES {
            return Err(invalid());
        }
        let parse = |table: toml::Table| {
            ConsumerConfiguration::parse(&toml::to_string(&table).expect("TOML table serializes"))
                .map_err(|_| invalid())
        };
        let defaults = parse(table)?;
        let mut scoped = BTreeMap::new();
        for (directory, settings) in directories {
            let path = PathBuf::from(directory);
            if !path.is_absolute()
                || path.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
            {
                return Err(invalid());
            }
            let toml::Value::Table(table) = settings else {
                return Err(invalid());
            };
            scoped.insert(path, parse(table)?);
        }
        Ok(Self {
            defaults,
            directories: scoped,
        })
    }

    /// The most specific directory policy replaces defaults in full: importing a
    /// policy cannot accidentally add forwarded variables or returned outputs.
    pub fn for_directory(&self, directory: &Path) -> &ConsumerConfiguration {
        self.directories
            .iter()
            .filter(|(path, _)| directory.starts_with(path))
            .max_by_key(|(path, _)| path.components().count())
            .map(|(_, settings)| settings)
            .unwrap_or(&self.defaults)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_most_specific_directory_are_complete_policies() {
        let config = Configuration::parse(
            r#"
[environment]
names = ["CI"]
[[labels]]
match = "*"
size = 1
[directories."/Users/me/acme".outputs]
paths = ["build/reports"]
[[directories."/Users/me/acme".labels]]
match = "test *"
size = 4
[directories."/Users/me/acme/nested".admission]
when_unavailable = "run"
"#,
        )
        .unwrap();
        let fallback = config.for_directory(Path::new("/Users/me/acme-other"));
        assert_eq!(fallback.forward_names, ["CI"]);
        assert_eq!(
            fallback
                .rule_for("test App")
                .unwrap()
                .size
                .unwrap()
                .as_f64(),
            1.0
        );
        let scoped = config.for_directory(Path::new("/Users/me/acme/src"));
        assert!(scoped.forward_names.is_empty());
        assert_eq!(scoped.outputs[0].as_str(), "build/reports");
        assert_eq!(
            scoped.rule_for("test App").unwrap().size.unwrap().as_f64(),
            4.0
        );
        let nested = config.for_directory(Path::new("/Users/me/acme/nested/src"));
        assert!(nested.outputs.is_empty());
        assert!(nested.labels.is_empty());
        assert!(nested.when_unavailable.is_some());
    }
    #[test]
    fn rejects_invalid_settings_in_every_scope_without_echoing_values() {
        for text in [
            "secret = 'do-not-print'",
            "directories = 4",
            "[directories.relative]\n",
            "[directories.\"/Users/me/../devbox\"]\n",
            "[directories.\"/Users/me/acme\".environment]\nnames = ['ELSECALL_TOKEN']",
            "[directories.\"/Users/me/acme\".outputs]\npaths = ['../do-not-print']",
        ] {
            let failure = Configuration::parse(text).unwrap_err();
            assert!(!format!("{failure:?}").contains("do-not-print"));
        }
    }
}
