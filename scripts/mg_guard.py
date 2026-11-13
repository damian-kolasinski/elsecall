#!/usr/bin/env python3
"""Run mg only once what it would publish is free of private details.

A submit pushes the branch's commits, and its title and guide become the pull
request's title and description and the commit message on main. A reply's body
can reach the pull request too. Git hooks see none of the text, and mg may push
without them, so this scans all of it first.
Usage: scripts/mg_guard.py <mg arguments>. See AGENTS.md, "Public material".
"""
from __future__ import annotations

import os
import stat
import subprocess
import sys
import tempfile

import privacy_scan

# Options whose value mg publishes: as given, or the content of the file named.
TEXT_OPTIONS = frozenset({"--title", "--body", "--note"})
FILE_OPTIONS = frozenset({"--guide"})


class GuardError(Exception):
    pass


def read_guide(value: str, stdin: str | None) -> tuple[str, str]:
    """The guide's text, and a path mg can still read it from: a guide from
    stdin or a pipe is consumed here, so it is saved to a file for mg."""
    if value != "-":
        try:
            if stat.S_ISREG(os.stat(value).st_mode):
                with open(value, "rb") as file:
                    return privacy_scan.decode(file.read()), value
            with open(value, "rb") as file:
                content = privacy_scan.decode(file.read())
        except OSError as error:
            raise GuardError(f"cannot read the guide {value}: {error.strerror}") from error
    else:
        content = sys.stdin.read() if stdin is None else stdin
    with tempfile.NamedTemporaryFile("w", suffix=".md", prefix="mg-guide-", delete=False) as file:
        file.write(content)
    return content, file.name


def published(args: list[str], stdin: str | None = None) -> tuple[list[tuple[str, str]], list[str]]:
    """The texts the command would publish, by option, and the arguments to run mg with."""
    texts, run = [], []
    position = 0
    while position < len(args):
        argument = args[position]
        position += 1
        option, has_value, value = argument.partition("=")
        if argument == "--":
            run.extend(args[position - 1 :])
            break
        if option not in TEXT_OPTIONS | FILE_OPTIONS or (not has_value and position >= len(args)):
            run.append(argument)  # not published, or missing its value, which mg reports
            continue
        if not has_value:
            value = args[position]
            position += 1
        if option in FILE_OPTIONS:
            content, value = read_guide(value, stdin)
            texts.append((option.lstrip("-"), content))
        else:
            texts.append((option.lstrip("-"), value))
        run.extend([option, value])
    return texts, run


def unpushed_commits() -> list[privacy_scan.Input]:
    """The commits a submit pushes: every commit on HEAD not yet on any remote."""
    try:
        return list(privacy_scan.commits(["HEAD", "--not", "--remotes"]))
    except subprocess.CalledProcessError as error:
        raise GuardError(f"cannot list the commits to submit: {error.stderr.decode(errors='replace').strip()}") from error


def requires_terms() -> bool:
    setting = subprocess.run(
        ["git", "config", "--bool", "privacyscan.requireterms"], capture_output=True, text=True
    )
    return setting.stdout.strip() == "true"


def main(argv: list[str], environ: dict[str, str]) -> int:
    try:
        texts, run = published(argv)
        terms = privacy_scan.load_terms(environ)
        rules = privacy_scan.PUBLIC_RULES + terms.rules()
    except (GuardError, privacy_scan.TermsError) as error:
        print(f"mg-guard: {error}", file=sys.stderr)
        return 2
    if not terms.terms and requires_terms():
        print(f"mg-guard: mg not run; this clone requires the private terms in {terms.origin}.", file=sys.stderr)
        return 2
    inputs = [(source, privacy_scan.numbered(text)) for source, text in texts]
    if argv[:1] == ["submit"]:
        try:
            inputs += unpushed_commits()
        except GuardError as error:
            print(f"mg-guard: {error}", file=sys.stderr)
            return 2
    findings = [finding for source, lines in inputs for finding in privacy_scan.scan_lines(source, lines, rules)]
    for finding in findings:
        print(finding.render(reveal=True))
    if findings:
        print(
            f"mg-guard: mg not run; {len(findings)} finding(s) in what it would publish. Describe "
            "them by kind instead; see AGENTS.md, \"Public material\".",
            file=sys.stderr,
        )
        return 1
    mg = environ.get("MG", "mg")  # where `mg` is the text editor, MG=mergegate
    os.execvp(mg, [mg, *run])


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], dict(os.environ)))
