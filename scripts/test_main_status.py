"""Tests for main_status.py, with a stand-in gh that records how it was run."""
from __future__ import annotations

import json
import os
import re
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


def run(conclusion: str, workflow: str = "Check") -> Run:
    return Run(workflow, conclusion, URL, COMMIT)


class Decide(unittest.TestCase):
    def test_a_failure_opens_the_issue_with_the_workflow_run_and_commit(self):
        action = main_status.decide(run("failure", "Module shape"), None)
        self.assertIsInstance(action, Open)
        self.assertIn(f"Module shape ended with failure on main at 0123456789ab: {URL}", action.body)
        self.assertIn("A passing Module shape run on main closes this issue", action.body)

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
            main_status.decide(run("success", "Privacy"), 7),
            Close(7, f"Privacy passed on main at 0123456789ab: {URL}"),
        )

    def test_a_pass_without_an_issue_changes_nothing(self):
        self.assertIsInstance(main_status.decide(run("success"), None), Nothing)

    def test_a_superseded_run_changes_nothing_either_way(self):
        for conclusion in ("cancelled", "skipped", "neutral", "stale", ""):
            for issue in (None, 7):
                self.assertIsInstance(main_status.decide(run(conclusion), issue), Nothing)

    def test_a_run_that_finished_after_a_later_one_reported_changes_nothing(self):
        for conclusion, issue in (("success", 7), ("failure", None), ("failure", 7)):
            self.assertIsInstance(main_status.decide(run(conclusion), issue, newer_result=True), Nothing)

    def test_the_watched_workflows_are_the_ones_the_repository_names(self):
        workflows = Path(__file__).resolve().parents[1] / ".github" / "workflows"
        status = (workflows / "main-status.yml").read_text()
        watched = re.search(r"\n    workflows: \[(.*)\]", status).group(1).split(", ")
        names = {
            re.search(r"(?m)^name: (.+)$", path.read_text()).group(1)
            for path in workflows.glob("*.yml")
        }
        self.assertEqual(sorted(watched), ["Check", "Module shape", "Privacy"])
        self.assertLessEqual(set(watched), names)


class Command(unittest.TestCase):
    """The script end to end against a gh stand-in."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        root = Path(self.directory.name)
        self.log = root / "calls.jsonl"
        self.issues = root / "issues.json"
        fake = root / "fake-gh"
        self.runs = root / "runs.json"
        # Like gh, it returns only the fields `--json` asks for.
        fake.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "args = sys.argv[1:]\n"
            "with open(os.environ['GH_LOG'], 'a') as log:\n"
            "    log.write(json.dumps(args) + '\\n')\n"
            "listed = {('issue', 'list'): 'GH_ISSUES', ('run', 'list'): 'GH_RUNS'}.get(tuple(args[:2]))\n"
            "if listed:\n"
            "    fields = args[args.index('--json') + 1].split(',')\n"
            "    items = json.load(open(os.environ[listed]))\n"
            "    print(json.dumps([{f: item[f] for f in fields if f in item} for item in items]))\n"
        )
        fake.chmod(0o755)
        self.env = dict(
            os.environ, GH=str(fake), GH_LOG=str(self.log), GH_ISSUES=str(self.issues), GH_RUNS=str(self.runs)
        )

    def tearDown(self):
        self.directory.cleanup()

    def main(self, workflow: str, conclusion: str, open_issues: dict[int, str], url: str = URL,
             runs: list[tuple[int, str]] = ()):
        self.issues.write_text(json.dumps([{"number": n, "title": t} for n, t in open_issues.items()]))
        self.runs.write_text(json.dumps([{"databaseId": i, "conclusion": c} for i, c in runs]))
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--repository", REPOSITORY, "--workflow", workflow,
             "--conclusion", conclusion, "--url", url, "--commit", COMMIT],
            env=self.env, capture_output=True, text=True,
        )

    def calls(self) -> list[list[str]]:
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_a_failure_creates_the_label_and_the_workflows_issue_in_the_repository(self):
        result = self.main("Privacy", "failure", {})
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertEqual([call[:2] for call in calls], [["run", "list"], ["issue", "list"], ["label", "create"], ["issue", "create"]])
        self.assertTrue(all(call[-2:] == ["--repo", REPOSITORY] for call in calls))
        create = calls[3]
        self.assertEqual(create[create.index("--title") + 1], "main is red: Privacy")
        self.assertEqual(create[create.index("--label") + 1], "main-red")
        self.assertIn(URL, create[create.index("--body") + 1])

    def test_a_pass_closes_only_its_own_workflows_issue(self):
        open_issues = {3: "main is red: Check", 9: "main is red: Module shape", 4: "main is red: Module shape"}
        result = self.main("Module shape", "success", open_issues)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls()[2][:3], ["issue", "close", "4"])
        self.assertIn("Module shape: closed main-red issue #4", result.stdout)

    def test_another_workflows_open_issue_is_not_commented_on(self):
        result = self.main("Privacy", "failure", {3: "main is red: Check"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call[:2] for call in self.calls()][2:], [["label", "create"], ["issue", "create"]])

    def test_a_late_pass_does_not_close_the_issue_a_later_failure_opened(self):
        # Run 42 passed, but run 50, of a later push, failed and already reported.
        result = self.main("Privacy", "success", {4: "main is red: Privacy"},
                           runs=[(50, "failure"), (42, "success"), (30, "failure")])
        self.assertEqual(result.returncode, 0, result.stderr)
        run_list = self.calls()[0]
        self.assertEqual(run_list[run_list.index("--workflow") + 1], "Privacy")
        self.assertEqual([call[:2] for call in self.calls()], [["run", "list"], ["issue", "list"]])
        self.assertIn("a later Privacy run on main has already reported", result.stdout)

    def test_a_later_cancelled_run_does_not_hide_a_result(self):
        result = self.main("Privacy", "success", {4: "main is red: Privacy"}, runs=[(50, "cancelled")])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls()[2][:3], ["issue", "close", "4"])

    def test_a_cancelled_run_only_reads(self):
        result = self.main("Check", "cancelled", {4: "main is red: Check"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call[:2] for call in self.calls()], [["issue", "list"]])

    def test_a_url_outside_actions_runs_is_refused_before_gh_runs(self):
        result = self.main("Check", "failure", {}, url="https://example.com/x")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a workflow run URL", result.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_workflow_name_with_shell_or_option_characters_is_refused(self):
        for name in ("-x", "Check; rm", "$(id)", ""):
            result = self.main(name, "failure", {})
            self.assertNotEqual(result.returncode, 0, name)
        self.assertEqual(self.calls(), [])


if __name__ == "__main__":
    unittest.main()
