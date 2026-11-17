"""Run install.sh against a fake mise, the one external tool it drives."""

import os
import platform
import subprocess
import tempfile
import unittest
from pathlib import Path

import stamp_installer

SCRIPT = Path(__file__).resolve().with_name("install.sh")

# Records every call, then answers like mise would for the commands install.sh uses.
FAKE_MISE = """#!/bin/sh
printf '%s\\n' "$*" >> "$FAKE_LOG"
case "$1" in
backends) printf 'github\\npackslip\\n' ;;
--version) echo "2026.10.11 macos-arm64" ;;
use) [ -z "${FAKE_USE_FAILS:-}" ] || exit 1 ;;
exec)
  printf '%s\\n' "$FAKE_RESULT"
  exit "${FAKE_STATUS:-0}"
  ;;
esac
"""


def opened():
    return '{"schema_version":1,"data":{"window":{"state":"opened","app":"/Users/me/Applications/Elsecall.app"}}}'


@unittest.skipUnless(platform.system() == "Darwin" and platform.machine() == "arm64", "Apple silicon only")
class InstallScript(unittest.TestCase):
    def run_script(self, result=opened(), status=0, script=SCRIPT, **environment):
        root = Path(self.enterContext(tempfile.TemporaryDirectory(prefix="elsecall-script-")))
        home = root / "home"
        tools = home / ".local/bin"
        tools.mkdir(parents=True)
        mise = tools / "mise"
        mise.write_text(FAKE_MISE)
        mise.chmod(0o755)
        log = root / "calls"
        completed = subprocess.run(
            ["/bin/sh", str(script)],
            env={
                "HOME": str(home),
                "PATH": "/usr/bin:/bin",
                "FAKE_LOG": str(log),
                "FAKE_RESULT": result,
                "FAKE_STATUS": str(status),
                **environment,
            },
            cwd=root,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        calls = log.read_text().splitlines() if log.exists() else []
        return completed, calls

    def test_it_selects_the_release_globally_then_installs_and_opens_it(self):
        completed, calls = self.run_script()
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn(
            "use --global --pin packslip:github.com/damian-kolasinski/elsecall@latest",
            calls,
        )
        self.assertEqual(calls[-1], "exec -- elsecall install --open")
        self.assertIn('"state":"opened"', completed.stdout)
        # ~/.local/bin is not on this PATH, so the script says how to add it.
        self.assertIn('export PATH="$HOME/.local/bin:$PATH"', completed.stderr)

    def test_a_chosen_version_is_selected(self):
        completed, calls = self.run_script(ELSECALL_VERSION="0.2.0")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn(
            "use --global --pin packslip:github.com/damian-kolasinski/elsecall@0.2.0",
            calls,
        )

    def test_the_installer_attached_to_a_release_installs_that_release(self):
        directory = Path(self.enterContext(tempfile.TemporaryDirectory(prefix="elsecall-stamped-")))
        stamped = directory / "install.sh"
        stamped.write_text(stamp_installer.stamp(SCRIPT.read_text(), "0.2.1"))
        completed, calls = self.run_script(script=stamped)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn(
            "use --global --pin packslip:github.com/damian-kolasinski/elsecall@0.2.1",
            calls,
        )
        completed, calls = self.run_script(script=stamped, ELSECALL_VERSION="0.3.0")
        self.assertIn(
            "use --global --pin packslip:github.com/damian-kolasinski/elsecall@0.3.0",
            calls,
        )

    def test_an_account_without_a_desktop_still_succeeds_and_explains(self):
        completed, _ = self.run_script(
            result='{"schema_version":1,"data":{"window":{"state":"background_session"}}}',
            status=1,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("no desktop session", completed.stderr)

    def test_a_window_that_does_not_open_fails_but_says_the_service_is_installed(self):
        completed, _ = self.run_script(
            result='{"schema_version":1,"data":{"window":{"state":"failed","error":{"code":"window_open_failed"}}}}',
            status=1,
        )
        self.assertEqual(completed.returncode, 1)
        self.assertIn("The service is installed, but the window did not open", completed.stderr)

    def test_a_failed_installation_fails_the_script(self):
        completed, _ = self.run_script(
            result='{"schema_version":1,"error":{"code":"startup_failed"}}',
            status=1,
        )
        self.assertEqual(completed.returncode, 1)
        self.assertIn("startup_failed", completed.stdout)

    def test_a_refused_release_stops_before_installing(self):
        completed, calls = self.run_script(FAKE_USE_FAILS="1")
        self.assertNotEqual(completed.returncode, 0)
        self.assertFalse(any(call.startswith("exec") for call in calls))


if __name__ == "__main__":
    unittest.main()
