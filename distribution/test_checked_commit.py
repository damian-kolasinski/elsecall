"""Tests for the wait on Check before a release."""

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

import checked_commit
from checked_commit import State

REPOSITORY = "example/elsecall"
COMMIT = "a" * 40
OTHER = "b" * 40


def run(status="completed", conclusion="success", **fields):
    value = {
        "head_sha": COMMIT,
        "event": "push",
        "head_branch": "main",
        "path": ".github/workflows/check.yml",
        "head_repository": {"full_name": REPOSITORY},
        "status": status,
        "conclusion": conclusion if status == "completed" else None,
        "created_at": "2026-11-17T10:00:00Z",
        "run_attempt": 1,
        "html_url": "https://github.com/example/elsecall/actions/runs/1",
    }
    value.update(fields)
    return value


class Verdict(unittest.TestCase):
    def judge(self, *runs):
        return checked_commit.verdict(list(runs), REPOSITORY, COMMIT)

    def test_a_passing_push_run_on_main_passes(self):
        self.assertEqual(self.judge(run()).state, State.PASSED)

    def test_runs_that_did_not_test_this_commit_on_main_do_not_count(self):
        for other in (
            run(head_sha=OTHER),
            run(event="pull_request"),
            run(head_branch="v0.2.1"),
            run(head_branch="feature"),
            run(path=".github/workflows/release.yml"),
            run(head_repository={"full_name": "fork/elsecall"}),
        ):
            with self.subTest(other=other):
                self.assertEqual(self.judge(other).state, State.MISSING)

    def test_an_unfinished_run_is_pending(self):
        for status in ("queued", "in_progress", "waiting", "requested", "pending"):
            with self.subTest(status=status):
                self.assertEqual(self.judge(run(status=status)).state, State.PENDING)

    def test_a_run_that_did_not_succeed_fails(self):
        for conclusion in ("failure", "cancelled", "timed_out", "skipped", "neutral", None):
            with self.subTest(conclusion=conclusion):
                result = self.judge(run(conclusion=conclusion))
                self.assertEqual(result.state, State.FAILED)

    def test_a_failure_with_another_run_still_going_is_pending(self):
        result = self.judge(run(conclusion="failure"), run(status="in_progress", created_at="2026-11-17T11:00:00Z"))
        self.assertEqual(result.state, State.PENDING)

    def test_any_passing_run_passes(self):
        self.assertEqual(self.judge(run(conclusion="failure"), run(created_at="2026-11-17T11:00:00Z")).state, State.PASSED)


class Clock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


class Wait(unittest.TestCase):
    def wait(self, answers, timeout=600, missing_grace=60):
        answers = list(answers)
        clock = Clock()
        lines = []

        def fetch(repository, commit):
            self.assertEqual((repository, commit), (REPOSITORY, COMMIT))
            answer = answers.pop(0) if len(answers) > 1 else answers[0]
            if isinstance(answer, Exception):
                raise answer
            return answer

        result = checked_commit.wait(
            REPOSITORY, COMMIT, fetch, clock, clock.sleep, timeout, missing_grace, 30, lines.append
        )
        return result, clock.now, lines

    def test_waits_for_a_queued_run_to_pass(self):
        url, elapsed, lines = self.wait([[run(status="queued")], [run(status="in_progress")], [run()]])
        self.assertEqual(url, "https://github.com/example/elsecall/actions/runs/1")
        self.assertEqual(elapsed, 60)
        self.assertEqual(len(lines), 3)
        self.assertIn("queued", lines[0])
        self.assertIn("in_progress", lines[1])
        self.assertIn("passed", lines[2])

    def test_rides_out_a_failing_api_call(self):
        url, _, lines = self.wait([RuntimeError("HTTP 502"), [run()]])
        self.assertTrue(url)
        self.assertIn("HTTP 502", lines[0])

    def test_gives_up_when_the_api_keeps_failing(self):
        with self.assertRaisesRegex(SystemExit, "Could not list Check runs .*HTTP 403"):
            self.wait([RuntimeError("HTTP 403")])

    def test_stops_at_a_failed_run(self):
        with self.assertRaisesRegex(SystemExit, "(?s)did not pass on main .*failure.*Re-run"):
            self.wait([[run(status="in_progress")], [run(conclusion="failure")]])

    def test_a_run_that_appears_late_is_waited_for(self):
        url, _, _ = self.wait([[], [], [run()]])
        self.assertTrue(url)

    def test_gives_up_when_main_never_ran_check(self):
        with self.assertRaisesRegex(SystemExit, "No Check run on main"):
            self.wait([[]])

    def test_gives_up_on_a_run_that_never_finishes(self):
        with self.assertRaisesRegex(SystemExit, "did not finish in 10 minutes"):
            self.wait([[run(status="queued")]])


class Command(unittest.TestCase):
    """The command against a fake gh: the boundary to GitHub's API."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.answers = self.root / "answers.json"
        self.calls = self.root / "calls.jsonl"
        fake = self.root / "bin" / "gh"
        fake.parent.mkdir()
        fake.write_text(
            textwrap.dedent(
                f"""\
                #!{sys.executable}
                import json, sys
                with open({str(self.calls)!r}, "a") as calls:
                    calls.write(json.dumps(sys.argv[1:]) + "\\n")
                answers = json.load(open({str(self.answers)!r}))
                answer = answers.pop(0) if len(answers) > 1 else answers[0]
                json.dump(answers, open({str(self.answers)!r}, "w"))
                if answer is None:
                    print("gh: Server Error (HTTP 502)", file=sys.stderr)
                    sys.exit(1)
                print(json.dumps({{"total_count": len(answer), "workflow_runs": answer}}))
                """
            )
        )
        fake.chmod(0o755)

    def command(self, answers, *extra):
        self.answers.write_text(json.dumps(answers))
        env = dict(os.environ, PATH=f"{self.root / 'bin'}{os.pathsep}{os.environ['PATH']}")
        return subprocess.run(
            [
                sys.executable,
                str(Path(__file__).with_name("checked_commit.py")),
                "--repository",
                REPOSITORY,
                "--commit",
                COMMIT,
                "--interval-seconds",
                "0",
                *extra,
            ],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )

    def test_asks_github_for_check_push_runs_on_main_and_records_the_run(self):
        summary = self.root / "summary.md"
        completed = self.command([None, [run(status="in_progress")], [run()]], "--summary", str(summary))
        self.assertEqual(completed.returncode, 0, completed.stderr)
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        self.assertEqual(len(calls), 3)
        self.assertEqual(
            calls[0],
            [
                "api",
                "--method",
                "GET",
                "repos/example/elsecall/actions/workflows/check.yml/runs",
                "-f",
                f"head_sha={COMMIT}",
                "-f",
                "event=push",
                "-f",
                "branch=main",
                "-f",
                "per_page=100",
            ],
        )
        self.assertIn("HTTP 502", completed.stdout)
        self.assertEqual(
            summary.read_text(), "Tested by Check on main: https://github.com/example/elsecall/actions/runs/1\n"
        )

    def test_fails_without_a_passing_run(self):
        completed = self.command([[run(conclusion="failure")]])
        self.assertEqual(completed.returncode, 1)
        self.assertIn("Check did not pass on main", completed.stderr)

    def test_rejects_a_malformed_commit(self):
        self.answers.write_text("[[]]")
        completed = subprocess.run(
            [sys.executable, str(Path(__file__).with_name("checked_commit.py")), "--repository", REPOSITORY, "--commit", "main"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 1)
        self.assertIn("invalid repository or commit", completed.stderr)
        self.assertFalse(self.calls.exists())


if __name__ == "__main__":
    unittest.main()
