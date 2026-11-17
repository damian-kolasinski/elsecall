"""Paths a branch changes, relative to the repository root.

Committed changes since the branch left its base, staged and unstaged changes,
and untracked files. Renames contribute both names: tests naming the old one are
affected too.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

# The target as the review hub has it, then the usual remote and local names.
BASES = ("mergegate/main", "origin/main", "main")


class NoBase(Exception):
    """No base revision to compare with."""


def git(root: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=root, check=True, capture_output=True, text=True
    ).stdout


def merge_base(root: Path, base: str | None) -> str:
    for candidate in [base] if base else BASES:
        try:
            return git(root, "merge-base", "HEAD", candidate).strip()
        except subprocess.CalledProcessError:
            continue
    raise NoBase(base or " or ".join(BASES))


def changed(root: Path, base: str | None = None) -> tuple[str, list[str]]:
    """The merge base used and the paths changed since it."""
    since = merge_base(root, base)
    paths: set[str] = set()
    # Against the working tree: commits, staged and unstaged changes together.
    fields = iter(git(root, "diff", "--name-status", "-M", "-z", since).split("\0"))
    for status in fields:
        if not status:
            continue
        # Renames and copies name the source, then the destination.
        paths.add(next(fields))
        if status[0] in "RC":
            paths.add(next(fields))
    paths |= set(git(root, "ls-files", "--others", "--exclude-standard", "-z").split("\0"))
    paths.discard("")
    return since, sorted(paths)
