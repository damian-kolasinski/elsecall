//! A root LaunchDaemon that keeps the Application Firewall letting other members
//! reach Elsecall.
//!
//! The firewall can ask whoever sits at the console "Accept incoming network
//! connections?" even for a program an administrator allowed. A console user who
//! leaves that prompt unanswered and logs out denies it: the program's rule turns
//! to Block, every member's connection is accepted and closed unanswered, and only
//! an administrator can lift it. A member that nobody watches would stay cut off.
//!
//! The guard checks the programs it was installed for every 30 seconds and allows
//! any that is not allowed. It runs only `socketfilterfw` with those paths as data,
//! never the programs themselves, which their accounts can replace: root never
//! executes anything a member's account controls. A rule already follows a
//! program's path from build to build, so the guard grants nothing the rule did
//! not.
use elsecall_failure::{Failure, Result};

pub const LABEL: &str = "dev.elsecall.firewall-guard";
pub const PLIST: &str = "/Library/LaunchDaemons/dev.elsecall.firewall-guard.plist";
const TOOL: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";
/// How often the guard looks, in seconds: a member a console logout cut off
/// answers again within this.
const INTERVAL_SECONDS: i64 = 30;

/// The check the daemon runs as root, with the programs as its arguments. It acts
/// only on a rule that blocks, which is what a denied prompt leaves; a program
/// with no rule reads as permitted. It skips anything but a regular file reached
/// without a symbolic link, since a link could aim the firewall at a program the
/// guard is not for, and a missing program, such as one an uninstall removed. The
/// log line names no path.
fn check(tool: &str) -> String {
    format!(
        r#"for program in "$@"; do
  [ -f "$program" ] && [ ! -L "$program" ] || continue
  directory=${{program%/*}}
  [ "$(cd "$directory" 2>/dev/null && pwd -P)" = "$directory" ] || continue
  case "$('{tool}' --getappblocked "$program" 2>/dev/null)" in
    *" is blocked.")
      '{tool}' --add "$program" >/dev/null 2>&1
      '{tool}' --unblockapp "$program" >/dev/null 2>&1
      /usr/bin/logger -t elsecall-firewall-guard "allowed a program's incoming connections again"
      ;;
  esac
done"#
    )
}

/// The LaunchDaemon's configuration, guarding `programs`.
pub fn configuration(programs: &[String]) -> Result<Vec<u8>> {
    configuration_with(TOOL, programs)
}

fn configuration_with(tool: &str, programs: &[String]) -> Result<Vec<u8>> {
    let mut values = plist::Dictionary::new();
    values.insert("Label".into(), LABEL.into());
    let mut arguments: Vec<plist::Value> = vec![
        "/bin/sh".into(),
        "-c".into(),
        check(tool).into(),
        "elsecall-firewall-guard".into(),
    ];
    arguments.extend(programs.iter().map(|p| p.clone().into()));
    values.insert("ProgramArguments".into(), plist::Value::Array(arguments));
    values.insert("RunAtLoad".into(), true.into());
    values.insert("StartInterval".into(), INTERVAL_SECONDS.into());
    values.insert("ProcessType".into(), "Background".into());
    values.insert("StandardOutPath".into(), "/dev/null".into());
    values.insert("StandardErrorPath".into(), "/dev/null".into());
    let mut bytes = Vec::new();
    plist::Value::Dictionary(values)
        .to_writer_xml(&mut bytes)
        .map_err(|_| {
            Failure::new(
                "firewall_guard_invalid",
                "The firewall guard's configuration could not be written.",
                "Report this; the program paths could not be encoded.",
            )
        })?;
    Ok(bytes)
}

/// A root shell script that installs `configuration` as the guard and loads it,
/// replacing an older one. It prints `installed` when it changed the guard and
/// `unchanged` when the same guard was already loaded.
pub fn install_script(configuration: &[u8]) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(configuration);
    format!(
        r#"set -e
plist='{PLIST}'
new=$(/usr/bin/mktemp /Library/LaunchDaemons/.elsecall-firewall-guard.XXXXXX)
trap '/bin/rm -f "$new"' EXIT
printf '%s' '{encoded}' | /usr/bin/base64 -D > "$new"
if /usr/bin/cmp -s "$new" "$plist" && /bin/launchctl print system/{LABEL} >/dev/null 2>&1; then
  echo unchanged
  exit 0
fi
/usr/sbin/chown root:wheel "$new"
/bin/chmod 644 "$new"
/bin/mv -f "$new" "$plist"
/bin/launchctl bootout system/{LABEL} >/dev/null 2>&1 || true
waited=0
while /bin/launchctl print system/{LABEL} >/dev/null 2>&1 && [ "$waited" -lt 100 ]; do
  /bin/sleep 0.1
  waited=$((waited + 1))
done
/bin/launchctl enable system/{LABEL}
/bin/launchctl bootstrap system "$plist"
echo installed"#
    )
}

/// Whether `install_script`'s output says it changed the guard.
pub fn installed(stdout: &[u8]) -> Option<bool> {
    match String::from_utf8_lossy(stdout).trim() {
        "installed" => Some(true),
        "unchanged" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn arguments(bytes: &[u8]) -> Vec<String> {
        let value = plist::Value::from_reader_xml(bytes).unwrap();
        let values = value.as_dictionary().unwrap();
        assert_eq!(values["Label"].as_string(), Some(LABEL));
        assert_eq!(values["StartInterval"].as_signed_integer(), Some(30));
        assert_eq!(values["RunAtLoad"].as_boolean(), Some(true));
        values["ProgramArguments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_string().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn programs_are_data_never_part_of_the_script() {
        let odd = "/Users/me/Library/Application Support/Elsecall/elsecall'; touch /tmp/x; '";
        let programs = vec![odd.to_owned(), "/usr/local/bin/tailscaled".to_owned()];
        let arguments = arguments(&configuration(&programs).unwrap());
        assert_eq!(arguments[..2], ["/bin/sh", "-c"]);
        assert!(!arguments[2].contains("/Users/me"));
        assert!(arguments[2].contains(TOOL));
        assert_eq!(arguments[3], "elsecall-firewall-guard");
        assert_eq!(arguments[4..], programs);
    }

    #[test]
    fn install_output_is_read_strictly() {
        assert_eq!(installed(b"installed\n"), Some(true));
        assert_eq!(installed(b"unchanged\n"), Some(false));
        assert_eq!(installed(b"Password:\ninstalled"), None);
    }

    #[test]
    fn install_script_carries_the_configuration_unchanged() {
        use base64::Engine;
        let configuration = configuration(&["/Users/me/a b".to_owned()]).unwrap();
        let script = install_script(&configuration);
        let encoded = script
            .lines()
            .find_map(|l| l.strip_prefix("printf '%s' '"))
            .and_then(|l| l.split_once('\'').map(|(e, _)| e))
            .unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            configuration
        );
        assert!(script.contains("bootstrap system \"$plist\""));
        assert!(script.contains(PLIST));
    }

    /// Runs the daemon's check as launchd would, against a fake `socketfilterfw`
    /// that reports each program's rule from a file, permitted without one, as
    /// the real tool does, and records every call.
    #[test]
    fn the_check_allows_only_blocked_programs_it_reaches_without_links() {
        let temporary = tempfile::tempdir().unwrap();
        // The check compares physical paths, and macOS's temporary directory
        // sits behind a link.
        let dir = std::fs::canonicalize(temporary.path()).unwrap();
        let tool = dir.join("socketfilterfw");
        let calls = dir.join("calls");
        std::fs::write(
            &tool,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{calls}'\ncase \"$1\" in --getappblocked) if [ -f \"$2.rule\" ]; then cat \"$2.rule\"; else echo \"Incoming connection to $2 is permitted.\"; fi;; esac\n",
                calls = calls.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let block = |path: &std::path::Path| {
            std::fs::write(
                format!("{}.rule", path.display()),
                format!("Incoming connection to {} is blocked.\n", path.display()),
            )
            .unwrap();
        };
        let program = |name: &str| {
            let path = dir.join(name);
            std::fs::write(&path, "").unwrap();
            path
        };
        let blocked = program("blocked app");
        block(&blocked);
        let permitted = program("permitted app");
        // A link, and a program behind a linked directory, are never touched,
        // even where the firewall blocks what they lead to.
        let link = dir.join("link app");
        std::os::unix::fs::symlink(&blocked, &link).unwrap();
        block(&link);
        std::fs::create_dir(dir.join("real")).unwrap();
        let behind = program("real/behind app");
        std::os::unix::fs::symlink(dir.join("real"), dir.join("linked")).unwrap();
        let through = dir.join("linked/behind app");
        block(&behind);
        block(&through);
        let missing = dir.join("missing app");
        let programs: Vec<String> = [&blocked, &permitted, &link, &through, &missing]
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let arguments =
            arguments(&configuration_with(&tool.display().to_string(), &programs).unwrap());
        let status = std::process::Command::new(&arguments[0])
            .args(&arguments[1..])
            .status()
            .unwrap();
        assert!(status.success());
        let calls = std::fs::read_to_string(&calls).unwrap();
        let calls: Vec<&str> = calls.lines().collect();
        let blocked = blocked.display();
        let permitted = permitted.display();
        assert_eq!(
            calls,
            [
                format!("--getappblocked {blocked}"),
                format!("--add {blocked}"),
                format!("--unblockapp {blocked}"),
                format!("--getappblocked {permitted}"),
            ]
        );
    }
}
