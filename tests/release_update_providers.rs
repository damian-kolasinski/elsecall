#![cfg(target_os = "macos")]
//! Controlled external mise executable: verification policy, isolation, and config preservation.
use elsecall::{
    paths::Paths,
    private_files,
    release_update::{
        self, Release, ReleaseDownload, ReleaseVersion,
        download::MiseDownload,
        selection::{CliSelection, MiseSelection},
    },
};
use std::{fs, os::unix::fs::PermissionsExt};
fn fixture() -> (tempfile::TempDir, Paths) {
    let home = tempfile::Builder::new()
        .prefix("ec-release-tools-")
        .tempdir_in("/tmp")
        .unwrap();
    let paths = Paths::for_home(home.path()).unwrap();
    private_files::owned_tree(&paths.home, &paths.home.join(".local/bin")).unwrap();
    let script = paths.home.join(".local/bin/mise");
    fs::write(
        &script,
        b"#!/bin/sh\nexec /usr/bin/python3 \"$HOME/fake-mise.py\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(paths.home.join("fake-mise.py"),r#"
import os, sys, json
from pathlib import Path
home=Path(os.environ['HOME']); args=sys.argv[1:]
if args[0] in ['trust','install','which']:
    root=Path(os.environ['MISE_CONFIG_FILE']).parent
    assert os.environ['MISE_GLOBAL_CONFIG_FILE']==str(root/'empty.toml')
    assert os.environ['MISE_CONFIG_DIR']==str(root/'config')
    assert os.environ['MISE_STATE_DIR']==str(root/'state')
    assert os.environ['MISE_CACHE_DIR']==str(root/'cache')
    assert os.environ['MISE_CEILING_PATHS']==str(root.parent.resolve())
    allowed={'MISE_CONFIG_FILE','MISE_GLOBAL_CONFIG_FILE','MISE_CONFIG_DIR','MISE_STATE_DIR','MISE_CACHE_DIR','MISE_DATA_DIR','MISE_CEILING_PATHS','MISE_YES'}
    assert all(not key.startswith('MISE_') or key in allowed for key in os.environ)
    config=(root/'mise.toml').read_text()
    assert 'identity = "https://github.com/damian-kolasinski/elsecall/.github/workflows/release.yml@refs/tags/v0.1.1"' in config
    assert 'issuer = "https://token.actions.githubusercontent.com"' in config
    if args[0]=='install':
        assert args==['install','packslip:github.com/damian-kolasinski/elsecall@0.1.1']
        if (home/'real-mise').exists():
            import subprocess
            probe=subprocess.run([(home/'real-mise').read_text(),'config','ls','--json'],capture_output=True)
            (home/'real-mise-output').write_bytes(probe.stdout+probe.stderr)
            assert probe.returncode==0
            configs=json.loads(probe.stdout)
            assert all(Path(item['path']).resolve().parent==root.resolve() for item in configs), configs
            assert any(Path(item['path']).resolve()==(root/'mise.toml').resolve() for item in configs), configs
        if (home/'reject').exists(): sys.exit(1)
        install=Path(os.environ['MISE_DATA_DIR'])/'installs/release'; install.mkdir(parents=True)
        binary=install/'elsecall'
        binary.write_text('#!/bin/sh\necho elsecall '+('9.0.0' if (home/'wrong-version').exists() else '0.1.1')+'\n'); binary.chmod(0o700)
        (install/'elsecall-ui').write_bytes(b'window bytes')
        if not (home/'unverified').exists(): (install/'.mise-packslip.json').write_text('{}')
    elif args[0]=='which': print(root/'data/installs/release/elsecall')
elif args[0]=='ls':
    if '--installed' in args:
        print(json.dumps([{'version':'0.1.1','install_path':(home/'linked').read_text()}] if (home/'linked').exists() else []))
    else: print(json.dumps([{'active':True,'source':{'path':str(home/'.config/mise/config.toml')}}]))
elif args[0]=='where':
    if not (home/'linked').exists(): sys.exit(1)
    print((home/'linked').read_text())
elif args[0]=='link': (home/'linked').write_text(args[2])
else: sys.exit(1)
"#).unwrap();
    (home, paths)
}
fn release() -> Release {
    Release {
        version: ReleaseVersion::try_from("0.1.1".to_owned()).unwrap(),
    }
}
#[test]
fn signed_download_is_isolated_and_requires_verified_metadata_and_the_expected_version() {
    for mode in ["valid", "reject", "unverified", "wrong-version"] {
        let (_home, paths) = fixture();
        if mode != "valid" {
            fs::write(paths.home.join(mode), b"").unwrap();
        }
        let stage = paths.home.join("stage");
        private_files::directory(&stage).unwrap();
        let result = MiseDownload { paths }.stage(&release(), &stage);
        assert_eq!(result.is_ok(), mode == "valid", "{mode}");
    }
}
#[test]
#[ignore = "set EC_TEST_MISE to an installed mise binary; no downloads are performed"]
fn real_mise_discovery_excludes_parent_configs_but_loads_the_signed_release_config() {
    let mise = std::env::var("EC_TEST_MISE").expect("EC_TEST_MISE");
    let (_home, paths) = fixture();
    fs::write(paths.home.join("real-mise"), mise).unwrap();
    fs::write(paths.home.join("mise.toml"), "[tools]\njq = \"1.7\"\n").unwrap();
    fs::write(paths.home.join(".tool-versions"), "node 22.0.0\n").unwrap();
    let stage = paths.home.join("releases/job");
    private_files::owned_tree(&paths.home, &stage).unwrap();
    let result = MiseDownload {
        paths: paths.clone(),
    }
    .stage(&release(), &stage);
    assert!(
        result.is_ok(),
        "{result:?}; mise config output: {}",
        fs::read_to_string(paths.home.join("real-mise-output")).unwrap_or_default()
    );
}
#[test]
fn mise_activation_and_rollback_preserve_other_tools_options_and_concurrent_user_edits() {
    let (_home, paths) = fixture();
    let config = paths.home.join(".config/mise/config.toml");
    private_files::owned_tree(&paths.home, config.parent().unwrap()).unwrap();
    let text = format!(
        "# personal tools\n[tools]\nrust = \"1.99.0\"\n{} = {{version = \"0.1.0\", custom_option = \"kept\"}}\n[env]\nTOOL_AUDIT = \"preserved\"\n",
        serde_json::to_string(release_update::TOOL).unwrap()
    );
    fs::write(&config, &text).unwrap();
    let selection = MiseSelection {
        paths: paths.clone(),
    };
    let previous = selection.snapshot().unwrap().unwrap();
    assert_eq!(previous.version, "0.1.0");
    assert!(
        !serde_json::to_string(&previous)
            .unwrap()
            .contains("preserved")
    );
    let stage = paths.home.join("stage");
    private_files::directory(&stage).unwrap();
    let staged = MiseDownload {
        paths: paths.clone(),
    }
    .stage(&release(), &stage)
    .unwrap();
    selection.activate(&release(), &staged, &previous).unwrap();
    assert!(selection.references(&release(), &stage).unwrap());
    assert!(
        !selection
            .references(&release(), &paths.home.join(".config"))
            .unwrap()
    );
    let document: toml::Value = toml::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(
        document["tools"][release_update::TOOL]["version"].as_str(),
        Some("0.1.1")
    );
    assert_eq!(
        document["tools"][release_update::TOOL]["custom_option"].as_str(),
        Some("kept")
    );
    assert_eq!(document["env"]["TOOL_AUDIT"].as_str(), Some("preserved"));
    // Another tool's concurrent change is retained when only Elsecall is restored.
    let updated = fs::read_to_string(&config)
        .unwrap()
        .replace("1.99.0", "1.100.0");
    fs::write(&config, updated).unwrap();
    selection.restore(&release(), &previous).unwrap();
    let restored = fs::read_to_string(&config).unwrap();
    assert!(restored.contains("1.100.0"));
    assert!(restored.contains("# personal tools"));
    assert_eq!(selection.snapshot().unwrap().unwrap(), previous);
    selection.activate(&release(), &staged, &previous).unwrap();
    let edited = fs::read_to_string(&config)
        .unwrap()
        .replace("\"0.1.1\"", "\"0.3.0\"");
    fs::write(&config, &edited).unwrap();
    assert!(selection.restore(&release(), &previous).is_err());
    assert_eq!(fs::read_to_string(&config).unwrap(), edited);
}

/// The real latest release on GitHub. Not part of `mise run check`: it needs the
/// network, and the answer changes with every release.
#[test]
#[ignore]
fn live_latest_release_is_read_without_the_rate_limited_api() {
    use elsecall::release_update::{ReleaseCatalog, catalog::GithubReleases};
    let latest = GithubReleases.latest().unwrap();
    println!("{latest:?}");
    assert!(latest.is_some());
}
