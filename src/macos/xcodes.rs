//! Xcode installs on this Mac, and the `xcodes` executable when it is present.
//!
//! Installed copies are `*.app` bundles directly in the applications directory
//! (`ELSECALL_XCODE_APPLICATIONS`, or `/Applications`) whose `Contents/Info.plist`
//! has `CFBundleIdentifier` `com.apple.dt.Xcode`. A symlink to a bundle counts.
//! When the directory contains both a bundle and a symlink to it, the install is
//! reported once, at the path that is not a symlink. The version is
//! `CFBundleShortVersionString` from that plist. The build is `ProductBuildVersion`
//! from `Contents/version.plist`. A bundle whose plists cannot be read, a plist
//! larger than 1 MiB, or a bundle that is not Xcode, is skipped.
//!
//! The default is the bundle whose `Contents/Developer` is the path printed by
//! `/usr/bin/xcode-select -p` (`ELSECALL_XCODE_SELECT` replaces that executable).
//! A failure, or a path that matches no bundle (including Command Line Tools),
//! leaves every Xcode non-default.
//!
//! The `xcodes` executable is `ELSECALL_XCODES` when that names a file, otherwise
//! the first file among `/opt/homebrew/bin/xcodes`, `/usr/local/bin/xcodes` and
//! `~/.local/share/mise/shims/xcodes`. A non-empty override that is not an
//! absolute path fails the manager. An absolute override that is not a file
//! hides the defaults and the executable is missing. The same absolute-path rule
//! applies to `ELSECALL_XCODE_APPLICATIONS` and `ELSECALL_XCODE_SELECT`. Those
//! overrides are honoured in every build, like `ELSECALL_MACHINE_LOAD`. Installed
//! Xcodes are still listed when the executable is missing. Its version is the
//! first token of `xcodes version` (1.6.0 on this machine prints that line).
//! Output is not logged.
use crate::{
    command_capture::{self, Program},
    tool_inventory::{
        Detail, Executable, InstalledTool, InventoryFailure, Manager, ManagerInventory,
        ToolManager, ToolName, ToolVersion, version_token,
    },
};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};

const VERSION_TIMEOUT: Duration = Duration::from_secs(30);
const SELECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PLIST_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Default)]
pub struct XcodesCli;

impl ToolManager for XcodesCli {
    fn manager(&self) -> Manager {
        Manager::Xcodes
    }

    fn inventory(&self) -> Result<ManagerInventory, InventoryFailure> {
        let mut tools = scan(&applications_dir()?)?;
        apply_default(&mut tools, selected_developer()?.as_deref())?;
        let executable = match xcodes_program()? {
            None => Executable::Missing,
            Some(path) => {
                let stdout = run(&path, &["version"], "xcodes version", VERSION_TIMEOUT)?;
                Executable::Found {
                    path,
                    version: version_token(&stdout, "xcodes version")?,
                }
            }
        };
        ManagerInventory::try_new(Manager::Xcodes, executable, tools)
    }
}

fn applications_dir() -> Result<PathBuf, InventoryFailure> {
    directory_from(command_capture::classify(command_capture::path_override(
        "ELSECALL_XCODE_APPLICATIONS",
    )))
}

fn directory_from(value: command_capture::OverridePath) -> Result<PathBuf, InventoryFailure> {
    match value {
        command_capture::OverridePath::Unset => Ok(PathBuf::from("/Applications")),
        command_capture::OverridePath::Absolute(path) => Ok(path),
        command_capture::OverridePath::NotAbsolute => Err(InventoryFailure::not_absolute(
            "ELSECALL_XCODE_APPLICATIONS",
        )),
    }
}

fn xcodes_program() -> Result<Option<PathBuf>, InventoryFailure> {
    executable_from(command_capture::locate(
        command_capture::path_override("ELSECALL_XCODES"),
        &xcodes_candidates(),
    ))
}

fn xcodes_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin/xcodes"),
        PathBuf::from("/usr/local/bin/xcodes"),
    ];
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        candidates.push(PathBuf::from(home).join(".local/share/mise/shims/xcodes"));
    }
    candidates
}

fn executable_from(located: command_capture::Located) -> Result<Option<PathBuf>, InventoryFailure> {
    match located {
        command_capture::Located::Found(path) => Ok(Some(path)),
        command_capture::Located::Missing => Ok(None),
        command_capture::Located::NotAbsolute => {
            Err(InventoryFailure::not_absolute("ELSECALL_XCODES"))
        }
    }
}

fn scan(directory: &Path) -> Result<Vec<InstalledTool>, InventoryFailure> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(InventoryFailure::failed(&format!(
                "the applications directory could not be read: {error}"
            )));
        }
    };
    let mut tools = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if !is_app_bundle(&entry.file_name()) || !path.is_dir() {
            continue;
        }
        if let Some(tool) = read_bundle(&path) {
            tools.push(tool);
        }
    }
    Ok(dedupe_bundles(tools))
}

fn dedupe_bundles(tools: Vec<InstalledTool>) -> Vec<InstalledTool> {
    let mut kept = Vec::new();
    let mut keys = Vec::new();
    for tool in tools {
        let path = bundle_path(&tool);
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let link = is_symlink(path);
        if let Some(index) = keys.iter().position(|existing| existing == &key) {
            let current = bundle_path(&kept[index]);
            if prefer_bundle(current, is_symlink(current), path, link) {
                kept[index] = tool;
            }
            continue;
        }
        keys.push(key);
        kept.push(tool);
    }
    kept
}

fn bundle_path(tool: &InstalledTool) -> &Path {
    match &tool.detail {
        Some(Detail::Xcode { path, .. }) => path,
        None => Path::new(""),
    }
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

fn prefer_bundle(
    current: &Path,
    current_link: bool,
    candidate: &Path,
    candidate_link: bool,
) -> bool {
    match (current_link, candidate_link) {
        (true, false) => true,
        (false, true) => false,
        _ => candidate < current,
    }
}

fn is_app_bundle(name: &OsStr) -> bool {
    name.as_encoded_bytes().ends_with(b".app")
}

fn read_bundle(bundle: &Path) -> Option<InstalledTool> {
    let info = dictionary(&bundle.join("Contents/Info.plist"))?;
    let versions = dictionary(&bundle.join("Contents/version.plist"))?;
    let (version, build) = xcode_release(&info, &versions)?;
    Some(InstalledTool {
        name: ToolName::new("xcode").expect("xcode is a valid tool name"),
        version,
        default: false,
        detail: Some(Detail::Xcode {
            build,
            path: bundle.to_path_buf(),
        }),
    })
}

fn dictionary(path: &Path) -> Option<plist::Dictionary> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_PLIST_BYTES {
        return None;
    }
    match plist::Value::from_file(path).ok()? {
        plist::Value::Dictionary(dictionary) => Some(dictionary),
        _ => None,
    }
}

fn xcode_release(
    info: &plist::Dictionary,
    version_plist: &plist::Dictionary,
) -> Option<(ToolVersion, String)> {
    if info.get("CFBundleIdentifier")?.as_string()? != "com.apple.dt.Xcode" {
        return None;
    }
    let version = ToolVersion::new(info.get("CFBundleShortVersionString")?.as_string()?)?;
    let build = version_plist.get("ProductBuildVersion")?.as_string()?;
    ToolVersion::new(build)?;
    Some((version, build.to_owned()))
}

fn selected_developer() -> Result<Option<PathBuf>, InventoryFailure> {
    let program = select_from(command_capture::classify(command_capture::path_override(
        "ELSECALL_XCODE_SELECT",
    )))?;
    let captured = match command_capture::run(Program {
        program: &program,
        arguments: &["-p"],
        directory: Path::new("/"),
        timeout: SELECT_TIMEOUT,
        remove_env: &[],
    }) {
        Ok(captured) => captured,
        Err(_) => return Ok(None),
    };
    if !captured.status.success() {
        return Ok(None);
    }
    Ok(developer_path(&captured.stdout))
}

fn select_from(value: command_capture::OverridePath) -> Result<PathBuf, InventoryFailure> {
    match value {
        command_capture::OverridePath::Unset => Ok(PathBuf::from("/usr/bin/xcode-select")),
        command_capture::OverridePath::Absolute(path) => Ok(path),
        command_capture::OverridePath::NotAbsolute => {
            Err(InventoryFailure::not_absolute("ELSECALL_XCODE_SELECT"))
        }
    }
}

fn developer_path(stdout: &[u8]) -> Option<PathBuf> {
    let text = std::str::from_utf8(stdout).ok()?;
    let line = text.lines().next()?.trim().trim_end_matches('/');
    if line.is_empty() {
        None
    } else {
        Some(PathBuf::from(line))
    }
}

fn apply_default(
    tools: &mut [InstalledTool],
    developer: Option<&Path>,
) -> Result<(), InventoryFailure> {
    let Some(developer) = developer else {
        return Ok(());
    };
    let mut selected = None;
    for (index, tool) in tools.iter().enumerate() {
        let Some(Detail::Xcode { path, .. }) = &tool.detail else {
            continue;
        };
        if !same_developer(path, developer) {
            continue;
        }
        if selected.is_some() {
            return Err(InventoryFailure::output_invalid(
                "xcode-select -p",
                "more than one Xcode matched the selected developer directory",
            ));
        }
        selected = Some(index);
    }
    if let Some(index) = selected {
        tools[index].default = true;
    }
    Ok(())
}

fn same_developer(bundle: &Path, developer: &Path) -> bool {
    let expected = bundle.join("Contents").join("Developer");
    if expected == developer {
        return true;
    }
    match (
        std::fs::canonicalize(&expected),
        std::fs::canonicalize(developer),
    ) {
        (Ok(expected), Ok(developer)) => expected == developer,
        _ => false,
    }
}

fn run(
    program: &Path,
    arguments: &[&str],
    label: &str,
    timeout: Duration,
) -> Result<Vec<u8>, InventoryFailure> {
    match command_capture::run(Program {
        program,
        arguments,
        directory: Path::new("/"),
        timeout,
        remove_env: &[],
    }) {
        Ok(captured) if captured.status.success() => Ok(captured.stdout),
        Ok(captured) => Err(InventoryFailure::failed(&exit_detail(
            label,
            captured.status,
        ))),
        Err(command_capture::CaptureFailure::TimedOut) => Err(InventoryFailure::timeout(label)),
        Err(command_capture::CaptureFailure::TooLarge) => Err(InventoryFailure::output_invalid(
            label,
            "the output was larger than 8 MiB",
        )),
        Err(command_capture::CaptureFailure::Start(error)) => Err(InventoryFailure::failed(
            &format!("{label} could not be started: {error}"),
        )),
    }
}

fn exit_detail(label: &str, status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("{label} exited with status {code}"),
        None => format!("{label} was terminated by a signal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    fn plist(pairs: &[(&str, &str)]) -> String {
        let mut body = String::from(concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            "\n",
            r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#,
            "\n",
            r#"<plist version="1.0"><dict>"#,
        ));
        for (key, value) in pairs {
            body.push_str("<key>");
            body.push_str(key);
            body.push_str("</key><string>");
            body.push_str(value);
            body.push_str("</string>");
        }
        body.push_str("</dict></plist>\n");
        body
    }

    fn install_xcode(directory: &Path, name: &str) -> PathBuf {
        let bundle = directory.join(name);
        let contents = bundle.join("Contents");
        fs::create_dir_all(contents.join("Developer")).unwrap();
        fs::write(
            contents.join("Info.plist"),
            plist(&[
                ("CFBundleIdentifier", "com.apple.dt.Xcode"),
                ("CFBundleShortVersionString", "26.2"),
            ]),
        )
        .unwrap();
        fs::write(
            contents.join("version.plist"),
            plist(&[("ProductBuildVersion", "17C52")]),
        )
        .unwrap();
        bundle
    }

    fn strings(pairs: &[(&str, &str)]) -> plist::Dictionary {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), plist::Value::String((*value).to_owned())))
            .collect()
    }

    fn xcode(path: &str, version: &str) -> InstalledTool {
        InstalledTool {
            name: ToolName::new("xcode").unwrap(),
            version: ToolVersion::new(version).unwrap(),
            default: false,
            detail: Some(Detail::Xcode {
                build: "17C52".into(),
                path: PathBuf::from(path),
            }),
        }
    }

    #[test]
    fn a_release_is_the_xcode_bundle_version_and_build() {
        let info = strings(&[
            ("CFBundleIdentifier", "com.apple.dt.Xcode"),
            ("CFBundleShortVersionString", "26.2"),
        ]);
        let versions = strings(&[("ProductBuildVersion", "17C52")]);
        let (version, build) = xcode_release(&info, &versions).unwrap();
        assert_eq!(version.as_str(), "26.2");
        assert_eq!(build, "17C52");

        let other = strings(&[("CFBundleIdentifier", "com.apple.TextEdit")]);
        assert!(xcode_release(&other, &versions).is_none());
        let blank = strings(&[
            ("CFBundleIdentifier", "com.apple.dt.Xcode"),
            ("CFBundleShortVersionString", "26 .2"),
        ]);
        assert!(xcode_release(&blank, &versions).is_none());
        assert!(xcode_release(&info, &plist::Dictionary::new()).is_none());
    }

    #[test]
    fn the_selected_developer_directory_marks_one_xcode() {
        let bundle = Path::new("/Applications/Xcode-26.2.0.app");
        let developer = Path::new("/Applications/Xcode-26.2.0.app/Contents/Developer");
        assert!(same_developer(bundle, developer));
        assert!(!same_developer(
            bundle,
            Path::new("/Library/Developer/CommandLineTools")
        ));
        assert_eq!(
            developer_path(b"/Applications/Xcode-26.2.0.app/Contents/Developer\n").unwrap(),
            developer
        );
        assert!(developer_path(b" \n").is_none());

        let mut tools = vec![
            xcode("/Applications/Xcode-16.4.0.app", "16.4"),
            xcode("/Applications/Xcode-26.2.0.app", "26.2"),
        ];
        apply_default(&mut tools, Some(developer)).unwrap();
        assert!(!tools[0].default);
        assert!(tools[1].default);
        let mut unmarked = vec![
            xcode("/Applications/Xcode-16.4.0.app", "16.4"),
            xcode("/Applications/Xcode-26.2.0.app", "26.2"),
        ];
        apply_default(&mut unmarked, None).unwrap();
        apply_default(
            &mut unmarked,
            Some(Path::new("/Library/Developer/CommandLineTools")),
        )
        .unwrap();
        assert!(unmarked.iter().all(|tool| !tool.default));

        let mut twins = vec![
            xcode("/Applications/Xcode-26.2.0.app", "26.2"),
            xcode("/Applications/Xcode-26.2.0.app", "26.2"),
        ];
        assert_eq!(
            apply_default(&mut twins, Some(developer))
                .unwrap_err()
                .code(),
            "tool_manager_output_invalid"
        );
    }

    #[test]
    fn a_symlinked_app_is_listed_and_a_link_beside_the_bundle_is_one_default() {
        let root = tempfile::tempdir().unwrap();
        let bundle = install_xcode(root.path(), "Xcode-26.2.0.app");
        let only = root.path().join("only");
        fs::create_dir(&only).unwrap();
        symlink(&bundle, only.join("Xcode.app")).unwrap();
        let listed = scan(&only).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].version.as_str(), "26.2");
        assert_eq!(bundle_path(&listed[0]), only.join("Xcode.app"));

        let apps = root.path().join("Applications");
        fs::create_dir(&apps).unwrap();
        let real = install_xcode(&apps, "Xcode-26.2.0.app");
        symlink(&real, apps.join("Xcode.app")).unwrap();
        let mut tools = scan(&apps).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(bundle_path(&tools[0]), real);
        apply_default(&mut tools, Some(&apps.join("Xcode.app/Contents/Developer"))).unwrap();
        assert!(tools[0].default);
    }

    #[test]
    fn a_plist_larger_than_one_mibibyte_is_skipped() {
        let root = tempfile::tempdir().unwrap();
        let contents = root.path().join("Xcode.app/Contents");
        fs::create_dir_all(&contents).unwrap();
        let mut info = String::from(concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            "\n",
            r#"<plist version="1.0"><dict>"#,
            "<key>CFBundleIdentifier</key><string>com.apple.dt.Xcode</string>",
            "<key>CFBundleShortVersionString</key><string>26.2</string>",
            "<key>pad</key><string>",
        ));
        info.push_str(&"x".repeat(MAX_PLIST_BYTES as usize));
        info.push_str("</string></dict></plist>\n");
        fs::write(contents.join("Info.plist"), info).unwrap();
        fs::write(
            contents.join("version.plist"),
            plist(&[("ProductBuildVersion", "17C52")]),
        )
        .unwrap();
        assert!(contents.join("Info.plist").metadata().unwrap().len() > MAX_PLIST_BYTES);
        assert!(scan(root.path()).unwrap().is_empty());
    }

    #[test]
    fn a_relative_override_names_its_variable() {
        let applications = directory_from(command_capture::OverridePath::NotAbsolute).unwrap_err();
        assert_eq!(applications.code(), "tool_manager_failed");
        assert_eq!(
            applications.hint(),
            "ELSECALL_XCODE_APPLICATIONS must be an absolute path"
        );
        let select = select_from(command_capture::OverridePath::NotAbsolute).unwrap_err();
        assert_eq!(
            select.hint(),
            "ELSECALL_XCODE_SELECT must be an absolute path"
        );
        let executable = executable_from(command_capture::Located::NotAbsolute).unwrap_err();
        assert_eq!(
            executable.hint(),
            "ELSECALL_XCODES must be an absolute path"
        );
        assert!(
            executable_from(command_capture::Located::Missing)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            select_from(command_capture::OverridePath::Absolute(PathBuf::from(
                "/tmp/xcode-select"
            )))
            .unwrap(),
            Path::new("/tmp/xcode-select")
        );
    }
}
