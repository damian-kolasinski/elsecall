"""Run the checks a branch's changes can affect, a faster loop than the full check.

Formatting and Clippy cover the whole workspace, as in the full check: both are
incremental and catch compile errors anywhere. Tests run only where
check_selection.py says the changed paths can be observed, or all of them when
it is unsure. Passing this is not a substitute for `mise run check`, which must
pass before work is submitted.

Usage: check_affected.py [--base REF] [--dry-run]
"""
from __future__ import annotations

import argparse
import shlex
import subprocess
import sys
from pathlib import Path

import changed_paths
import check_selection

ROOT = Path(__file__).resolve().parent.parent
FULL_CHECK = "mise run check"


def plan(selection: check_selection.Selection) -> list[list[str]]:
    commands = [
        ["cargo", "fmt", "--all", "--", "--check"],
        ["cargo", "clippy", "--workspace", "--locked", "--all-targets", "--", "-D", "warnings"],
    ]
    python = {"distribution", "scripts"} if selection.everything else selection.targets.python
    for suite in sorted(python):
        commands.append(["python3", "-m", "unittest", "discover", "-s", suite, "-p", "test_*.py"])
    tests = ["python3", "scripts/test-environment.py", "cargo", "nextest", "run", "--workspace", "--locked"]
    if selection.everything:
        commands.append(tests)
        # Only library sources hold doctests, and they select everything.
        commands.append(["python3", "scripts/test-environment.py", "cargo", "test", "--doc", "--workspace", "--locked"])
    elif selection.nextest_filter():
        commands.append(tests + ["-E", selection.nextest_filter()])
    return commands


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--base", help="compare with this revision's merge base (default: mergegate/main, origin/main or main)")
    parser.add_argument("--dry-run", action="store_true", help="print the selection and commands without running them")
    arguments = parser.parse_args()
    missing = subprocess.run(["cargo", "nextest", "--version"], cwd=ROOT, capture_output=True).returncode
    if missing and not arguments.dry_run:
        print("check:affected: cargo-nextest is missing; run 'mise install'", file=sys.stderr)
        return 1
    try:
        since, paths = changed_paths.changed(ROOT, arguments.base)
    except changed_paths.NoBase as error:
        print(f"No base revision ({error}); running every test.", file=sys.stderr)
        since, paths = None, []
        selection = check_selection.Selection(check_selection.Everything("no base revision"), check_selection.Targets(), [])
    else:
        selection = check_selection.select(ROOT, paths)
    if since:
        print(f"Changes since {since[:12]}: {len(paths)} path(s).")
    for path, reason in selection.reasons:
        print(f"  {path}: {reason}")
    commands = plan(selection)
    if selection.everything:
        print(f"Running every test: {selection.everything.reason}.")
    elif len(commands) == 2:
        print("No test can observe these changes; formatting and Clippy only.")
    for command in commands:
        print("+ " + shlex.join(command), flush=True)
        if arguments.dry_run:
            continue
        status = subprocess.run(command, cwd=ROOT).returncode
        if status != 0:
            print(f"Failed: {shlex.join(command)}", file=sys.stderr)
            return status
    if not arguments.dry_run:
        print(f"Affected checks passed. Run `{FULL_CHECK}` before submitting.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
