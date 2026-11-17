"""Wait until the Check workflow has passed on main for the commit being released.

Check runs fmt, clippy and the full test suite on every commit pushed to main.
A release builds and publishes the same commit, so it relies on that run
instead of running the suite again. Only a push run on main counts: a pull
request run tests a merge of its branch, not the commit that gets tagged.
"""

import argparse
import re
import subprocess
import sys
import time
import json
from dataclasses import dataclass
from enum import Enum

WORKFLOW = ".github/workflows/check.yml"
BRANCH = "main"
REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
COMMIT = re.compile(r"[0-9a-f]{40}")


class State(Enum):
    PASSED = "passed"
    PENDING = "pending"
    FAILED = "failed"
    MISSING = "missing"


@dataclass(frozen=True)
class Verdict:
    state: State
    url: str = ""
    detail: str = ""


def verdict(runs, repository, commit):
    """Judge the Check runs GitHub lists for a commit.

    Any passing run on main is enough; a re-run that passed after a failure
    counts, since GitHub reports a run's latest attempt.
    """
    relevant = [
        run
        for run in runs
        if run.get("head_sha") == commit
        and run.get("event") == "push"
        and run.get("head_branch") == BRANCH
        and run.get("path") == WORKFLOW
        and (run.get("head_repository") or {}).get("full_name") == repository
    ]
    if not relevant:
        return Verdict(State.MISSING)
    newest = max(relevant, key=lambda run: run.get("created_at") or "")
    for run in relevant:
        if run.get("status") == "completed" and run.get("conclusion") == "success":
            return Verdict(State.PASSED, run.get("html_url", ""))
    unfinished = [run for run in relevant if run.get("status") != "completed"]
    if unfinished:
        run = max(unfinished, key=lambda run: run.get("created_at") or "")
        return Verdict(State.PENDING, run.get("html_url", ""), run.get("status", ""))
    return Verdict(State.FAILED, newest.get("html_url", ""), newest.get("conclusion") or "")


def check_runs(repository, commit):
    """List the Check workflow's push runs on main for a commit through gh."""
    result = subprocess.run(
        [
            "gh",
            "api",
            "--method",
            "GET",
            f"repos/{repository}/actions/workflows/check.yml/runs",
            "-f",
            f"head_sha={commit}",
            "-f",
            "event=push",
            "-f",
            f"branch={BRANCH}",
            "-f",
            "per_page=100",
        ],
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
    if result.returncode:
        detail = (result.stderr or result.stdout).strip().splitlines()
        raise RuntimeError(detail[-1] if detail else f"gh api failed (exit {result.returncode})")
    return json.loads(result.stdout)["workflow_runs"]


def wait(repository, commit, fetch, clock, sleep, timeout, missing_grace, interval, report):
    """Poll until Check passed, failed or did not appear; return the passing run's URL."""
    started = clock()
    failing_since = None
    shown = None
    while True:
        try:
            current = verdict(fetch(repository, commit), repository, commit)
            failing_since = None
        except (RuntimeError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
            failing_since = clock() if failing_since is None else failing_since
            if clock() - failing_since >= missing_grace:
                raise SystemExit(f"Could not list Check runs for {commit}: {error}") from error
            current = Verdict(State.PENDING, detail=f"could not list Check runs: {error}")
        if current.state is State.PASSED:
            report(f"Check passed on {BRANCH} for {commit}: {current.url}")
            return current.url
        if current.state is State.FAILED:
            raise SystemExit(
                f"Check did not pass on {BRANCH} for {commit} ({current.detail}): {current.url}\n"
                "Re-run its failed jobs. Once it passes, re-run this workflow's failed jobs."
            )
        elapsed = clock() - started
        if current.state is State.MISSING and elapsed >= missing_grace:
            raise SystemExit(
                f"No Check run on {BRANCH} for {commit}. GitHub runs Check only for the newest "
                f"commit of each push to {BRANCH}; release a commit that Check tested there."
            )
        if elapsed >= timeout:
            raise SystemExit(f"Check on {BRANCH} for {commit} did not finish in {timeout / 60:.0f} minutes: {current.url}")
        if (current.state, current.url, current.detail) != shown:
            shown = (current.state, current.url, current.detail)
            report(f"Waiting for Check on {BRANCH} for {commit}: {current.detail or current.state.value} {current.url}".rstrip())
        sleep(interval)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--timeout-minutes", type=float, default=120)
    parser.add_argument("--missing-grace-minutes", type=float, default=5)
    parser.add_argument("--interval-seconds", type=float, default=30)
    parser.add_argument("--summary", help="file to append the passing run to, such as $GITHUB_STEP_SUMMARY")
    args = parser.parse_args(argv)
    if not REPOSITORY.fullmatch(args.repository) or not COMMIT.fullmatch(args.commit):
        raise SystemExit("invalid repository or commit")
    url = wait(
        args.repository,
        args.commit,
        fetch=check_runs,
        clock=time.monotonic,
        sleep=time.sleep,
        timeout=args.timeout_minutes * 60,
        missing_grace=args.missing_grace_minutes * 60,
        interval=args.interval_seconds,
        report=lambda line: print(line, flush=True),
    )
    if args.summary:
        with open(args.summary, "a", encoding="utf-8") as file:
            file.write(f"Tested by Check on {BRANCH}: {url}\n")


if __name__ == "__main__":
    sys.exit(main())
