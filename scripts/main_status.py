#!/usr/bin/env python3
"""Keep one GitHub issue open while main's Check is red.

`.github/workflows/main-status.yml` runs this after each Check run on main
finishes. A run that did not pass opens the issue, or comments on the one
already open; a passing run closes it. A cancelled or skipped run says nothing
about main and changes nothing: a newer commit superseded it. A timed-out run,
or one that could not start, counts as failed: main's Check is broken either way.

Usage: main_status.py --repository OWNER/NAME --conclusion C --url URL --commit SHA
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
TITLE = "main is red"
REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
COMMIT = re.compile(r"[0-9a-f]{40}")
RUN_URL = re.compile(r"https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/actions/runs/[0-9]+")


@dataclass(frozen=True)
class Run:
    conclusion: str
    url: str
    commit: str


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


def decide(run: Run, open_issue: int | None) -> Action:
    """What one finished Check run on main does to the issue."""
    if run.conclusion in SILENT:
        return Nothing(f"a {run.conclusion or 'unfinished'} run says nothing about main")
    if run.conclusion == "success":
        if open_issue is None:
            return Nothing("main is green and no issue is open")
        return Close(open_issue, f"Check passed on main at {run.commit[:12]}: {run.url}")
    failed = f"Check ended with {run.conclusion} on main at {run.commit[:12]}: {run.url}"
    if open_issue is None:
        return Open(
            f"{failed}\n\n"
            "A passing Check run on main closes this issue. Before starting a fix, "
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

    def open_issue(self) -> int | None:
        listed = json.loads(
            self._run("issue", "list", "--label", LABEL, "--state", "open", "--json", "number")
        )
        numbers = sorted(item["number"] for item in listed)
        return numbers[0] if numbers else None

    def apply(self, action: Action) -> None:
        match action:
            case Open(body):
                self._run(
                    "label", "create", LABEL, "--force", "--color", "b60205",
                    "--description", "Check is failing on main",
                )
                self._run("issue", "create", "--title", TITLE, "--label", LABEL, "--body", body)
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
    return parsed.repository, Run(parsed.conclusion, parsed.url, parsed.commit)


def main(argv: list[str]) -> None:
    repository, run = arguments(argv)
    github = GitHub(repository)
    action = decide(run, github.open_issue())
    github.apply(action)
    print(describe(action))


if __name__ == "__main__":
    main(sys.argv[1:])
