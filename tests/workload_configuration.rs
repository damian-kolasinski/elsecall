//! Personal policy import through the real CLI and filesystem; no service needed.
mod processes;
use elsecall::{
    paths::Paths,
    workload_configuration::{self, FILE_NAME},
};
use serde_json::Value;
use std::{os::unix::fs::PermissionsExt, path::Path};

fn cli(home: &Path, args: &[&str]) -> (bool, Value) {
    let output = processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", home)
        .args(args)
        .output()
        .unwrap();
    (
        output.status.success(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}

const LEGACY: &str = r#"# Preserve these choices and their comments.
[outputs]
paths = ["build/reports"]
[workspace]
refs = ["refs/tags/release-*"]
[environment]
names = ["CI"]
prefixes = ["APP_"]
[consistency]
configuration = ["~/.gradle/gradle.properties"]
[consistency.toolchains]
xcode = ["xcodebuild", "-version"]
[admission]
when_unavailable = "run"
[[labels]]
match = "acme test *"
size = 4.5
hold_limit = "15m"
placement = "placeable"
[[labels]]
match = "acme warm *"
urgency = "deferrable"
deadline = "30m"
at_deadline = "skip"
"#;

#[test]
fn migration_preserves_complete_policy_and_unrelated_settings_and_is_repeatable() {
    elsecall::workload_settings::WorkloadSettings::parse(LEGACY).unwrap();
    let home = tempfile::Builder::new()
        .prefix("ec-policy-")
        .tempdir_in("/tmp")
        .unwrap();
    let repo = tempfile::Builder::new()
        .prefix("ec-policy-")
        .tempdir_in("/tmp")
        .unwrap();
    let paths = Paths::for_home(home.path()).unwrap();
    elsecall::private_files::directory(&paths.data).unwrap();
    let service = "[capacity]\nreserved_cores = 3\n";
    std::fs::write(paths.configuration(), service).unwrap();
    let original = "# Personal defaults\n[environment]\nnames = ['GLOBAL_CI']\n";
    let target = paths.data.join(FILE_NAME);
    std::fs::write(&target, original).unwrap();
    let legacy = repo.path().join(".elsecall.toml");
    std::fs::write(&legacy, LEGACY).unwrap();
    let args = ["config", "migrate", legacy.to_str().unwrap()];
    let (ok, result) = cli(home.path(), &args);
    assert!(ok, "{result}");
    assert_eq!(result["data"]["imported"], 1);
    assert_eq!(result["data"]["sources_preserved"], true);
    assert_eq!(
        std::fs::read_to_string(result["data"]["backup"].as_str().unwrap()).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read_to_string(paths.configuration()).unwrap(),
        service
    );
    assert_eq!(std::fs::read_to_string(&legacy).unwrap(), LEGACY);
    let migrated = std::fs::read_to_string(&target).unwrap();
    assert!(migrated.contains("# Personal defaults"));
    assert!(migrated.contains("# Preserve these choices"));
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let resolved = workload_configuration::load(&paths, repo.path()).unwrap();
    assert_eq!(
        resolved,
        elsecall::workload_settings::WorkloadSettings::parse(LEGACY).unwrap()
    );
    assert_eq!(
        workload_configuration::load(&paths, home.path())
            .unwrap()
            .forward_names,
        ["GLOBAL_CI"]
    );
    let (ok, repeat) = cli(home.path(), &args);
    assert!(ok, "{repeat}");
    assert_eq!(repeat["data"]["changed"], false);
    assert_eq!(std::fs::read_to_string(target).unwrap(), migrated);
}

#[test]
fn conflicts_or_invalid_sources_leave_every_file_untouched() {
    let home = tempfile::Builder::new()
        .prefix("ec-policy-")
        .tempdir_in("/tmp")
        .unwrap();
    let repo = tempfile::Builder::new()
        .prefix("ec-policy-")
        .tempdir_in("/tmp")
        .unwrap();
    let other = tempfile::Builder::new()
        .prefix("ec-policy-")
        .tempdir_in("/tmp")
        .unwrap();
    let legacy = repo.path().join(".elsecall.toml");
    std::fs::write(&legacy, LEGACY).unwrap();
    let paths = Paths::for_home(home.path()).unwrap();
    let args = ["config", "migrate", repo.path().to_str().unwrap()];
    let (ok, result) = cli(home.path(), &args);
    assert!(ok, "{result}");
    let target = paths.data.join(FILE_NAME);
    let original = std::fs::read(&target).unwrap();
    std::fs::write(&legacy, "[environment]\nnames = ['DIFFERENT']\n").unwrap();
    let (ok, failure) = cli(home.path(), &args);
    assert!(!ok);
    assert_eq!(failure["error"]["code"], "configuration_conflict");
    assert_eq!(std::fs::read(&target).unwrap(), original);
    // An earlier valid import cannot be committed when a later input is invalid.
    let fresh = tempfile::Builder::new()
        .prefix("ec-policy-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::write(
        other.path().join(".elsecall.toml"),
        "secret = 'do-not-print'",
    )
    .unwrap();
    let (ok, failure) = cli(
        fresh.path(),
        &[
            "config",
            "migrate",
            repo.path().to_str().unwrap(),
            other.path().to_str().unwrap(),
        ],
    );
    assert!(!ok);
    assert!(!failure.to_string().contains("do-not-print"));
    let fresh_paths = Paths::for_home(fresh.path()).unwrap();
    assert!(!fresh_paths.data.join(FILE_NAME).exists());
}

#[test]
fn repository_files_are_ignored_and_directory_policy_stops_at_nested_git_roots() {
    let home = tempfile::Builder::new()
        .prefix("ec-policy-")
        .tempdir_in("/tmp")
        .unwrap();
    let repo = tempfile::tempdir().unwrap();
    let nested = repo.path().join("nested");
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    std::fs::create_dir_all(nested.join(".git")).unwrap();
    std::fs::write(repo.path().join(".elsecall.toml"), "nonsense = true").unwrap();
    processes::workload_policy(home.path(), None, "[environment]\nnames = ['GLOBAL_CI']\n");
    processes::workload_policy(
        home.path(),
        Some(repo.path()),
        "[environment]\nnames = ['APP_TOKEN']\n",
    );
    let paths = Paths::for_home(home.path()).unwrap();
    assert_eq!(
        workload_configuration::load(&paths, repo.path())
            .unwrap()
            .forward_names,
        ["APP_TOKEN"]
    );
    assert_eq!(
        workload_configuration::load(&paths, &nested)
            .unwrap()
            .forward_names,
        ["GLOBAL_CI"]
    );
    let output = processes::command(env!("CARGO_BIN_EXE_elsecall"))
        .env("HOME", home.path())
        .env("ELSECALL", "off")
        .current_dir(repo.path())
        .args(["run", "--", "/bin/echo", "global-policy"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"global-policy\n");
}

#[test]
fn selected_policy_digest_distinguishes_directory_overrides_without_revealing_paths() {
    let home = tempfile::Builder::new()
        .prefix("ec-policy-")
        .tempdir_in("/tmp")
        .unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    processes::workload_policy(
        home.path(),
        Some(first.path()),
        "[[labels]]\nmatch = '*'\nplacement = 'placeable'\n",
    );
    processes::workload_policy(
        home.path(),
        Some(second.path()),
        "[[labels]]\nmatch = '*'\nplacement = 'pinned'\n",
    );
    let paths = Paths::for_home(home.path()).unwrap();
    let text = std::fs::read_to_string(paths.data.join(FILE_NAME)).unwrap();
    let configuration = workload_configuration::Configuration::parse(&text).unwrap();
    assert_ne!(
        configuration.digest(first.path()).unwrap(),
        configuration.digest(second.path()).unwrap()
    );
    let moved = tempfile::tempdir().unwrap();
    processes::workload_policy(
        home.path(),
        Some(moved.path()),
        "[[labels]]\nmatch = '*'\nplacement = 'placeable'\n",
    );
    let text = std::fs::read_to_string(paths.data.join(FILE_NAME)).unwrap();
    let configuration = workload_configuration::Configuration::parse(&text).unwrap();
    assert_eq!(
        configuration.digest(first.path()).unwrap(),
        configuration.digest(moved.path()).unwrap()
    );
}
