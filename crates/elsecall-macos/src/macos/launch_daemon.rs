//! Installing a root LaunchDaemon: the root shell script that puts a
//! configuration in `/Library/LaunchDaemons` and loads it in the system domain.
//! Each daemon's own module says what it runs and why root may load it.

/// A root shell script that installs `configuration` at `plist` as the daemon
/// `label` and loads it, replacing an older one. `label` and `plist` go into the
/// script as they are, so callers pass constants or validated names: letters,
/// digits, `.`, `_`, `-` and `/` only. It prints `installed` when it
/// changed the daemon and `unchanged` when the same one was already loaded. The
/// configuration travels as base64, so nothing in it is ever shell syntax.
pub(crate) fn install_script(label: &str, plist: &str, configuration: &[u8]) -> String {
    use base64::Engine;
    let plain = |text: &str| {
        text.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
    };
    assert!(plain(label) && plain(plist), "a plain label and path");
    let encoded = base64::engine::general_purpose::STANDARD.encode(configuration);
    format!(
        r#"set -e
plist='{plist}'
new=$(/usr/bin/mktemp /Library/LaunchDaemons/.{label}.XXXXXX)
trap '/bin/rm -f "$new"' EXIT
printf '%s' '{encoded}' | /usr/bin/base64 -D > "$new"
if /usr/bin/cmp -s "$new" "$plist" && /bin/launchctl print system/{label} >/dev/null 2>&1; then
  echo unchanged
  exit 0
fi
/usr/sbin/chown root:wheel "$new"
/bin/chmod 644 "$new"
/bin/mv -f "$new" "$plist"
/bin/launchctl bootout system/{label} >/dev/null 2>&1 || true
waited=0
while /bin/launchctl print system/{label} >/dev/null 2>&1 && [ "$waited" -lt 100 ]; do
  /bin/sleep 0.1
  waited=$((waited + 1))
done
/bin/launchctl enable system/{label}
/bin/launchctl bootstrap system "$plist"
echo installed"#
    )
}

/// Whether `install_script`'s output says it changed the daemon.
pub(crate) fn installed(stdout: &[u8]) -> Option<bool> {
    match String::from_utf8_lossy(stdout).trim() {
        "installed" => Some(true),
        "unchanged" => Some(false),
        _ => None,
    }
}

/// The commands an administrator runs to remove the daemon.
pub(crate) fn removal(label: &str, plist: &str) -> [String; 2] {
    [
        format!("sudo launchctl bootout system/{label}"),
        format!("sudo rm {plist}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_output_is_read_strictly() {
        assert_eq!(installed(b"installed\n"), Some(true));
        assert_eq!(installed(b"unchanged\n"), Some(false));
        assert_eq!(installed(b"Password:\ninstalled"), None);
    }

    #[test]
    fn install_script_carries_the_configuration_unchanged() {
        use base64::Engine;
        let configuration = b"<plist>'; touch /tmp/x; '</plist>";
        let script = install_script(
            "dev.example.label",
            "/Library/LaunchDaemons/x.plist",
            configuration,
        );
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
        assert!(!script.contains("touch"));
        assert!(script.contains("bootstrap system \"$plist\""));
        assert!(script.contains("plist='/Library/LaunchDaemons/x.plist'"));
        assert!(script.contains("/Library/LaunchDaemons/.dev.example.label.XXXXXX"));
    }
}
