//! The root steps of installing Xcode, and the one-time grant that lets this
//! account take them without a password.
//!
//! Placing an Xcode bundle, selecting it and accepting its license need root.
//! Elsecall never gets a general root shell for them: a person installs, once
//! per member, a helper owned by root ([`HELPER`], `xcode-admin.sh`, rendered
//! for this account) and a sudoers drop-in ([`DROP_IN`]) that lets exactly this
//! account run exactly that helper with `place`, `select` or `license` and a
//! version of two or three numbers, without a password, and nothing else. sudo
//! checks the arguments against that pattern before the helper runs, and the
//! helper checks them again.
//!
//! The account may be an administrator, and administrators may rename anything
//! in `/Applications`. So bundles live in [`STORE`], a tree only root writes,
//! the helper never reads through anything the account could swap, and root
//! writes nowhere else: the link from `/Applications` is the account's. It re-runs itself with an empty
//! environment, copies the staged bundle (never moves it, so the account keeps
//! no handle on what root then runs) into a root-only folder without extended
//! attributes or ACLs, with the flags an account can set cleared (not every
//! flag: clearing `compressed` breaks Xcode's files), makes it root's and not writable by
//! anyone else, strips set-ID bits, and places it only once `codesign`
//! verifies it, deeply and strictly, as Apple's Xcode and the bundle says it is
//! that version, [`MINIMUM`] or later, and its license is accepted: a placed
//! Xcode is a ready one. It selects and licenses only bundles it placed, and
//! runs first-launch tasks, which install system packages, only for the newest
//! it placed.
use base64::{Engine, engine::general_purpose::STANDARD};
use elsecall_contract::failure::{Failure, Result};
use serde::Serialize;
use std::{fmt::Write as _, os::unix::fs::MetadataExt, path::Path};

/// Where the helper is installed: root's, in a folder only root writes.
pub(crate) const HELPER: &str = "/Library/PrivilegedHelperTools/dev.elsecall.xcode-admin";
/// The sudoers drop-in, read by the `#includedir` macOS's sudoers ends with.
pub(crate) const DROP_IN: &str = "/private/etc/sudoers.d/elsecall";
/// Where placed bundles live: under `/Library`, which only root writes.
pub(crate) const STORE: &str = "/Library/Elsecall/Xcodes";
/// The oldest Xcode the helper places: an old release's first-launch packages
/// would replace newer system ones.
pub(crate) const MINIMUM: &str = "16.0";
/// The arguments sudo lets the account pass: an action and a version of two or
/// three numbers. `[.]`, not `\.`: sudoers removes backslashes from a word.
pub(crate) const ARGUMENTS: &str = r"^(place|select|license) [0-9]+[.][0-9]+([.][0-9]+)?$";

const TEMPLATE: &str = include_str!("xcode_privileges/xcode-admin.sh");

/// Where the helper's paths and programs come from. Production uses the real
/// ones; tests point them at a scratch folder and fakes.
pub(crate) struct Layout<'a> {
    pub(crate) store: &'a str,
    pub(crate) owner: &'a str,
    pub(crate) account: &'a str,
    /// A shell expression printing the staging folder of `$user`.
    pub(crate) staging: &'a str,
    pub(crate) codesign: &'a str,
    pub(crate) xcode_select: &'a str,
}

/// The real layout, for `account`.
pub(crate) fn production(account: &str) -> Layout<'_> {
    Layout {
        store: STORE,
        owner: "root:wheel",
        account,
        staging: r#"home=$(dscl . -read "/Users/$user" NFSHomeDirectory | sed -n 's/^NFSHomeDirectory: //p')
	[ -n "$home" ] || fail "$user has no home folder" 67
	printf '%s\n' "$home/Library/Application Support/Elsecall/xcode-staging""#,
        codesign: "/usr/bin/codesign",
        xcode_select: "/usr/bin/xcode-select",
    }
}

/// The helper as `layout` places it.
pub(crate) fn helper(layout: &Layout<'_>) -> String {
    TEMPLATE
        .replace("@STORE@", layout.store)
        .replace("@OWNER@", layout.owner)
        .replace("@ACCOUNT@", layout.account)
        .replace("@MINIMUM@", MINIMUM)
        .replace("@STAGING@", layout.staging)
        .replace("@CODESIGN@", layout.codesign)
        .replace("@XCODE_SELECT@", layout.xcode_select)
}

/// The drop-in for `account`: one rule, for that account, as root, without a
/// password, for the helper with [`ARGUMENTS`] only. None for a name sudoers
/// could read as anything but one account.
pub(crate) fn drop_in(account: &str) -> Option<String> {
    let valid = !account.is_empty()
        && account.len() <= 32
        && account
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && account
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'));
    if !valid {
        return None;
    }
    let mut text = String::new();
    let _ = writeln!(
        text,
        "# Installed by elsecall tools privileges install. Lets {account} place, select and"
    );
    let _ = writeln!(
        text,
        "# license Xcode through Elsecall's helper, and nothing else."
    );
    let _ = writeln!(
        text,
        "{account} ALL = (root) NOPASSWD: {HELPER} {ARGUMENTS}"
    );
    Some(text)
}

/// Whether this account can take the root steps: the helper is in place, as
/// this build writes it and root's, and a drop-in is there. The drop-in is
/// root's alone to read, so only its presence is known here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Status {
    pub(crate) account: String,
    pub(crate) helper: HelperState,
    pub(crate) drop_in: DropInState,
    pub(crate) ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HelperState {
    Current,
    /// Another build wrote it, or it is not root's.
    Stale,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DropInState {
    Present,
    Missing,
}

/// This account's status.
pub(crate) fn status() -> Result<Status> {
    let account = account()?;
    let helper = match std::fs::symlink_metadata(HELPER) {
        Err(_) => HelperState::Missing,
        Ok(metadata) => {
            let current = metadata.is_file()
                && metadata.uid() == 0
                && metadata.mode() & 0o022 == 0
                && std::fs::read(HELPER)
                    .is_ok_and(|bytes| bytes == helper(&production(&account)).as_bytes());
            if current {
                HelperState::Current
            } else {
                HelperState::Stale
            }
        }
    };
    let drop_in = match std::fs::symlink_metadata(DROP_IN) {
        Ok(metadata) if metadata.is_file() && metadata.uid() == 0 => DropInState::Present,
        _ => DropInState::Missing,
    };
    Ok(Status {
        ready: helper == HelperState::Current && drop_in == DropInState::Present,
        account,
        helper,
        drop_in,
    })
}

/// The account Elsecall runs as, which the drop-in names.
fn account() -> Result<String> {
    // SAFETY: getuid cannot fail; getpwuid's record is read before any other
    // call that could replace it.
    let name = unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() {
            None
        } else {
            std::ffi::CStr::from_ptr((*entry).pw_name)
                .to_str()
                .ok()
                .map(str::to_owned)
        }
    };
    name.filter(|name| drop_in(name).is_some()).ok_or_else(|| {
        Failure::new(
            "account_unnamed",
            "This account's name cannot be written in a sudoers rule.",
            "Run Elsecall from an account whose short name is letters, digits, dots, dashes and underscores.",
        )
    })
}

/// The root shell script that installs the helper and this account's
/// drop-in: each written beside its place under a name sudo and nothing else
/// reads, made root's, the drop-in checked by `visudo`, then moved into place.
pub(crate) fn install_script(account: &str) -> Result<String> {
    let rule = drop_in(account).ok_or_else(|| {
        Failure::new(
            "account_unnamed",
            "This account's name cannot be written in a sudoers rule.",
            "Run Elsecall from an account whose short name is letters, digits, dots, dashes and underscores.",
        )
    })?;
    let folder = Path::new(HELPER).parent().expect("the helper has a folder");
    Ok(format!(
        r#"set -eu
umask 022
folder='{folder}'
# A folder that exists keeps its mode; it must be root's alone.
if [ ! -d "$folder" ]; then
  mkdir "$folder"
  chown root:wheel "$folder"
  chmod 755 "$folder"
fi
if [ -L "$folder" ] || [ "$(stat -f %u "$folder")" != 0 ]; then echo "$folder is not root's" >&2; exit 1; fi
case "$(stat -f %Sp "$folder")" in ?????w???? | ????????w?) echo "$folder is writable by others" >&2; exit 1 ;; esac
helper=$(mktemp "$folder/.elsecall.XXXXXX")
printf '%s' '{helper}' | base64 -D > "$helper"
chown root:wheel "$helper"
chmod 755 "$helper"
mv -f "$helper" '{HELPER}'
rule=$(mktemp /private/etc/sudoers.d/.elsecall.XXXXXX)
printf '%s' '{rule}' | base64 -D > "$rule"
chown root:wheel "$rule"
chmod 440 "$rule"
if ! visudo -cf "$rule" >/dev/null; then rm -f "$rule"; exit 1; fi
mv -f "$rule" '{DROP_IN}'
"#,
        folder = folder.display(),
        helper = STANDARD.encode(helper(&production(account))),
        rule = STANDARD.encode(rule),
    ))
}

/// The root shell script that removes both. A bundle the helper placed stays:
/// nothing is ever uninstalled.
pub(crate) fn remove_script() -> String {
    format!("set -eu\nrm -f '{DROP_IN}' '{HELPER}'\n")
}

#[cfg(test)]
mod tests {
    use crate::xcode_privileges::*;
    use std::{
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        process::{Command, Output},
    };

    #[test]
    fn the_drop_in_names_one_account_one_helper_and_three_argument_shapes() {
        let text = drop_in("devbox").unwrap();
        let rules: Vec<&str> = text
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
            .collect();
        assert_eq!(
            rules,
            [
                "devbox ALL = (root) NOPASSWD: /Library/PrivilegedHelperTools/dev.elsecall.xcode-admin ^(place|select|license) [0-9]+[.][0-9]+([.][0-9]+)?$"
            ],
            "exactly one rule, for the helper with an action and a version only"
        );
        for account in [
            "",
            "root ALL",
            "a,b",
            "-x",
            "%admin",
            "me\nroot",
            &"a".repeat(33),
        ] {
            assert!(drop_in(account).is_none(), "{account:?}");
        }
        let scratch = tempfile::tempdir().unwrap();
        let file = scratch.path().join("elsecall");
        std::fs::write(&file, &text).unwrap();
        let checked = Command::new("/usr/sbin/visudo")
            .args(["-c", "-f"])
            .arg(&file)
            .output()
            .unwrap();
        assert!(checked.status.success(), "{checked:?}");
    }

    #[test]
    fn the_install_script_writes_root_owned_files_through_visudo() {
        let script = install_script("devbox").unwrap();
        assert!(script.contains("visudo -cf"));
        assert!(script.contains("chmod 440"));
        assert!(script.contains(DROP_IN) && script.contains(HELPER));
        let syntax = Command::new("/bin/sh")
            .args(["-n", "-c", &script])
            .output()
            .unwrap();
        assert!(syntax.status.success(), "{syntax:?}");
        assert!(install_script("root ALL").is_err());
    }

    /// A scratch Applications folder, a staging folder and fakes for codesign,
    /// xcode-select and an Xcode's own xcodebuild.
    struct Fixture {
        root: tempfile::TempDir,
        helper: PathBuf,
    }

    impl Fixture {
        fn new(signature_valid: bool) -> Self {
            let root = tempfile::tempdir().unwrap();
            let base = root.path();
            for folder in ["Applications", "staging", "bin", "Library"] {
                std::fs::create_dir(base.join(folder)).unwrap();
            }
            let fake = |name: &str, body: &str| {
                let path = base.join("bin").join(name);
                std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
                path
            };
            let log = base.join("log");
            let codesign = fake(
                "codesign",
                &format!(
                    "echo \"codesign $*\" >> '{}'; exit {}",
                    log.display(),
                    u8::from(!signature_valid)
                ),
            );
            let select = fake(
                "xcode-select",
                &format!("echo \"xcode-select $*\" >> '{}'", log.display()),
            );
            let owner = format!(
                "{}:{}",
                String::from_utf8(
                    Command::new("/usr/bin/id")
                        .arg("-un")
                        .output()
                        .unwrap()
                        .stdout
                )
                .unwrap()
                .trim(),
                String::from_utf8(
                    Command::new("/usr/bin/id")
                        .arg("-gn")
                        .output()
                        .unwrap()
                        .stdout
                )
                .unwrap()
                .trim(),
            );
            let store = base.join("Library/Xcodes").display().to_string();
            let staging = format!("printf '%s\\n' '{}'", base.join("staging").display());
            let layout = Layout {
                store: &store,
                owner: &owner,
                account: "devbox",
                staging: &staging,
                codesign: &codesign.display().to_string(),
                xcode_select: &select.display().to_string(),
            };
            let helper = base.join("xcode-admin");
            std::fs::write(&helper, super::helper(&layout)).unwrap();
            Self { root, helper }
        }

        fn stage(&self, version: &str, plist_version: &str) -> PathBuf {
            let bundle = self
                .root
                .path()
                .join(format!("staging/Xcode-{version}.app"));
            let contents = bundle.join("Contents");
            std::fs::create_dir_all(contents.join("Developer/usr/bin")).unwrap();
            std::fs::write(
                contents.join("Info.plist"),
                format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>com.apple.dt.Xcode</string><key>CFBundleShortVersionString</key><string>{plist_version}</string></dict></plist>"),
            )
            .unwrap();
            let xcodebuild = contents.join("Developer/usr/bin/xcodebuild");
            std::fs::write(
                &xcodebuild,
                format!(
                    "#!/bin/sh\necho \"xcodebuild $*\" >> '{}'\n",
                    self.root.path().join("log").display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&xcodebuild, std::fs::Permissions::from_mode(0o775)).unwrap();
            let tool = contents.join("Developer/usr/bin/setid");
            std::fs::write(&tool, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o4777)).unwrap();
            bundle
        }

        fn run(&self, args: &[&str]) -> Output {
            Command::new("/bin/sh")
                .arg(&self.helper)
                .args(args)
                .env("SUDO_USER", "devbox")
                .output()
                .unwrap()
        }

        fn placed(&self, version: &str) -> PathBuf {
            self.root
                .path()
                .join(format!("Library/Xcodes/Xcode-{version}.app"))
        }

        fn link(&self, version: &str) -> PathBuf {
            self.root
                .path()
                .join(format!("Applications/Xcode-{version}.app"))
        }

        fn log(&self) -> String {
            std::fs::read_to_string(self.root.path().join("log")).unwrap_or_default()
        }
    }

    #[test]
    fn the_helper_refuses_arguments_outside_its_shapes() {
        let fixture = Fixture::new(true);
        for args in [
            &["place", "26"][..],
            &["place", "26.2.x"],
            &["place", "../26.2"],
            &["remove", "26.2"],
            &["place"],
            &["place", "26.2", "x"],
            &["place", "26..2"],
            &["place", "15.4"],
            &["license", "13.0"],
            &["--clean", "place"],
        ] {
            let ran = fixture.run(args);
            assert!(matches!(ran.status.code(), Some(64)), "{args:?}: {ran:?}");
        }
        assert!(fixture.log().is_empty(), "nothing ran");
    }

    #[test]
    fn the_helper_places_only_a_verified_bundle_as_a_hardened_copy() {
        let fixture = Fixture::new(true);
        let staged = fixture.stage("26.2", "26.2");
        let placed = fixture.run(&["place", "26.2"]);
        assert!(placed.status.success(), "{placed:?}");
        let bundle = fixture.placed("26.2");
        assert!(bundle.join("Contents/Info.plist").is_file());
        assert!(
            staged.exists(),
            "a copy: the staged bundle is the account's to remove"
        );
        let mode = |path: PathBuf| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            mode(bundle.join("Contents/Developer/usr/bin/setid")),
            0o755,
            "set-ID and others' write bits are stripped"
        );
        assert_eq!(
            mode(bundle.join("Contents/Developer/usr/bin/xcodebuild")),
            0o755
        );
        assert!(
            fixture
                .log()
                .contains("codesign --verify --deep --strict -R=anchor apple and identifier \"com.apple.dt.Xcode\""),
            "{}",
            fixture.log()
        );
        let leftovers: Vec<_> = std::fs::read_dir(fixture.root.path().join("Library/Xcodes"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["Xcode-26.2.app"], "no working folder is left");
        assert!(
            !fixture.link("26.2").exists(),
            "root writes nothing outside the store"
        );
        assert_eq!(
            fixture.run(&["place", "26.2"]).status.code(),
            Some(73),
            "never over an existing bundle"
        );

        assert!(fixture.run(&["license", "26.2"]).status.success());
        assert!(fixture.run(&["select", "26.2"]).status.success());
        let log = fixture.log();
        assert!(
            log.contains("xcodebuild -license accept\nxcodebuild -runFirstLaunch\n"),
            "{log}"
        );
        assert!(
            log.contains(&format!("xcode-select --switch {}", bundle.display())),
            "{log}"
        );
    }

    #[test]
    fn first_launch_runs_only_for_the_newest_placed_and_one_placing_at_a_time() {
        let fixture = Fixture::new(true);
        fixture.stage("26.2", "26.2");
        fixture.stage("26.10", "26.10");
        let lock = fixture.root.path().join("Library/Xcodes/.placing-26.2");
        std::fs::create_dir_all(&lock).unwrap();
        assert_eq!(fixture.run(&["place", "26.2"]).status.code(), Some(75));
        assert!(lock.exists(), "another placing's lock is left alone");
        std::fs::remove_dir(&lock).unwrap();
        let first_launches = || fixture.log().matches("-runFirstLaunch").count();
        assert!(fixture.run(&["place", "26.10"]).status.success());
        assert!(
            !lock.with_file_name(".placing-26.10").exists(),
            "a placing removes its lock"
        );
        assert_eq!(
            fixture.log().matches("-license accept").count(),
            1,
            "placed means licensed"
        );
        assert_eq!(first_launches(), 1, "the newest so far");
        assert!(fixture.run(&["place", "26.2"]).status.success());
        assert_eq!(
            first_launches(),
            1,
            "26.2 is older than 26.10: {}",
            fixture.log()
        );
        assert!(fixture.run(&["license", "26.2"]).status.success());
        assert_eq!(first_launches(), 1);
        assert!(fixture.run(&["license", "26.10"]).status.success());
        assert_eq!(first_launches(), 2);
    }

    #[test]
    fn the_helper_runs_with_none_of_the_callers_environment() {
        let fixture = Fixture::new(true);
        fixture.stage("26.2", "26.2");
        assert!(fixture.run(&["place", "26.2"]).status.success());
        let xcodebuild = fixture
            .placed("26.2")
            .join("Contents/Developer/usr/bin/xcodebuild");
        std::fs::set_permissions(&xcodebuild, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            &xcodebuild,
            format!(
                "#!/bin/sh\nenv | sort >> '{}'\n",
                fixture.root.path().join("log").display()
            ),
        )
        .unwrap();
        let ran = Command::new("/bin/sh")
            .arg(&fixture.helper)
            .args(["license", "26.2"])
            .env("HOME", "/Users/devbox")
            .env("DEVELOPER_DIR", "/tmp/elsewhere")
            .env("SSH_AUTH_SOCK", "/tmp/agent")
            .output()
            .unwrap();
        assert!(ran.status.success(), "{ran:?}");
        let log = fixture.log();
        assert!(log.contains("HOME=/var/root"), "{log}");
        for kept in ["devbox", "DEVELOPER_DIR", "SSH_AUTH_SOCK"] {
            assert!(
                !log.contains(kept),
                "{kept} reached the helper's commands: {log}"
            );
        }
    }

    #[test]
    fn the_helper_places_nothing_unverified_and_runs_nothing_it_did_not_place() {
        let unsigned = Fixture::new(false);
        unsigned.stage("26.2", "26.2");
        assert_eq!(unsigned.run(&["place", "26.2"]).status.code(), Some(65));
        assert!(!unsigned.placed("26.2").exists());

        let mislabelled = Fixture::new(true);
        mislabelled.stage("26.2", "26.3");
        assert_eq!(mislabelled.run(&["place", "26.2"]).status.code(), Some(65));
        assert!(!mislabelled.placed("26.2").exists());

        let fixture = Fixture::new(true);
        assert_eq!(
            fixture.run(&["place", "26.2"]).status.code(),
            Some(66),
            "nothing staged"
        );
        assert_eq!(
            fixture.run(&["select", "26.2"]).status.code(),
            Some(66),
            "nothing placed"
        );
        fixture.stage("26.2", "26.2");
        assert!(fixture.run(&["place", "26.2"]).status.success());
        let bundle = fixture.placed("26.2");
        std::fs::set_permissions(&bundle, std::fs::Permissions::from_mode(0o757)).unwrap();
        assert_eq!(
            fixture.run(&["select", "26.2"]).status.code(),
            Some(77),
            "writable by others"
        );
        std::fs::set_permissions(&bundle, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store = fixture.root.path().join("Library/Xcodes");
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            fixture.run(&["license", "26.2"]).status.code(),
            Some(77),
            "a store others may write to"
        );
        assert_eq!(
            fixture.log().matches("xcodebuild -license accept").count(),
            1,
            "only the one placing accepted a license: {}",
            fixture.log()
        );
    }

    #[test]
    fn a_license_that_fails_leaves_nothing_placed() {
        let fixture = Fixture::new(true);
        let staged = fixture.stage("26.2", "26.2");
        std::fs::write(
            staged.join("Contents/Developer/usr/bin/xcodebuild"),
            "#!/bin/sh\nexit 1\n",
        )
        .unwrap();
        assert_eq!(fixture.run(&["place", "26.2"]).status.code(), Some(70));
        assert!(
            !fixture.placed("26.2").exists(),
            "the next try places it again"
        );
        let left: Vec<_> = std::fs::read_dir(fixture.root.path().join("Library/Xcodes"))
            .unwrap()
            .collect();
        assert!(left.is_empty(), "no working folder or lock is left");
    }
}
