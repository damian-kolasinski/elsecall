"""Tests for main_status.py, with a stand-in gh that records how it was run."""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import main_status
from main_status import Close, Comment, Nothing, Open, Run

SCRIPT = Path(__file__).with_name("main_status.py")
REPOSITORY = "me/acme"
COMMIT = "0123456789abcdef0123456789abcdef01234567"
URL = "https://github.com/me/acme/actions/runs/42"


def run(conclusion: str) -> Run:
    return Run(conclusion, URL, COMMIT)


class Decide(unittest.TestCase):
    def test_a_failure_opens_the_issue_with_the_run_and_commit(self):
        action = main_status.decide(run("failure"), None)
        self.assertIsInstance(action, Open)
        self.assertIn(f"at 0123456789ab: {URL}", action.body)

    def test_a_failure_while_the_issue_is_open_comments_on_it(self):
        self.assertEqual(
            main_status.decide(run("failure"), 7),
            Comment(7, f"Check ended with failure on main at 0123456789ab: {URL}"),
        )

    def test_a_run_that_timed_out_or_never_started_counts_as_red(self):
        for conclusion in ("timed_out", "startup_failure", "action_required"):
            self.assertIsInstance(main_status.decide(run(conclusion), None), Open)
            self.assertIsInstance(main_status.decide(run(conclusion), 7), Comment)

    def test_a_pass_closes_the_open_issue(self):
        self.assertEqual(
            main_status.decide(run("success"), 7),
            Close(7, f"Check passed on main at 0123456789ab: {URL}"),
        )

    def test_a_pass_without_an_issue_changes_nothing(self):
        self.assertIsInstance(main_status.decide(run("success"), None), Nothing)

    def test_a_superseded_run_changes_nothing_either_way(self):
        for conclusion in ("cancelled", "skipped", "neutral", "stale", ""):
            for issue in (None, 7):
                self.assertIsInstance(main_status.decide(run(conclusion), issue), Nothing)


class Command(unittest.TestCase):
    """The script end to end against a gh stand-in."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        root = Path(self.directory.name)
        self.log = root / "calls.jsonl"
        self.issues = root / "issues.json"
        fake = root / "fake-gh"
        fake.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "with open(os.environ['GH_LOG'], 'a') as log:\n"
            "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            "if sys.argv[1:3] == ['issue', 'list']:\n"
            "    print(open(os.environ['GH_ISSUES']).read())\n"
        )
        fake.chmod(0o755)
        self.env = dict(os.environ, GH=str(fake), GH_LOG=str(self.log), GH_ISSUES=str(self.issues))

    def tearDown(self):
        self.directory.cleanup()

    def main(self, conclusion: str, open_issues: list[int], url: str = URL):
        self.issues.write_text(json.dumps([{"number": n} for n in open_issues]))
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--repository", REPOSITORY, "--conclusion", conclusion,
             "--url", url, "--commit", COMMIT],
            env=self.env, capture_output=True, text=True,
        )

    def calls(self) -> list[list[str]]:
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_a_failure_creates_the_label_and_the_issue_in_the_repository(self):
        result = self.main("failure", [])
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertEqual([call[:2] for call in calls], [["issue", "list"], ["label", "create"], ["issue", "create"]])
        self.assertTrue(all(call[-2:] == ["--repo", REPOSITORY] for call in calls))
        create = calls[2]
        self.assertEqual(create[create.index("--label") + 1], "main-red")
        self.assertIn(URL, create[create.index("--body") + 1])

    def test_a_pass_closes_the_oldest_open_issue(self):
        result = self.main("success", [9, 4])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls()[1][:3], ["issue", "close", "4"])
        self.assertIn("closed main-red issue #4", result.stdout)

    def test_a_cancelled_run_only_reads(self):
        result = self.main("cancelled", [4])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call[:2] for call in self.calls()], [["issue", "list"]])

    def test_a_url_outside_actions_runs_is_refused_before_gh_runs(self):
        result = self.main("failure", [], url="https://example.com/x")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a workflow run URL", result.stderr)
        self.assertEqual(self.calls(), [])


if __name__ == "__main__":
    unittest.main()
