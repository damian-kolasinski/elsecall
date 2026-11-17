"""Unit tests for the VM tool's pure parts; they need neither Tart nor VNC."""

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from elsecall_vm import image, registry, terminal, waits  # noqa: E402
from elsecall_vm.guest import tail  # noqa: E402
from elsecall_vm.deadline import poll  # noqa: E402
from elsecall_vm.errors import VmError, redact  # noqa: E402
from elsecall_vm.recorder import frames_due  # noqa: E402
from elsecall_vm.vnc import endpoint_from_log, keystrokes  # noqa: E402


def secret() -> str:
    # Assembled at run time so the source holds no password-shaped literal.
    return "".join(["s3", "cr", "et", "-pw"])


class EndpointTest(unittest.TestCase):
    def test_parses_tart_banner(self):
        found = endpoint_from_log(f"Opening vnc://:{secret()}@127.0.0.1:53952...\n")
        self.assertEqual((found.host, found.port, found.password), ("127.0.0.1", 53952, secret()))

    def test_repr_hides_password(self):
        found = endpoint_from_log(f"Opening vnc://:{secret()}@127.0.0.1:5900...")
        self.assertNotIn(secret(), repr(found))
        self.assertNotIn(secret(), str(found))

    def test_missing_banner(self):
        self.assertIsNone(endpoint_from_log("booting\n"))


class RedactTest(unittest.TestCase):
    def test_removes_password_from_messages(self):
        text = redact(f"tart run exited 1: Opening vnc://:{secret()}@127.0.0.1:5900...")
        self.assertNotIn(secret(), text)
        self.assertIn("vnc://:***@127.0.0.1:5900", text)

    def test_errors_are_redacted(self):
        self.assertNotIn(secret(), str(VmError(f"vnc://:{secret()}@127.0.0.1:1")))


class TailTest(unittest.TestCase):
    def test_redacts_before_cutting(self):
        log = f"Opening vnc://:{secret()}@127.0.0.1:5900...\n"
        # A cut inside the URL must not leave the password behind.
        for size in range(1, len(log) + 1):
            self.assertNotIn(secret(), tail(log, size))


class GuestInputTest(unittest.TestCase):
    def test_process_probe_runs_pgrep_without_a_shell(self):
        self.assertEqual(waits.process("Elsecall.app").argv, ["pgrep", "-f", "--", "Elsecall.app"])

    def test_file_probe_passes_the_path_as_an_argument(self):
        argv = waits.file("~/a b*.txt").argv
        self.assertEqual(argv[-1], "~/a b*.txt")
        self.assertNotIn("a b*", argv[2])

    def test_hostnames_and_vm_names_are_restricted(self):
        self.assertIn("scutil --set LocalHostName mac-mini", image.rename("mac-mini"))
        for bad in ("Mac Mini", "a;b", ""):
            with self.assertRaises(ValueError):
                image.rename(bad)
        for bad in ("../x", "a/b", ".hidden", ""):
            with self.assertRaises(ValueError):
                registry.check_name(bad)


class KeystrokesTest(unittest.TestCase):
    def test_shifted_characters_use_their_us_keys(self):
        self.assertEqual(
            keystrokes("a|B ~\n"),
            [(False, "a"), (True, "\\"), (True, "b"), (False, "space"), (True, "`"), (False, "enter")],
        )

    def test_plain_punctuation_needs_no_shift(self):
        self.assertEqual(keystrokes("-./"), [(False, "-"), (False, "."), (False, "/")])


class FramesDueTest(unittest.TestCase):
    def test_first_frame_is_due_at_once(self):
        self.assertEqual(frames_due(0.0, 10, 0), 1)

    def test_slow_capture_repeats_frames(self):
        self.assertEqual(frames_due(0.55, 10, 1), 5)

    def test_fast_capture_writes_nothing_early(self):
        self.assertEqual(frames_due(0.05, 10, 1), 0)


class PollTest(unittest.TestCase):
    def fake_clock(self):
        now = [0.0]
        return (lambda: now[0]), (lambda seconds: now.__setitem__(0, now[0] + seconds))

    def test_returns_as_soon_as_condition_holds(self):
        clock, sleep = self.fake_clock()
        calls = iter([False, False, True])
        self.assertTrue(poll(lambda: next(calls), 10, 1, clock, sleep))
        self.assertEqual(clock(), 2.0)

    def test_gives_up_at_the_deadline_not_after(self):
        clock, sleep = self.fake_clock()
        self.assertFalse(poll(lambda: False, 2.5, 1, clock, sleep))
        self.assertEqual(clock(), 2.5)


class SessionScriptTest(unittest.TestCase):
    def run_session(self, body: str) -> tuple[str, str]:
        with tempfile.TemporaryDirectory() as home:
            script = terminal.session_script("demo", body, type_delay_cs=0)
            # The guest runs it as an interactive login zsh that ends at a prompt;
            # here it runs non-interactively and the final prompt is replaced.
            script = script.replace("#!/bin/zsh -il", "").replace("exec zsh -il", "exit")
            path = Path(home) / "demo.command"
            path.write_text(script)
            done = subprocess.run(["zsh", "-f", str(path)], capture_output=True, text=True,
                                  env={"HOME": home, "PATH": os.environ["PATH"], "TERM": "dumb"})
            status = (Path(home) / "ecvm/demo.status").read_text().strip()
            return done.stdout, status

    @unittest.skipUnless(os.path.exists("/bin/zsh"), "needs zsh")
    def test_shows_commands_narrates_and_records_success(self):
        out, status = self.run_session("# Install it\necho 'hi there' | tr a-z A-Z\n")
        self.assertEqual(status, "0")
        self.assertIn("Install it", out)
        self.assertIn("echo 'hi there' | tr a-z A-Z", out)
        self.assertIn("HI THERE", out)

    @unittest.skipUnless(os.path.exists("/bin/zsh"), "needs zsh")
    def test_stops_at_first_failure_with_its_status(self):
        out, status = self.run_session("false | true\necho never\n")
        self.assertEqual(status, "1")
        self.assertNotIn("\nnever", out)

    def test_labels_are_restricted(self):
        with self.assertRaises(ValueError):
            terminal.check_label("../x")


class RegistryTest(unittest.TestCase):
    def test_only_claimed_vms_are_owned_and_state_is_private(self):
        with tempfile.TemporaryDirectory() as home:
            os.environ["ECVM_HOME"] = home
            try:
                self.assertIsNone(registry.owned("ec-base"))
                registry.claim("demo-1", registry.Kind.VM, "ec-ready", "02:00:00:00:00:01")
                self.assertEqual(registry.owned("demo-1").source, "ec-ready")
                self.assertEqual(registry.owned("demo-1").mac, "02:00:00:00:00:01")
                self.assertEqual(registry.vm_dir("demo-1").stat().st_mode & 0o777, 0o700)
                registry.forget("demo-1")
                self.assertIsNone(registry.owned("demo-1"))
            finally:
                del os.environ["ECVM_HOME"]


if __name__ == "__main__":
    unittest.main()
