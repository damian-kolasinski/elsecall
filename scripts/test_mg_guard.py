"""Tests for mg_guard.py, with a stand-in mg that records how it was run.

Samples that must be caught are assembled at run time, so this file itself
passes the privacy scan.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import mg_guard

GUARD = Path(__file__).with_name("mg_guard.py")
REAL_HOME = "/Users/" + "jane"


class Arguments(unittest.TestCase):
    def test_finds_published_text_and_keeps_every_argument(self):
        with tempfile.TemporaryDirectory() as directory:
            guide = Path(directory) / "guide.md"
            guide.write_text("Why it changed.\n")
            args = ["submit", "--title", "Add it", f"--guide={guide}", "--on", "none", "--wake", "", "--json"]
            texts, run = mg_guard.published(args)
            self.assertEqual(texts, [("title", "Add it"), ("guide", "Why it changed.\n")])
            self.assertEqual(run, ["submit", "--title", "Add it", "--guide", str(guide), "--on", "none", "--wake", "", "--json"])

    def test_a_guide_from_stdin_is_saved_for_mg(self):
        texts, run = mg_guard.published(["submit", "--guide", "-"], stdin="From stdin.\n")
        self.assertEqual(texts, [("guide", "From stdin.\n")])
        self.assertEqual(run[:2], ["submit", "--guide"])
        self.assertEqual(Path(run[2]).read_text(), "From stdin.\n")
        os.unlink(run[2])

    def test_replies_and_options_without_values(self):
        self.assertEqual(
            mg_guard.published(["reply", "c_3", "--status", "rejected", "--body=Because"]),
            ([("body", "Because")], ["reply", "c_3", "--status", "rejected", "--body", "Because"]),
        )
        self.assertEqual(mg_guard.published(["submit", "--title"]), ([], ["submit", "--title"]))


class CommandLine(unittest.TestCase):
    """Runs the guard in a throwaway repository with a published main, isolated
    from the operator's git configuration and private terms."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.record = self.root / "mg-ran.json"
        fake = self.root / "fake-mg"
        fake.write_text(
            f"#!{sys.executable}\nimport json, sys\n"
            f"json.dump(sys.argv[1:], open({str(self.record)!r}, 'w'))\n"
        )
        fake.chmod(0o755)
        self.env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("PRIVACY_SCAN_", "GIT_", "XDG_CONFIG_HOME"))
        }
        self.env.update(HOME=str(self.root), MG=str(fake), PYTHONDONTWRITEBYTECODE="1", GIT_CONFIG_NOSYSTEM="1")
        for args in [
            ["init", "-q", "-b", "main"], ["config", "user.name", "Dev"], ["config", "user.email", "dev@example.com"],
            ["config", "commit.gpgsign", "false"], ["init", "-q", "--bare", str(self.root / "remote.git")],
            ["remote", "add", "origin", str(self.root / "remote.git")],
        ]:
            self.git(*args)
        self.commit("a.md", "published\n", "Start")
        self.git("push", "-q", "origin", "main")

    def tearDown(self):
        self.directory.cleanup()

    def git(self, *args: str) -> None:
        subprocess.run(["git", *args], cwd=self.repo, check=True, capture_output=True, env=self.env)

    def commit(self, path: str, content: str, message: str) -> None:
        (self.repo / path).write_text(content)
        self.git("add", "-A")
        self.git("commit", "-q", "--no-verify", "-m", message)

    def guard(self, *args: str, stdin: str = "") -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(GUARD), *args], input=stdin, capture_output=True, text=True, env=self.env, cwd=self.repo
        )

    def test_runs_mg_with_the_same_arguments_when_clean(self):
        guide = self.root / "guide.md"
        guide.write_text("Runs on mac-mini.\n")
        self.commit("b.md", "clean\n", "Clean work")
        result = self.guard("submit", "--title", "Add it", "--guide", str(guide), "--wake", "")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(self.record.read_text()), ["submit", "--title", "Add it", "--guide", str(guide), "--wake", ""])

    def test_a_guide_from_a_pipe_reaches_mg_intact(self):
        result = self.guard("submit", "--guide", "/dev/stdin", stdin="Piped guide.\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        ran = json.loads(self.record.read_text())
        self.assertEqual(Path(ran[2]).read_text(), "Piped guide.\n")
        os.unlink(ran[2])

    def test_refuses_to_run_mg_when_what_it_publishes_leaks(self):
        guide = self.root / "guide.md"
        guide.write_text(f"Why.\n\nTested in {REAL_HOME}/src.\n")
        result = self.guard("submit", "--guide", str(guide))
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"guide:3:11: home-path: {REAL_HOME}", result.stdout)

        self.env["PRIVACY_SCAN_TERMS"] = "Contoso"
        result = self.guard("reply", "c_1", "--status", "rejected", "--body", "Contoso needs it")
        self.assertEqual(result.returncode, 1)
        self.assertIn("body:1:1: private-term 1: Contoso", result.stdout)

        # A commit that skipped every hook is caught before mg pushes it.
        self.commit("c.md", "for Contoso\n", "Leaky work")
        result = self.guard("submit", "--title", "Clean title")
        self.assertEqual(result.returncode, 1)
        self.assertIn("c.md:1:5: private-term 1: Contoso", result.stdout)
        self.assertNotIn("a.md", result.stdout)
        self.assertFalse(self.record.exists())

    def test_reports_problems_without_running_mg(self):
        result = self.guard("submit", "--guide", str(self.root / "missing.md"))
        self.assertEqual(result.returncode, 2)
        self.assertIn("cannot read the guide", result.stderr)
        self.git("config", "privacyscan.requireTerms", "true")
        result = self.guard("submit")
        self.assertEqual(result.returncode, 2)
        self.assertIn("requires the private terms", result.stderr)
        self.assertFalse(self.record.exists())


if __name__ == "__main__":
    unittest.main()
