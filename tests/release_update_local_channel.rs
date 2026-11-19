#![cfg(target_os = "macos")]
//! The local release channel with real curl, tar and packslip: releases are signed
//! here with throwaway local keys and read from a directory or a loopback server.
//! The only fakes are the release's executables, which report a version.
use elsecall_host::paths::Paths;
use elsecall_lifecycle::release_update::{
    Release, ReleaseCatalog, ReleaseDownload, ReleaseVersion,
    local_channel::{self, LocalChannel, LocalDownload, Packslip},
};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
};

/// The packslip CLI that `mise.toml` pins. Tests fail rather than skip without it.
fn packslip() -> PathBuf {
    if let Some(path) = std::env::var_os("EC_TEST_PACKSLIP") {
        return path.into();
    }
    let found = Command::new("mise")
        .args(["which", "packslip"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| PathBuf::from(String::from_utf8(output.stdout).unwrap().trim()));
    found.expect("these tests need packslip: install mise.toml's tools, or set EC_TEST_PACKSLIP")
}

fn run(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Fixture {
    root: tempfile::TempDir,
    packslip: PathBuf,
    paths: Paths,
    key: PathBuf,
    public: String,
    listed: Vec<String>,
}
struct Publish<'a> {
    version: &'a str,
    reports: &'a str,
    project: &'a str,
    signed_as: &'a str,
    key: &'a Path,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("ec-local-channel-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = root.path().join("home");
        fs::create_dir(&home).unwrap();
        let packslip = packslip();
        let key = Self::keygen(&packslip, root.path(), "channel");
        let public = Self::public(&key);
        Self {
            paths: Paths::for_home(&home).unwrap(),
            root,
            packslip,
            key,
            public,
            listed: Vec::new(),
        }
    }
    fn keygen(packslip: &Path, root: &Path, name: &str) -> PathBuf {
        let key = root.join(format!("{name}.key"));
        run(Command::new(packslip).arg("keygen").arg("--out").arg(&key));
        key
    }
    fn public(key: &Path) -> String {
        let text = fs::read_to_string(key.with_extension("pub")).unwrap();
        text.lines().last().unwrap().to_owned()
    }
    fn channel_dir(&self) -> PathBuf {
        self.root.path().join("channel")
    }
    fn channel(&self) -> LocalChannel {
        LocalChannel::new(
            &format!("file://{}/", self.channel_dir().display()),
            &self.public,
        )
        .unwrap()
    }
    fn download(&self) -> LocalDownload {
        LocalDownload {
            paths: self.paths.clone(),
            channel: self.channel(),
            packslip: Packslip::At(self.packslip.clone()),
        }
    }
    fn release(&mut self, version: &str) {
        let key = self.key.clone();
        self.publish(Publish {
            version,
            reports: version,
            project: local_channel::PROJECT,
            signed_as: version,
            key: &key,
        });
    }
    /// Builds, signs and lists one release as scripts/local-release.sh does.
    fn publish(&mut self, release: Publish<'_>) {
        let content = self
            .root
            .path()
            .join(format!("content-{}", release.version));
        fs::create_dir(&content).unwrap();
        write_executable(
            &content.join("elsecall"),
            &format!("#!/bin/sh\necho elsecall {}\n", release.reports),
        );
        write_executable(&content.join("elsecall-ui"), "#!/bin/sh\nexit 0\n");
        let folder = self.channel_dir().join(format!("v{}", release.version));
        fs::create_dir_all(&folder).unwrap();
        let archive = folder.join(format!(
            "elsecall-v{}-aarch64-apple-darwin.tar.gz",
            release.version
        ));
        run(Command::new("/usr/bin/tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&content)
            .args(["elsecall", "elsecall-ui"]));
        run(Command::new(&self.packslip)
            .args(["create", "--project", release.project])
            .args(["--version", release.signed_as])
            .args(["--bin", "elsecall", "--bin", "elsecall-ui"])
            .arg("--key")
            .arg(release.key)
            .args(["--no-log", "--no-libs", "--out"])
            .arg(&folder)
            .arg(format!("{}:macos/aarch64", archive.display())));
        self.listed.push(release.version.to_owned());
        fs::write(
            self.channel_dir().join("releases.json"),
            serde_json::to_vec(&self.listed).unwrap(),
        )
        .unwrap();
    }
    fn stage(
        &self,
        version: &str,
    ) -> elsecall_contract::failure::Result<elsecall_lifecycle::release_update::StagedRelease> {
        let stage = self
            .paths
            .home
            .join(".local/share/elsecall/releases")
            .join(version);
        elsecall_host::private_files::owned_tree(&self.paths.home, &stage).unwrap();
        self.download().stage(&release(version), &stage)
    }
}
fn write_executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
fn release(version: &str) -> Release {
    Release {
        version: ReleaseVersion::try_from(version.to_owned()).unwrap(),
    }
}

/// Serves a directory over HTTP on loopback, one request per connection.
fn serve(directory: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).unwrap();
            let line = String::from_utf8_lossy(&request[..read]);
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let file = directory.join(path.trim_start_matches('/'));
            let response = match fs::read(&file).ok().filter(|_| !path.contains("..")) {
                Some(body) => {
                    let mut response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    response.extend(body);
                    response
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            };
            let _ = stream.write_all(&response);
        }
    });
    format!("http://{address}")
}

#[test]
fn catalog_offers_the_newest_release_from_a_directory_or_a_server() {
    let mut fixture = Fixture::new();
    fixture.release("0.1.1");
    fixture.release("0.1.3");
    fixture.release("0.1.2");
    assert_eq!(fixture.channel().latest().unwrap(), Some(release("0.1.3")));
    let served = LocalChannel::new(&serve(fixture.channel_dir()), &fixture.public).unwrap();
    assert_eq!(served.latest().unwrap(), Some(release("0.1.3")));

    let missing = LocalChannel::new(
        &format!("file://{}", fixture.root.path().join("absent").display()),
        &fixture.public,
    )
    .unwrap();
    assert_eq!(
        missing.latest().unwrap_err().code,
        "local_release_unreachable"
    );
}

#[test]
fn download_unpacks_a_release_the_pinned_key_verifies() {
    let mut fixture = Fixture::new();
    fixture.release("0.1.2");
    let staged = fixture.stage("0.1.2").unwrap();
    let stage = fixture
        .paths
        .home
        .join(".local/share/elsecall/releases/0.1.2");
    assert!(staged.executable.starts_with(&stage));
    assert_eq!(
        staged.window,
        staged.executable.with_file_name("elsecall-ui")
    );
    let reported = Command::new(&staged.executable)
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&reported.stdout).trim(),
        "elsecall 0.1.2"
    );

    // The same release served over HTTP verifies the same way.
    let download = LocalDownload {
        channel: LocalChannel::new(&serve(fixture.channel_dir()), &fixture.public).unwrap(),
        ..fixture.download()
    };
    let served = fixture
        .paths
        .home
        .join(".local/share/elsecall/releases/served");
    elsecall_host::private_files::owned_tree(&fixture.paths.home, &served).unwrap();
    let staged_again = download.stage(&release("0.1.2"), &served).unwrap();
    assert_eq!(staged_again.digest, staged.digest);
}

#[test]
fn download_refuses_what_the_pinned_key_does_not_vouch_for() {
    let mut fixture = Fixture::new();
    let stranger = Fixture::keygen(&fixture.packslip, fixture.root.path(), "stranger");
    let key = fixture.key.clone();
    let signed = |version, reports, project, signed_as, key| Publish {
        version,
        reports,
        project,
        signed_as,
        key,
    };
    let project = local_channel::PROJECT;
    fixture.publish(signed("0.2.1", "0.2.1", project, "0.2.1", &stranger));
    fixture.publish(signed("0.2.2", "0.2.2", "other.localhost", "0.2.2", &key));
    fixture.publish(signed("0.2.3", "0.2.3", project, "0.2.9", &key));
    fixture.publish(signed("0.2.4", "0.2.4", project, "0.2.4", &key));
    fixture.publish(signed("0.2.5", "9.0.0", project, "0.2.5", &key));
    // The archive is swapped after it was signed.
    let tampered = fixture
        .channel_dir()
        .join("v0.2.4/elsecall-v0.2.4-aarch64-apple-darwin.tar.gz");
    let mut bytes = fs::read(&tampered).unwrap();
    bytes.push(0);
    fs::write(&tampered, bytes).unwrap();

    for (version, code, case) in [
        (
            "0.2.1",
            "local_release_unverified",
            "signed with another key",
        ),
        (
            "0.2.2",
            "local_release_unverified",
            "signed for another project",
        ),
        (
            "0.2.3",
            "local_release_unverified",
            "signed as another version",
        ),
        (
            "0.2.4",
            "local_release_unverified",
            "archive changed after signing",
        ),
        (
            "0.2.5",
            "release_update_failed",
            "executable is another version",
        ),
        (
            "0.2.6",
            "local_release_unreachable",
            "release not in the channel",
        ),
    ] {
        let error = fixture.stage(version).unwrap_err();
        assert_eq!(error.code, code, "{case}");
    }
}

/// mise is the boundary here: a stand-in that installs nothing and runs the real
/// packslip for `mise exec`, after checking it was called as the updater must.
fn fake_mise(fixture: &Fixture) {
    let bin = fixture.paths.home.join(".local/bin");
    fs::create_dir_all(&bin).unwrap();
    write_executable(
        &bin.join("mise"),
        &format!(
            r#"#!/bin/sh
[ -f "$MISE_GLOBAL_CONFIG_FILE" ] && [ ! -s "$MISE_GLOBAL_CONFIG_FILE" ] && [ -n "$MISE_CEILING_PATHS" ] || exit 9
case "$1" in
install) [ ! -e "$HOME/offline" ] && [ "$2" = "{packslip}" ] ;;
exec) [ "$2" = "{packslip}" ] && [ "$3" = -- ] && [ "$4" = packslip ] || exit 8; shift 4; exec "{binary}" "$@" ;;
*) exit 7 ;;
esac
"#,
            packslip = local_channel::PACKSLIP,
            binary = fixture.packslip.display()
        ),
    );
}

#[test]
fn the_updater_runs_the_pinned_packslip_through_mise_and_names_a_missing_verifier() {
    let mut fixture = Fixture::new();
    fixture.release("0.3.1");
    fixture.release("0.3.2");
    fake_mise(&fixture);
    let through_mise = |version: &str| {
        let stage = fixture
            .paths
            .home
            .join(".local/share/elsecall/releases")
            .join(version);
        elsecall_host::private_files::owned_tree(&fixture.paths.home, &stage).unwrap();
        LocalDownload {
            packslip: Packslip::Mise,
            ..fixture.download()
        }
        .stage(&release(version), &stage)
    };
    let staged = through_mise("0.3.1").unwrap();
    assert!(staged.executable.is_file());
    // A verifier that can't be installed is reported as such, not as a bad release.
    fs::write(fixture.paths.home.join("offline"), b"").unwrap();
    assert_eq!(
        through_mise("0.3.2").unwrap_err().code,
        "local_release_verifier_unavailable"
    );
}

#[test]
fn channel_settings_name_a_url_and_a_minisign_public_key() {
    let fixture = Fixture::new();
    for location in [
        "ftp://example.com/channel",
        "/srv/channel",
        "file://relative",
        "http://",
        "http://example.com/a b",
    ] {
        assert_eq!(
            LocalChannel::new(location, &fixture.public)
                .unwrap_err()
                .code,
            "local_release_channel_invalid",
            "{location}"
        );
    }
    for key in ["", "not base64!", "RWQ=", &fixture.public[..20]] {
        assert_eq!(
            LocalChannel::new("http://192.168.64.1:8765", key)
                .unwrap_err()
                .code,
            "local_release_channel_invalid",
            "{key}"
        );
    }
    assert_eq!(
        LocalChannel::new("https://example.com/channel/", &fixture.public)
            .unwrap()
            .location(),
        "https://example.com/channel"
    );
}
