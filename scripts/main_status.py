#!/usr/bin/env python3
"""Keep one GitHub issue open per workflow that is red on main.

`.github/workflows/main-status.yml` runs this after each run of a watched
workflow on main finishes. Each workflow has its own issue, titled
`main is red: <workflow>` and labelled main-red, so a pass in one never closes
another's. A run that did not pass opens its workflow's issue, or comments on
the one already open; a passing run closes it. A cancelled or skipped run says
nothing about main and changes nothing: a newer commit superseded it. Neither
does a run that finished after a later run of its workflow already reported,
since runs of different pushes can finish out of order. A timed-out run counts
as failed: the workflow is broken on main either way.

A run whose jobs never got a runner (GitHub had no capacity) says nothing about
the code. It runs again, up to MAX_ATTEMPTS in all, unless a later run of the
workflow will test main anyway; re-running it then would take that later run's
place in the workflow's queue. Only after the last attempt is it reported, as
not tested, so a red main is never mistaken for a tested one.

Usage: main_status.py --repository OWNER/NAME --workflow NAME --conclusion C
                      --url URL --commit SHA --attempt N
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass

LABEL = "main-red"
# Attempts in all, the first included, for a run no runner picked up.
MAX_ATTEMPTS = 3
REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
WORKFLOW = re.compile(r"[A-Za-z0-9][A-Za-z0-9 ._-]{0,99}")
COMMIT = re.compile(r"[0-9a-f]{40}")
RUN_URL = re.compile(r"https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/actions/runs/[0-9]+")


@dataclass(frozen=True)
class Run:
    workflow: str
    conclusion: str
    url: str
    commit: str
    attempt: int = 1

    @property
    def id(self) -> int:
        """The run's ID, from its URL. Later runs have larger IDs."""
        return int(self.url.rsplit("/", 1)[1])


@dataclass(frozen=True)
class Later:
    """What later push runs of the same workflow on main say."""

    # One has already ended with a result: it, not this run, is what main is now.
    result: bool = False
    # One has a result or will have one: it tests main whatever this run does.
    run: bool = False


def title(workflow: str) -> str:
    """The title of a workflow's issue, which is how the issue is found again."""
    return f"main is red: {workflow}"


@dataclass(frozen=True)
class Open:
    body: str


@dataclass(frozen=True)
class Comment:
    issue: int
    body: str


@dataclass(frozen=True)
class Close:
    issue: int
    body: str


@dataclass(frozen=True)
class Rerun:
    run_id: int


@dataclass(frozen=True)
class Nothing:
    reason: str


Action = Open | Comment | Close | Rerun | Nothing


# Conclusions of a run that a newer commit superseded, or that never ran.
SILENT = {"cancelled", "skipped", "neutral", "stale", ""}


def decide(run: Run, open_issue: int | None, later: Later = Later(), never_started: bool = False) -> Action:
    """What one finished run on main does to its workflow's issue.

    `never_started` says every job that did not pass never got a runner."""
    if run.conclusion in SILENT:
        return Nothing(f"a {run.conclusion or 'unfinished'} run says nothing about main")
    if later.result:
        return Nothing(f"a later {run.workflow} run on main has already reported")
    if run.conclusion == "success":
        if open_issue is None:
            return Nothing(f"{run.workflow} is green and no issue is open")
        return Close(open_issue, f"{run.workflow} passed on main at {run.commit[:12]}: {run.url}")
    if never_started:
        if later.run:
            return Nothing(f"no runner picked up {run.workflow}; a later run will test main")
        if run.attempt < MAX_ATTEMPTS:
            return Rerun(run.id)
        failed = (
            f"{run.workflow} did not run on main at {run.commit[:12]}: no runner picked it up "
            f"in {run.attempt} attempts, so main is untested: {run.url}"
        )
    else:
        failed = f"{run.workflow} ended with {run.conclusion} on main at {run.commit[:12]}: {run.url}"
    if open_issue is None:
        return Open(
            f"{failed}\n\n"
            f"A passing {run.workflow} run on main closes this issue. Before starting a fix, "
            "comment here so other sessions leave it to you, and fix it in its own change."
        )
    return Comment(open_issue, failed)


class GitHub:
    """Issues and runs, through the gh CLI (or `$GH`, for tests)."""

    def __init__(self, repository: str):
        self.repository = repository
        self.gh = os.environ.get("GH", "gh")

    def _gh(self, *args: str) -> str:
        result = subprocess.run([self.gh, *args], capture_output=True, text=True)
        if result.returncode != 0:
            raise SystemExit(f"gh {args[0]} {args[1]} failed: {result.stderr.strip()}")
        return result.stdout

    def _run(self, *args: str) -> str:
        return self._gh(*args, "--repo", self.repository)

    def open_issue(self, workflow: str) -> int | None:
        listed = json.loads(
            self._run("issue", "list", "--label", LABEL, "--state", "open", "--json", "number,title")
        )
        numbers = sorted(item["number"] for item in listed if item["title"] == title(workflow))
        return numbers[0] if numbers else None

    def later(self, run: Run) -> Later:
        listed = json.loads(self._run(
            "run", "list", "--workflow", run.workflow, "--branch", "main", "--event", "push",
            "--limit", "20", "--json", "databaseId,status,conclusion",
        ))
        later = [item for item in listed if item["databaseId"] > run.id]
        superseded = [item["status"] == "completed" and item["conclusion"] in SILENT for item in later]
        return Later(
            result=any(item["status"] == "completed" and not gone for item, gone in zip(later, superseded)),
            run=not all(superseded),
        )

    def never_started(self, run: Run) -> bool:
        """Whether every job of the attempt that did not pass never got a runner."""
        jobs = json.loads(self._gh(
            "api", f"repos/{self.repository}/actions/runs/{run.id}/attempts/{run.attempt}/jobs",
        ))["jobs"]
        failed = [job for job in jobs if job["conclusion"] != "success"]
        return bool(failed) and all(not job.get("runner_name") and not job.get("steps") for job in failed)

    def apply(self, workflow: str, action: Action) -> None:
        match action:
            case Open(body):
                self._run(
                    "label", "create", LABEL, "--force", "--color", "b60205",
                    "--description", "A workflow is failing on main",
                )
                self._run("issue", "create", "--title", title(workflow), "--label", LABEL, "--body", body)
            case Comment(issue, body):
                self._run("issue", "comment", str(issue), "--body", body)
            case Close(issue, body):
                self._run("issue", "close", str(issue), "--comment", body)
            case Rerun(run_id):
                self._run("run", "rerun", str(run_id))
            case Nothing():
                pass


def describe(action: Action) -> str:
    match action:
        case Open():
            return "opened the main-red issue"
        case Comment(issue):
            return f"commented on main-red issue #{issue}"
        case Close(issue):
            return f"closed main-red issue #{issue}"
        case Rerun(run_id):
            return f"no runner picked up run {run_id}; started it again"
        case Nothing(reason):
            return f"left the issue alone: {reason}"


def arguments(argv: list[str]) -> tuple[str, Run]:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repository", required=True)
    parser.add_argument("--workflow", required=True)
    parser.add_argument("--conclusion", required=True)
    parser.add_argument("--url", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--attempt", required=True, type=int)
    parsed = parser.parse_args(argv)
    if not REPOSITORY.fullmatch(parsed.repository):
        parser.error(f"not a repository: {parsed.repository!r}")
    if not COMMIT.fullmatch(parsed.commit):
        parser.error(f"not a commit: {parsed.commit!r}")
    if not RUN_URL.fullmatch(parsed.url):
        parser.error(f"not a workflow run URL: {parsed.url!r}")
    if not WORKFLOW.fullmatch(parsed.workflow):
        parser.error(f"not a workflow name: {parsed.workflow!r}")
    if parsed.attempt < 1:
        parser.error(f"not an attempt: {parsed.attempt}")
    run = Run(parsed.workflow, parsed.conclusion, parsed.url, parsed.commit, parsed.attempt)
    return parsed.repository, run


def main(argv: list[str]) -> None:
    repository, run = arguments(argv)
    github = GitHub(repository)
    reported = run.conclusion not in SILENT
    later = github.later(run) if reported else Later()
    failed = reported and run.conclusion != "success" and not later.result
    action = decide(run, github.open_issue(run.workflow), later, failed and github.never_started(run))
    github.apply(run.workflow, action)
    print(f"{run.workflow}: {describe(action)}")


if __name__ == "__main__":
    main(sys.argv[1:])
