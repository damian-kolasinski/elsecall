//! Align only Elsecall's existing global mise request, preserving unrelated settings.
use super::{ISSUER, Release, StagedRelease, TOOL, invalid};
use crate::{
    build_identity::Digest,
    failure::{Failure, Result},
    paths::Paths,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub config: PathBuf,
    pub version: String,
    pub identity: Option<String>,
    pub issuer: Option<String>,
}
pub trait CliSelection: Send + Sync {
    fn snapshot(&self) -> Result<Option<Snapshot>>;
    fn activate(
        &self,
        release: &Release,
        staged: &StagedRelease,
        previous: &Snapshot,
    ) -> Result<()>;
    fn restore(&self, release: &Release, previous: &Snapshot) -> Result<()>;
    /// Whether mise retains an installation inside this update's cache.
    fn references(&self, release: &Release, directory: &Path) -> Result<bool>;
}
pub struct MiseSelection {
    pub paths: Paths,
}
#[derive(Deserialize)]
struct Entry {
    active: bool,
    source: Source,
}
#[derive(Deserialize)]
struct Source {
    path: PathBuf,
}
impl CliSelection for MiseSelection {
    fn snapshot(&self) -> Result<Option<Snapshot>> {
        let mise = super::tools::mise(&self.paths)?;
        let bytes = super::tools::run(
            &mise,
            &["ls", "--global", "--current", "--json", TOOL],
            &self.paths.home,
            Duration::from_secs(30),
            &[("HOME".into(), self.paths.home.as_os_str().to_owned())],
        )?;
        let entries: Vec<Entry> = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("Could not read the global mise selection."))?;
        let active: Vec<_> = entries.into_iter().filter(|entry| entry.active).collect();
        if active.is_empty() {
            return Ok(None);
        }
        if active.len() != 1 {
            return Err(invalid(
                "Elsecall has multiple global mise requests; select one before updating.",
            ));
        }
        let config = active.into_iter().next().expect("one entry").source.path;
        owned_config(&self.paths, &config)?;
        let text =
            fs::read_to_string(&config).map_err(|e| Failure::io("Read mise selection", e))?;
        read(&config, &text).map(Some)
    }
    fn activate(
        &self,
        release: &Release,
        staged: &StagedRelease,
        previous: &Snapshot,
    ) -> Result<()> {
        let mise = super::tools::mise(&self.paths)?;
        let request = format!("{TOOL}@{}", release.version);
        // An installed version stays immutable. An existing install may be reused only if byte-identical.
        let where_result = super::tools::run(
            &mise,
            &["where", &request],
            &self.paths.home,
            Duration::from_secs(30),
            &[("HOME".into(), self.paths.home.as_os_str().to_owned())],
        );
        match where_result {
            Ok(bytes) => {
                let directory = std::str::from_utf8(&bytes)
                    .ok()
                    .map(str::trim)
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .ok_or_else(|| invalid("Invalid mise installation path."))?;
                if Digest::of_file(&directory.join("elsecall")).ok() != Some(staged.digest)
                    || Digest::of_file(&directory.join("elsecall-ui")).ok()
                        != Some(staged.window_digest)
                {
                    return Err(invalid(
                        "Another install already occupies this release version in mise.",
                    ));
                }
            }
            Err(_) => {
                let directory = staged.executable.parent().expect("release directory");
                super::tools::run(
                    &mise,
                    &[
                        "link",
                        &request,
                        directory.to_str().expect("UTF8 release path"),
                    ],
                    &self.paths.home,
                    Duration::from_secs(30),
                    &[("HOME".into(), self.paths.home.as_os_str().to_owned())],
                )?;
            }
        }
        edit(&self.paths, previous, &applied(release, previous))
    }
    fn restore(&self, release: &Release, previous: &Snapshot) -> Result<()> {
        edit(&self.paths, &applied(release, previous), previous)
    }
    fn references(&self, release: &Release, directory: &Path) -> Result<bool> {
        #[derive(Deserialize)]
        struct Installed {
            version: String,
            install_path: PathBuf,
        }
        let mise = super::tools::mise(&self.paths)?;
        let bytes = super::tools::run(
            &mise,
            &["ls", "--installed", "--json", TOOL],
            &self.paths.home,
            Duration::from_secs(30),
            &[("HOME".into(), self.paths.home.as_os_str().to_owned())],
        )?;
        let entries: Vec<Installed> = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("Could not read installed mise releases."))?;
        let directory =
            fs::canonicalize(directory).map_err(|e| Failure::io("Locate release cache", e))?;
        for entry in entries {
            if entry.version == release.version.to_string() {
                let installed = fs::canonicalize(entry.install_path)
                    .map_err(|e| Failure::io("Locate selected release", e))?;
                if installed.starts_with(&directory) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}
fn applied(release: &Release, previous: &Snapshot) -> Snapshot {
    Snapshot {
        config: previous.config.clone(),
        version: release.version.to_string(),
        identity: Some(release.identity()),
        issuer: Some(ISSUER.into()),
    }
}
fn owned_config(paths: &Paths, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if !path.starts_with(&paths.home) {
        return Err(invalid(
            "The global mise request is outside this user's home.",
        ));
    }
    crate::private_files::owned_tree(&paths.home, path.parent().expect("config parent"))?;
    let metadata =
        fs::symlink_metadata(path).map_err(|e| Failure::io("Inspect mise configuration", e))?;
    if !metadata.is_file()
        || metadata.uid() != crate::local_access::user_id()
        || metadata.mode() & 0o022 != 0
    {
        return Err(invalid(
            "The mise configuration must be an owned file that other users cannot modify.",
        ));
    }
    Ok(())
}
fn read(path: &Path, text: &str) -> Result<Snapshot> {
    let document: toml::Value = toml::from_str(text)
        .map_err(|_| invalid("The mise configuration is not readable TOML."))?;
    let node = document
        .get("tools")
        .and_then(|tools| tools.get(TOOL))
        .ok_or_else(|| {
            invalid("The global Elsecall request must use its full packslip project name.")
        })?;
    let version = node
        .as_str()
        .or_else(|| node.get("version").and_then(toml::Value::as_str))
        .ok_or_else(|| invalid("The global Elsecall request has no single version."))?;
    if version.len() > 128 || version.contains(char::is_whitespace) {
        return Err(invalid(
            "The global Elsecall request is not a single version.",
        ));
    }
    let field = |name: &str| {
        node.get(name)
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    };
    let identity = field("identity");
    let issuer = field("issuer");
    if identity.as_ref().is_some_and(|value| {
        !value.starts_with(&format!(
            "https://{}/.github/workflows/release.yml@refs/tags/v",
            super::PROJECT
        ))
    }) || issuer.as_ref().is_some_and(|value| value != ISSUER)
    {
        return Err(invalid(
            "The existing mise request uses a different release trust policy.",
        ));
    }
    Ok(Snapshot {
        config: path.into(),
        version: version.into(),
        identity,
        issuer,
    })
}
fn edit(paths: &Paths, expected: &Snapshot, target: &Snapshot) -> Result<()> {
    owned_config(paths, &expected.config)?;
    let text =
        fs::read_to_string(&expected.config).map_err(|e| Failure::io("Read mise request", e))?;
    if read(&expected.config, &text)? != *expected {
        return Err(invalid(
            "The global Elsecall request changed during the update; it was preserved.",
        ));
    }
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| invalid("The mise configuration is not readable TOML."))?;
    let node = &mut document["tools"][TOOL];
    if node.as_str().is_some() {
        *node =
            toml_edit::Item::Value(toml_edit::Value::InlineTable(toml_edit::InlineTable::new()));
    }
    set(node, "version", Some(&target.version));
    set(node, "identity", target.identity.as_deref());
    set(node, "issuer", target.issuer.as_deref());
    let parent = expected.config.parent().expect("config parent");
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| Failure::io("Stage mise request", e))?;
    use std::io::Write;
    temporary
        .as_file()
        .set_permissions(
            fs::metadata(&expected.config)
                .map_err(|e| Failure::io("Read mise permissions", e))?
                .permissions(),
        )
        .map_err(|e| Failure::io("Set mise permissions", e))?;
    temporary
        .write_all(document.to_string().as_bytes())
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|e| Failure::io("Write mise request", e))?;
    if fs::read_to_string(&expected.config).ok().as_deref() != Some(text.as_str()) {
        return Err(invalid(
            "The mise configuration changed during the update; retry.",
        ));
    }
    temporary
        .persist(&expected.config)
        .map_err(|e| Failure::io("Replace mise request", e.error))?;
    Ok(())
}
fn set(node: &mut toml_edit::Item, key: &str, value: Option<&str>) {
    match node {
        toml_edit::Item::Table(table) => {
            if let Some(value) = value {
                table.insert(key, toml_edit::value(value));
            } else {
                table.remove(key);
            }
        }
        toml_edit::Item::Value(toml_edit::Value::InlineTable(table)) => {
            if let Some(value) = value {
                table.insert(key, value.into());
            } else {
                table.remove(key);
            }
        }
        _ => unreachable!("validated mise tool request"),
    }
}
