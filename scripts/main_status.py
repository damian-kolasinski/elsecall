#!/usr/bin/env python3
"""Keep one GitHub issue open per workflow that is red on main.

`.github/workflows/main-status.yml` runs this after each run of a watched
workflow on main finishes. Each workflow has its own issue, titled
`main is red: <workflow>` and labelled main-red, so a pass in one never closes
another's. A run that did not pass opens its workflow's issue, or comments on
the one already open; a passing run closes it. A cancelled or skipped run says
nothing about main and changes nothing: a newer commit superseded it. Neither
does a run that finished after a later run of its workflow already reported,
since runs of different pushes can finish out of order. A
timed-out run, or one that could not start, counts as failed: the workflow is
broken on main either way.

Usage: main_status.py --repository OWNER/NAME --workflow NAME --conclusion C --url URL --commit SHA
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

    @property
    def id(self) -> int:
        """The run's ID, from its URL. Later runs have larger IDs."""
        return int(self.url.rsplit("/", 1)[1])


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
class Nothing:
    reason: str


Action = Open | Comment | Close | Nothing


# Conclusions of a run that a newer commit superseded, or that never ran.
SILENT = {"cancelled", "skipped", "neutral", "stale", ""}


def decide(run: Run, open_issue: int | None, newer_result: bool = False) -> Action:
    """What one finished run on main does to its workflow's issue.

    `newer_result` says a later run of the workflow on main has already ended
    with a result: runs of different pushes can finish out of order, and the
    later one is what main is now."""
    if run.conclusion in SILENT:
        return Nothing(f"a {run.conclusion or 'unfinished'} run says nothing about main")
    if newer_result:
        return Nothing(f"a later {run.workflow} run on main has already reported")
    if run.conclusion == "success":
        if open_issue is None:
            return Nothing(f"{run.workflow} is green and no issue is open")
        return Close(open_issue, f"{run.workflow} passed on main at {run.commit[:12]}: {run.url}")
    failed = f"{run.workflow} ended with {run.conclusion} on main at {run.commit[:12]}: {run.url}"
    if open_issue is None:
        return Open(
            f"{failed}\n\n"
            f"A passing {run.workflow} run on main closes this issue. Before starting a fix, "
            "comment here so other sessions leave it to you, and fix it in its own change."
        )
    return Comment(open_issue, failed)


class GitHub:
    """The issue operations, through the gh CLI (or `$GH`, for tests)."""

    def __init__(self, repository: str):
        self.repository = repository
        self.gh = os.environ.get("GH", "gh")

    def _run(self, *args: str) -> str:
        result = subprocess.run(
            [self.gh, *args, "--repo", self.repository], capture_output=True, text=True
        )
        if result.returncode != 0:
            raise SystemExit(f"gh {args[0]} {args[1]} failed: {result.stderr.strip()}")
        return result.stdout

    def open_issue(self, workflow: str) -> int | None:
        listed = json.loads(
            self._run("issue", "list", "--label", LABEL, "--state", "open", "--json", "number,title")
        )
        numbers = sorted(item["number"] for item in listed if item["title"] == title(workflow))
        return numbers[0] if numbers else None

    def newer_result(self, run: Run) -> bool:
        """Whether a later push run of the workflow on main has ended with a result."""
        listed = json.loads(self._run(
            "run", "list", "--workflow", run.workflow, "--branch", "main", "--event", "push",
            "--status", "completed", "--limit", "20", "--json", "databaseId,conclusion",
        ))
        return any(item["databaseId"] > run.id and item["conclusion"] not in SILENT for item in listed)

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
        case Nothing(reason):
            return f"left the issue alone: {reason}"


def arguments(argv: list[str]) -> tuple[str, Run]:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repository", required=True)
    parser.add_argument("--workflow", required=True)
    parser.add_argument("--conclusion", required=True)
    parser.add_argument("--url", required=True)
    parser.add_argument("--commit", required=True)
    parsed = parser.parse_args(argv)
    if not REPOSITORY.fullmatch(parsed.repository):
        parser.error(f"not a repository: {parsed.repository!r}")
    if not COMMIT.fullmatch(parsed.commit):
        parser.error(f"not a commit: {parsed.commit!r}")
    if not RUN_URL.fullmatch(parsed.url):
        parser.error(f"not a workflow run URL: {parsed.url!r}")
    if not WORKFLOW.fullmatch(parsed.workflow):
        parser.error(f"not a workflow name: {parsed.workflow!r}")
    return parsed.repository, Run(parsed.workflow, parsed.conclusion, parsed.url, parsed.commit)


def main(argv: list[str]) -> None:
    repository, run = arguments(argv)
    github = GitHub(repository)
    newer = run.conclusion not in SILENT and github.newer_result(run)
    action = decide(run, github.open_issue(run.workflow), newer)
    github.apply(run.workflow, action)
    print(f"{run.workflow}: {describe(action)}")


if __name__ == "__main__":
    main(sys.argv[1:])
