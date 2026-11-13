#!/usr/bin/env python3
"""Find private details before they reach this public repository.

Each finding names where a rule matched and which rule, but not the matched
text unless --reveal is given, so CI logs never republish what they catch.
AGENTS.md ("Public material") lists the neutral names to use instead.
"""
from __future__ import annotations  # macOS's own python3 is 3.9, and git GUIs run hooks with it

import argparse
import dataclasses
import functools
import ipaddress
import os
import re
import secrets
import subprocess
import sys
import unicodedata
from dataclasses import dataclass
from typing import Callable, Iterable, Iterator

Span = tuple[int, int]


@dataclass(frozen=True)
class Finding:
    source: str
    line: int
    column: int
    rule: str
    text: str

    def render(self, reveal: bool) -> str:
        where = f"{self.source}:{self.line}:{self.column}: {self.rule}"
        return f"{where}: {self.text}" if reveal else where


# Home directories, in POSIX, JSON-escaped, Windows and ~name forms. Allowed:
# the neutral accounts, shared and CI accounts, single letters, and templates
# such as <name>, $USER, {account} or "...".
HOME_NAMES = frozenset(
    {"me", "devbox", "caller", "someone", "user", "username", "name", "you", "your-name",
     "shared", "guest", "runner"}
)
HOME_PATH = re.compile(r"[\\/](?:Users|home)[\\/]+([^\\/\s\"'`)\]]+)")
TILDE_HOME = re.compile(r"(?<![\w~])~([A-Za-z][\w.-]*)(?=/)")

# macOS names a machine after its owner: "<Name>'s MacBook Pro", <Name>s-MacBook-Pro.local.
OWNER_MACHINE = re.compile(
    r"\b[A-Za-z][\w-]*?(?:['’]s |s-)(?:MacBook|Mac[ -]mini|iMac|Mac[ -]Studio|Mac[ -]Pro)",
    re.IGNORECASE,
)

# Tailscale's address space. Examples use its first and last /24 and the first
# 65,536 addresses of its IPv6 prefix. Tailscale picks real addresses at random
# across the range; sequential assignments at its start identify no one.
TAILNET_V4 = ipaddress.ip_network("100.64.0.0/10")
EXAMPLE_V4 = (ipaddress.ip_network("100.64.0.0/24"), ipaddress.ip_network("100.127.255.0/24"))
PUBLIC_V4 = frozenset({ipaddress.ip_address("100.100.100.100")})  # Tailscale's own resolver
EXAMPLE_V6 = ipaddress.ip_network("fd7a:115c:a1e0::/112")
IPV4 = re.compile(r"(?<![\d.])\d{1,3}(?:\.\d{1,3}){3}(?!\d)")
DASHED_IPV4 = re.compile(r"(?<!\d)(?<!\d-)100-\d{1,3}-\d{1,3}-\d{1,3}(?![\d])")
TAILNET_IPV6 = re.compile(r"fd7a:115c:a1e0:[0-9a-f:]*", re.IGNORECASE)

# MagicDNS names carry the tailnet's own name: <node>.<tailnet>.ts.net.
TAILNET_NAME = re.compile(r"([A-Za-z0-9-]+)\.(?:ts\.net|beta\.tailscale\.net)\b", re.IGNORECASE)
EXAMPLE_TAILNET = "example"

EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@((?:[A-Za-z0-9-]+\.)+[A-Za-z]{2,})")
EXAMPLE_EMAIL_DOMAINS = ("example.com", "example.org", "example.net", "users.noreply.github.com")
LOCAL_DOMAINS = ("local", "lan", "internal", "home.arpa")  # ssh targets, not mailboxes
PUBLIC_ADDRESSES = frozenset({"git@github.com", "git@gitlab.com", "noreply@github.com"})

CREDENTIALS = re.compile(
    r"gh[pousr]_[A-Za-z0-9]{36}"
    r"|github_pat_[A-Za-z0-9_]{22,}"
    r"|glpat-[A-Za-z0-9_-]{20,}"
    r"|tskey-[A-Za-z0-9-]{10,}"
    r"|sk-(?:ant-|proj-)?[A-Za-z0-9_-]{20,}"
    r"|AIza[0-9A-Za-z_-]{35}"
    r"|AKIA[0-9A-Z]{16}"
    r"|xox[abprs]-[A-Za-z0-9-]{10,}|xapp-[A-Za-z0-9-]{10,}"
    r"|hooks\.slack\.com/services/[A-Za-z0-9/]+"
    r"|-----BEGIN [A-Z ]*PRIVATE KEY(?: BLOCK)?-----"
)


def allowed_home(name: str) -> bool:
    return len(name) == 1 or name.lower() in HOME_NAMES or name[0] in "<${."


def home_paths(line: str) -> Iterator[Span]:
    for pattern in (HOME_PATH, TILDE_HOME):
        for match in pattern.finditer(line):
            if not allowed_home(match.group(1)):
                yield match.span()


def owner_machines(line: str) -> Iterator[Span]:
    for match in OWNER_MACHINE.finditer(line):
        yield match.span()


def is_private_tailnet_v4(text: str) -> bool:
    try:
        address = ipaddress.ip_address(text)
    except ValueError:
        return False
    return (
        address in TAILNET_V4
        and address not in PUBLIC_V4
        and not any(address in example for example in EXAMPLE_V4)
    )


def first_ipv6(text: str) -> ipaddress.IPv6Address | None:
    """The longest leading run of groups that parses, so ports and punctuation after the address drop off."""
    while text:
        try:
            return ipaddress.IPv6Address(text)
        except ValueError:
            if ":" not in text:
                return None
            text = text.rsplit(":", 1)[0]
            if text.endswith(":") and not text.endswith("::"):
                text = text[:-1]
    return None


def tailnet_addresses(line: str) -> Iterator[Span]:
    for match in IPV4.finditer(line):
        if is_private_tailnet_v4(match.group()):
            yield match.span()
    for match in DASHED_IPV4.finditer(line):
        if is_private_tailnet_v4(match.group().replace("-", ".")):
            yield match.span()
    for match in TAILNET_IPV6.finditer(line):
        address = first_ipv6(match.group())
        if address is not None and address not in EXAMPLE_V6:
            yield match.span()


def tailnet_names(line: str) -> Iterator[Span]:
    for match in TAILNET_NAME.finditer(line):
        if match.group(1).lower() != EXAMPLE_TAILNET:
            yield match.span()


def email_addresses(line: str) -> Iterator[Span]:
    for match in EMAIL.finditer(line):
        domain = match.group(1).lower()
        if match.group().lower() in PUBLIC_ADDRESSES:
            continue
        if any(domain == allowed or domain.endswith("." + allowed) for allowed in EXAMPLE_EMAIL_DOMAINS + LOCAL_DOMAINS):
            continue
        yield match.span()


def credentials(line: str) -> Iterator[Span]:
    for match in CREDENTIALS.finditer(line):
        yield match.span()


Rule = tuple[str, Callable[[str], Iterable[Span]]]

PUBLIC_RULES: tuple[Rule, ...] = (
    ("home-path", home_paths),
    ("owner-machine", owner_machines),
    ("tailnet-address", tailnet_addresses),
    ("tailnet-name", tailnet_names),
    ("email", email_addresses),
    ("credential", credentials),
)


# Private terms: the employer, client, machine and account names that make this
# repository's own leaks. Listing them here would publish them, so they live
# outside it: in $PRIVACY_SCAN_TERMS (CI, from a secret) or a file, one per line.
TERMS_VARIABLE = "PRIVACY_SCAN_TERMS"
TERMS_FILE_VARIABLE = "PRIVACY_SCAN_TERMS_FILE"
DEFAULT_TERMS_FILE = "~/.config/elsecall/private-terms"

# Between a term's characters, a match may have a few separators, so a term
# also catches its spaced, dashed, dotted and underscored spellings.
SEPARATOR = r"[\s_./-]{0,3}"
SEPARATORS = re.compile(r"[\s_./-]")
# Letters without a decomposition, folded to their base letter; invisible
# joiners and soft hyphens fold to a separator. One character for one.
FOLDED = str.maketrans(
    {"ł": "l", "Ł": "l", "đ": "d", "Đ": "d", "ø": "o", "Ø": "o", "ı": "i", "ħ": "h", "Ħ": "h",
     "­": "-", "​": "-", "‌": "-", "‍": "-", "⁠": "-", "﻿": "-"}
)


class TermsError(Exception):
    pass


@functools.lru_cache(maxsize=16)
def fold(text: str) -> str:
    """Lowercase without diacritics, one character for one, so spans found in
    the folded text are spans of the original. Cached: every term folds the same line."""
    folded = []
    for character in text.translate(FOLDED):
        base = [part for part in unicodedata.normalize("NFKD", character) if not unicodedata.combining(part)]
        folded.append(base[0].lower()[:1] if len(base) == 1 else character.lower()[:1] or character)
    return "".join(folded)


def term_rule(term: str) -> Callable[[str], Iterable[Span]]:
    letters = [character for character in fold(term) if not SEPARATORS.fullmatch(character)]
    if not letters:
        raise TermsError("a private term has no letters or digits; remove it")
    pattern = re.compile(SEPARATOR.join(re.escape(character) for character in letters))
    return lambda line: (match.span() for match in pattern.finditer(fold(line)))


@dataclass(frozen=True)
class Terms:
    terms: tuple[tuple[int, str], ...]  # (line in the list, term)
    origin: str  # where they came from, or where they were looked for

    def rules(self) -> tuple[Rule, ...]:
        """One rule per term, named by its line in the list, so a report
        locates the term for its owner without printing it."""
        return tuple((f"private-term {number}", term_rule(term)) for number, term in self.terms)


def parse_terms(text: str) -> tuple[tuple[int, str], ...]:
    lines = ((number, line.strip()) for number, line in enumerate(text.splitlines(), start=1))
    return tuple((number, line) for number, line in lines if line and not line.startswith("#"))


def load_terms(environ: dict[str, str]) -> Terms:
    if environ.get(TERMS_VARIABLE, "").strip():
        return Terms(parse_terms(environ[TERMS_VARIABLE]), f"${TERMS_VARIABLE}")
    path = os.path.expanduser(environ.get(TERMS_FILE_VARIABLE) or DEFAULT_TERMS_FILE)
    try:
        with open(path, encoding="utf-8") as file:
            return Terms(parse_terms(file.read()), path)
    except FileNotFoundError:
        return Terms((), path)
    except (OSError, UnicodeDecodeError) as error:
        raise TermsError(f"cannot read the private terms in {path}: {error}") from error


# An input is a named sequence of numbered lines: a file, a commit message, a
# list of paths, or the lines a commit adds to one file, numbered as in that file.
Lines = Iterable[tuple[int, str]]
Input = tuple[str, Lines]


def scan_lines(source: str, lines: Lines, rules: Iterable[Rule]) -> Iterator[Finding]:
    rules = tuple(rules)
    for number, line in lines:
        for name, rule in rules:
            for start, end in rule(line):
                yield Finding(source, number, start + 1, name, line[start:end])


def numbered(text: str) -> Lines:
    """Lines as git counts them: split on newlines only."""
    return enumerate(text.split("\n"), start=1)


def scan_text(source: str, text: str, rules: Iterable[Rule] = PUBLIC_RULES) -> list[Finding]:
    return list(scan_lines(source, numbered(text), rules))


def decode(data: bytes) -> str:
    """Text to scan. Binary content is scanned too, since compiled files and
    image metadata carry paths and names: its ASCII survives decoding."""
    return data.decode("utf-8", errors="replace")


def git(*args: str) -> bytes:
    return subprocess.run(["git", "-c", "core.quotePath=false", *args], check=True, capture_output=True).stdout


def to_top_level() -> None:
    os.chdir(git("rev-parse", "--show-toplevel").decode().strip())


def path_list(source: str, paths: list[str]) -> Input:
    """File names leak as readily as contents."""
    return source, enumerate(paths, start=1)


def tracked_files() -> Iterator[Input]:
    to_top_level()
    paths = [path for path in git("ls-files", "-z").decode(errors="replace").split("\0") if path]
    yield path_list("tracked paths", paths)
    for path in paths:
        try:
            with open(path, "rb") as file:
                yield path, numbered(decode(file.read()))
        except (FileNotFoundError, IsADirectoryError):
            continue  # deleted in the working tree, or a submodule


def staged_files() -> Iterator[Input]:
    """What the next commit records: the index's blob for each added or changed
    path, read by object ID so no path is mistaken for a revision."""
    to_top_level()
    raw = git("diff", "--cached", "--raw", "-z", "--no-abbrev", "--no-renames", "--diff-filter=d")
    fields = raw.decode(errors="replace").split("\0")
    entries = []
    for status, path in zip(fields[0::2], fields[1::2]):
        _, new_mode, _, blob, _ = status.lstrip(":").split(" ")
        entries.append((new_mode, blob, path))
    yield path_list("staged paths", [path for _, _, path in entries])
    for mode, blob, path in entries:
        if mode != "160000":  # a submodule's entry is a commit, not content
            yield path, numbered(decode(git("cat-file", "blob", blob)))


def named_files(paths: list[str], name: str | None) -> Iterator[Input]:
    for path in paths:
        if path == "-":
            yield name or "stdin", numbered(sys.stdin.read())
            continue
        with open(path, "rb") as file:
            yield name or path, numbered(decode(file.read()))


SCISSORS = "# ------------------------ >8 ------------------------"


def commit_message(path: str) -> Iterator[Input]:
    """A message being committed, comments included, since `commit -m` keeps
    them. Only the diff that `commit -v` shows below the scissors is left out."""
    with open(path, "rb") as file:
        lines = decode(file.read()).split("\n")
    if SCISSORS in lines:
        lines = lines[: lines.index(SCISSORS)]
    yield "commit message", enumerate(lines, start=1)


HUNK = re.compile(r"^@@ -\d+(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")


ESCAPES = {"a": 7, "b": 8, "t": 9, "n": 10, "v": 11, "f": 12, "r": 13, '"': 34, "\\": 92}


def unquote(path: str) -> str:
    """A path as git prints it, without its C-style quoting of unusual names."""
    if not (len(path) >= 2 and path.startswith('"') and path.endswith('"')):
        return path
    body, raw, position = path[1:-1], bytearray(), 0
    while position < len(body):
        character = body[position]
        if character == "\\" and position + 1 < len(body):
            following = body[position + 1]
            if following in "01234567":
                raw.append(int(body[position + 1 : position + 4], 8))
                position += 4
                continue
            raw.append(ESCAPES.get(following, ord(following)))
            position += 2
            continue
        raw.extend(character.encode())
        position += 1
    return raw.decode("utf-8", errors="replace")


def diff_path(header: str) -> str:
    path = unquote(header[4:].rstrip("\t"))  # git ends a path with spaces with a tab
    return path[2:] if path.startswith(("a/", "b/")) else path


def raw_path(line: str) -> str:
    """The path of a `--raw` line, ":<modes> <objects> <status>\t<path>"."""
    return unquote(line.partition("\t")[2])


def added_lines(patch: Iterable[str]) -> Iterator[tuple[str, int, str]]:
    """(path, line number in the new file, text) for each line a unified diff
    adds. Hunk sizes, not line prefixes, say where a hunk ends, so an added
    line that starts with "++ " is still content."""
    path, number, old_left, new_left = "", 0, 0, 0
    for line in patch:
        if old_left or new_left:
            if line.startswith("+"):
                yield path, number, line[1:]
                number += 1
                new_left -= 1
            elif line.startswith("-"):
                old_left -= 1
            elif line.startswith(" "):
                number += 1
                old_left -= 1
                new_left -= 1
            continue  # "\ No newline at end of file" counts as neither
        if line.startswith("+++ "):
            path = diff_path(line)
        elif hunk := HUNK.match(line):
            old_count, start, new_count = hunk.groups()
            old_left = 1 if old_count is None else int(old_count)
            new_left = 1 if new_count is None else int(new_count)
            number = int(start)


def commits(revisions: list[str]) -> Iterator[Input]:
    """Each commit's author and committer, message, the paths it writes and the
    lines it adds, since a pull request shows every commit even after a squash
    merge. Binary and textconv'd files are diffed as text, merges against their
    first parent, and paths come from --raw, which lists empty files too, so
    nothing a commit adds goes unseen."""
    marker = f"privacy-scan-{secrets.token_hex(8)}"
    start, end = f"\n{marker} commit ", f"\n{marker} end\n"
    log = git(
        "log", f"--format=%n{marker} commit %H%n%an <%ae>%n%cn <%ce>%n%B%n{marker} end",
        "--raw", "--patch", "--unified=0", "--text", "--no-textconv", "--no-color", "--no-ext-diff",
        "--src-prefix=a/", "--dst-prefix=b/", "--no-renames", "--diff-merges=first-parent", *revisions, "--",
    )
    for record in ("\n" + decode(log)).split(start)[1:]:
        header, _, diff = record.partition(end)
        sha, _, message = header.partition("\n")
        commit = f"commit {sha[:12]}"
        yield commit, numbered(message)
        lines = diff.split("\n")
        yield path_list(f"{commit} paths", [raw_path(line) for line in lines if line.startswith(":")])
        by_path: dict[str, list[tuple[int, str]]] = {}
        for path, number, line in added_lines(lines):
            by_path.setdefault(path, []).append((number, line))
        for path, added in by_path.items():
            yield f"{commit} {path}", added


ZERO = re.compile(r"^0+$")


def object_type(name: str) -> str:
    return git("cat-file", "-t", name).decode().strip()


def tree_contents(label: str, tree: str) -> Iterator[Input]:
    entries = [entry for entry in git("ls-tree", "-r", "-z", tree).decode(errors="replace").split("\0") if entry]
    blobs = [(info.split(" ")[2], path) for info, _, path in (entry.partition("\t") for entry in entries)
             if info.split(" ")[1] == "blob"]
    yield path_list(f"{label} paths", [path for _, path in blobs])
    for blob, path in blobs:
        yield f"{label} {path}", numbered(decode(git("cat-file", "blob", blob)))


def pushed(updates: str) -> Iterator[Input]:
    """What a push would publish, from the lines git gives a pre-push hook: the
    ref names, tag messages, and every commit not yet on any remote. Remotes
    are matched by their tracking refs, not by name, since a push may name its
    remote by URL."""
    refs = []
    for line in updates.splitlines():
        if not line.strip():
            continue
        _, local_sha, remote_ref, remote_sha = line.split(" ")
        refs.append(remote_ref)
        if ZERO.match(local_sha):
            continue  # a deletion publishes nothing
        target, kind = local_sha, object_type(local_sha)
        if kind == "tag":
            yield f"tag {remote_ref}", numbered(decode(git("cat-file", "tag", local_sha)))
            target = git("rev-parse", f"{local_sha}^{{}}").decode().strip()
            kind = object_type(target)
        if kind == "commit":
            known = not ZERO.match(remote_sha) and subprocess.run(
                ["git", "cat-file", "-e", f"{remote_sha}^{{commit}}"], capture_output=True
            ).returncode == 0
            yield from commits([f"{remote_sha}..{target}"] if known else [target, "--not", "--remotes"])
        elif kind == "blob":
            yield f"{remote_ref} blob", numbered(decode(git("cat-file", "blob", target)))
        elif kind == "tree":
            yield from tree_contents(f"{remote_ref} tree", target)
    yield path_list("pushed refs", refs)


def redact(text: str, rules: tuple[Rule, ...]) -> str:
    """A source label without what the rules match in it, such as a file named
    after a private term, for reports that must not reveal."""
    spans = sorted({span for _, rule in rules for span in rule(text)}, reverse=True)
    for start, end in spans:
        text = text[:start] + "<withheld>" + text[end:]
    return text


def main(argv: list[str], environ: dict[str, str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--reveal", action="store_true", help="also print the matched text")
    parser.add_argument(
        "--require-terms", action="store_true", help="fail when no private terms are configured"
    )
    modes = parser.add_subparsers(dest="mode", required=True)
    modes.add_parser("tree", help="scan every tracked file and its path")
    modes.add_parser("staged", help="scan the files and paths staged for the next commit")
    message = modes.add_parser("message", help="scan a commit message file, as the commit-msg hook gets it")
    message.add_argument("path")
    history = modes.add_parser("commits", help="scan the commits git log selects, such as BASE..HEAD")
    history.add_argument("revisions", nargs="+")
    modes.add_parser("push", help="scan what a push publishes, given the pre-push hook's stdin")
    files = modes.add_parser("files", help="scan the given files; - reads stdin")
    files.add_argument("--name", help="the name to report the input under")
    files.add_argument("paths", nargs="+")
    options = parser.parse_args(argv)

    try:
        terms = load_terms(environ)
        rules = PUBLIC_RULES + terms.rules()
    except TermsError as error:
        print(f"privacy-scan: {error}", file=sys.stderr)
        return 2
    if options.require_terms and not terms.terms:
        print(
            f"privacy-scan: no private terms configured; set ${TERMS_VARIABLE} or list them in {terms.origin}.",
            file=sys.stderr,
        )
        return 2

    inputs: Iterable[Input]
    if options.mode == "tree":
        inputs = tracked_files()
    elif options.mode == "staged":
        inputs = staged_files()
    elif options.mode == "message":
        inputs = commit_message(options.path)
    elif options.mode == "commits":
        inputs = commits(options.revisions)
    elif options.mode == "push":
        inputs = pushed(sys.stdin.read())
    else:
        inputs = named_files(options.paths, options.name)

    findings = []
    scanned = 0
    for source, lines in inputs:
        scanned += 1
        findings.extend(scan_lines(source, lines, rules))

    for finding in findings:
        if not options.reveal:
            finding = dataclasses.replace(finding, source=redact(finding.source, rules))
        print(finding.render(options.reveal))
    coverage = (
        f"public rules and {len(terms.terms)} private term(s) from {terms.origin}"
        if terms.terms
        else f"public rules only; no private terms in {terms.origin}"
    )
    if findings:
        print(
            f"privacy-scan: {len(findings)} finding(s) in {scanned} input(s), {coverage}. Replace them "
            "with the neutral names in AGENTS.md, \"Public material\".",
            file=sys.stderr,
        )
        return 1
    print(f"privacy-scan: clean, {scanned} input(s), {coverage}.", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], dict(os.environ)))
