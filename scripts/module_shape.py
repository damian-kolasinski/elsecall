#!/usr/bin/env python3
"""Keep Rust modules small, folders shaped by responsibility, and names specific.

Checks every Rust file in the repository (tracked, or new and not ignored):

- A source file over 800 lines, not counting `#[cfg(test)]` code, warns; over
  1,500 it fails. Test files (under a `tests/` directory, `tests.rs`,
  `*_tests.rs`) warn over 1,000 lines and never fail.
- A folder holding over 40 source files directly warns; over 60 it fails.
  Test files are not counted.
- A module named for no responsibility (`core`, `utils`, `helpers`, ...) fails.

Older violations are listed in `scripts/module-shape-exceptions.txt`, each with
the ceiling it stood at. A listed file or folder fails only above its ceiling,
so it can shrink but not grow. Ceilings only go down: `--shrink` lowers them to
what the code measures now and drops resolved entries; nothing adds or raises
one. With a base (`--base REF`, else the merge base with `mergegate/main` when
that remote exists), a file renamed since the base keeps its ceiling, so a move
is never also a split; and the list may not gain or raise an entry against the
base's list (from the first base that has this guard on).

A listed path that no longer exists fails, so a move rewrites the list in the
same change. With a base, warnings show only for what the change touched.

Usage: module_shape.py [--base REF] [--shrink] [--all]
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

SOURCE_SOFT, SOURCE_HARD = 800, 1500
TEST_SOFT = 1000
FOLDER_SOFT, FOLDER_HARD = 40, 60
# Names that say where code sits rather than what it is responsible for. A
# single `helper` is allowed: it names one thing, such as a helper process.
CATCH_ALL = frozenset({"core", "util", "utils", "helpers", "common", "misc", "shared", "design_system"})
EXCEPTIONS = "scripts/module-shape-exceptions.txt"
GUARD = "scripts/module_shape.py"
LINES, FILES = "lines", "files"
HEADER = (
    "# <kind> <path> <ceiling>: violations older than scripts/module_shape.py, at the size\n"
    "# they stood at. `lines`: non-test lines of a Rust file; `files`: source files directly\n"
    "# in a folder. Ceilings only go down: scripts/module_shape.py --shrink lowers them.\n"
)


# --- Measuring Rust sources ---------------------------------------------------


@dataclass
class Lexer:
    """What carries from one line to the next: an open block comment's depth,
    or an open string's closing quote and whether escapes count in it."""

    comment_depth: int = 0
    closing: str | None = None
    escapes: bool = True

    def in_code(self) -> bool:
        return self.comment_depth == 0 and self.closing is None


def starts_word(line: str, i: int) -> bool:
    return i == 0 or not (line[i - 1].isalnum() or line[i - 1] == "_")


def scan(line: str, lexer: Lexer) -> tuple[int, int, str]:
    """Net `{` minus `}` in the code on `line`, net `(`/`[` minus `)`/`]`, and
    the last code character on it."""
    delta, parens, last, i, n = 0, 0, "", 0, len(line)
    while i < n:
        c = line[i]
        if lexer.comment_depth:
            if line.startswith("*/", i):
                lexer.comment_depth -= 1
                i += 2
            elif line.startswith("/*", i):
                lexer.comment_depth += 1
                i += 2
            else:
                i += 1
            continue
        if lexer.closing is not None:
            if lexer.escapes and c == "\\":
                i += 2
            elif line.startswith(lexer.closing, i):
                i += len(lexer.closing)
                lexer.closing = None
            else:
                i += 1
            continue
        if line.startswith("//", i):
            break
        if line.startswith("/*", i):
            lexer.comment_depth = 1
            i += 2
            continue
        # Raw strings: r"..", r#".."#, br#".."#. No escapes inside.
        prefix = 2 if line.startswith("br", i) else 1 if c == "r" else 0
        if prefix and starts_word(line, i):
            j = i + prefix
            while j < n and line[j] == "#":
                j += 1
            if j < n and line[j] == '"':
                lexer.closing, lexer.escapes = '"' + "#" * (j - i - prefix), False
                i = j + 1
                continue
        if c == '"':
            lexer.closing, lexer.escapes = '"', True
            i += 1
            continue
        if c == "'":
            # A char literal ('x', '\'', '\u{1F600}'), not a lifetime ('a).
            if i + 3 < n and line[i + 1] == "\\":
                end = line.find("'", i + 3)
                i = end + 1 if end != -1 else n
                last = "'"
                continue
            if i + 2 < n and line[i + 2] == "'":
                i += 3
                last = "'"
                continue
        if c == "{":
            delta += 1
        elif c == "}":
            delta -= 1
        elif c in "([":
            parens += 1
        elif c in ")]":
            parens -= 1
        if not c.isspace():
            last = c
        i += 1
    return delta, parens, last


# `#[cfg(test)]`, and `#[cfg(all(test, ..))]`, which is test code too.
TEST_ATTRIBUTE = re.compile(r"#\[cfg\((?:test|all\(\s*test\b.*?\))\)\]")


def source_lines(text: str) -> int:
    """Lines of a Rust source outside test-only items."""
    return len(production_lines(text))


def production_lines(text: str) -> list[tuple[int, str]]:
    """The lines of a Rust source outside test-only items, with their 1-based
    line numbers, in order."""
    lexer = Lexer()
    production: list[tuple[int, str]] = []
    # code | attribute (test cfg seen, item not yet open) | item (inside it)
    phase, depth, parens = "code", 0, 0
    for number, line in enumerate(text.splitlines(), 1):
        stripped = line.strip()
        if phase == "code":
            attribute = TEST_ATTRIBUTE.match(stripped) if lexer.in_code() else None
            if attribute is None:
                production.append((number, line))
                scan(line, lexer)
                continue
            phase, parens = "attribute", 0
            line = stripped[attribute.end():]
            stripped = line.strip()
            if not stripped:
                continue
        if phase == "attribute":
            if stripped.startswith("#[") and parens == 0 and "{" not in stripped:
                continue
            delta, opened, last = scan(line, lexer)
            parens += opened
            if delta > 0:
                phase, depth = "item", delta
            elif delta < 0:
                # The test-only item was the last field or variant: this line
                # closes the enclosing production item.
                production.append((number, line))
                phase = "code"
            elif parens <= 0 and last in (";", ",", "}"):
                # A statement, a field or variant, or a one-line item ended.
                phase = "code"
            continue
        delta, _, _ = scan(line, lexer)
        depth += delta
        if depth <= 0:
            phase, depth = "code", 0
    return production


def is_test_file(path: str) -> bool:
    parts = PurePosixPath(path).parts
    name = parts[-1]
    return "tests" in parts[:-1] or name == "tests.rs" or name.endswith("_tests.rs")


# --- The repository's shape ---------------------------------------------------


@dataclass(frozen=True)
class Shape:
    sources: dict[str, int]  # path -> non-test lines
    tests: dict[str, int]  # path -> lines
    folders: dict[str, int]  # folder -> source files directly in it
    catch_all: tuple[str, ...]  # module paths named for no responsibility


def catch_all_name(name: str) -> bool:
    return name.replace("-", "_") in CATCH_ALL


def module_name(path: str) -> str:
    pure = PurePosixPath(path)
    return pure.parent.name if pure.name == "mod.rs" else pure.stem


def shape_of(files: dict[str, str]) -> Shape:
    """The shape of Rust files given as path -> text."""
    sources, tests = {}, {}
    for path, text in files.items():
        if is_test_file(path):
            tests[path] = len(text.splitlines())
        else:
            sources[path] = source_lines(text)
    folders = Counter(str(PurePosixPath(path).parent) for path in sources)
    catch_all = set()
    # Test code may share fixtures through `tests/common`, as Rust's own
    # integration-test layout does; production modules may not.
    for path in sources:
        parts = PurePosixPath(path).parts
        for index, part in enumerate(parts[:-1]):
            if catch_all_name(part):
                catch_all.add("/".join(parts[: index + 1]))
        if catch_all_name(module_name(path)) and PurePosixPath(path).name != "mod.rs":
            catch_all.add(path)
    return Shape(sources, tests, dict(folders), tuple(sorted(catch_all)))


# --- Exceptions and findings --------------------------------------------------

Key = tuple[str, str]  # (kind, path)


def parse_exceptions(text: str) -> dict[Key, int]:
    entries = {}
    for number, line in enumerate(text.splitlines(), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        fields = line.split()
        if len(fields) != 3 or fields[0] not in (LINES, FILES) or not fields[2].isdigit():
            raise SystemExit(f"{EXCEPTIONS}:{number}: expected `<lines|files> <path> <ceiling>`, got {line!r}")
        entries[(fields[0], fields[1])] = int(fields[2])
    return entries


def format_exceptions(entries: dict[Key, int]) -> str:
    return HEADER + "".join(f"{kind} {path} {ceiling}\n" for (kind, path), ceiling in sorted(entries.items()))


def ceiling(entries: dict[Key, int], key: Key, renames: dict[str, str]) -> int | None:
    """A listed ceiling, following the file from the path it was renamed from."""
    if key in entries:
        return entries[key]
    if key[0] == LINES and key[1] in renames:
        return entries.get((LINES, renames[key[1]]))
    return None


@dataclass(frozen=True)
class Finding:
    error: bool
    path: str
    message: str


def findings(shape: Shape, entries: dict[Key, int], renames: dict[str, str]) -> list[Finding]:
    found = []
    for path, lines in shape.sources.items():
        if lines <= SOURCE_SOFT:
            continue
        limit = ceiling(entries, (LINES, path), renames)
        if lines > SOURCE_HARD and (limit is None or lines > limit):
            allowed = f"its ceiling of {limit}" if limit is not None else f"{SOURCE_HARD}"
            found.append(Finding(True, path, f"{lines} non-test lines, over {allowed}: split a module out before adding more"))
        else:
            found.append(Finding(False, path, f"{lines} non-test lines (soft limit {SOURCE_SOFT})"))
    for path, lines in shape.tests.items():
        if lines > TEST_SOFT:
            found.append(Finding(False, path, f"test file has {lines} lines (soft limit {TEST_SOFT})"))
    for folder, count in shape.folders.items():
        if count <= FOLDER_SOFT:
            continue
        limit = ceiling(entries, (FILES, folder), renames)
        if count > FOLDER_HARD and (limit is None or count > limit):
            allowed = f"its ceiling of {limit}" if limit is not None else f"{FOLDER_HARD}"
            found.append(Finding(True, folder, f"{count} source files, over {allowed}: group them into folders by responsibility"))
        else:
            found.append(Finding(False, folder, f"{count} source files (soft limit {FOLDER_SOFT})"))
    for path in shape.catch_all:
        found.append(Finding(True, path, "named for no responsibility: name the module for what it owns"))
    found += listed(shape, entries)
    return sorted(found, key=lambda f: (not f.error, f.path))


def listed(shape: Shape, entries: dict[Key, int]) -> list[Finding]:
    """Entries the code has moved away from. A missing path is an error: left in
    the list, it would leave the moved file unlisted for every later change."""
    found = []
    for (kind, path), limit in sorted(entries.items()):
        measured = (shape.sources if kind == LINES else shape.folders).get(path)
        hard = SOURCE_HARD if kind == LINES else FOLDER_HARD
        if measured is None:
            found.append(Finding(True, EXCEPTIONS, (
                f"lists `{kind} {path}`, which no longer exists. If it moved, stage the move "
                "(`git mv`) so its ceiling follows it, then run scripts/module_shape.py --shrink"
            )))
        elif measured <= hard:
            found.append(Finding(False, EXCEPTIONS, f"`{kind} {path}` is back within limits: run --shrink to drop it"))
        elif measured < limit:
            found.append(Finding(False, EXCEPTIONS, f"`{kind} {path}` measures {measured}: run --shrink to lower its ceiling from {limit}"))
    return found


def ratchet(entries: dict[Key, int], base: dict[Key, int], renames: dict[str, str]) -> list[Finding]:
    """Entries the list gained or raised against the base's list."""
    found = []
    for key, value in sorted(entries.items()):
        allowed = ceiling(base, key, renames)
        if allowed is None:
            found.append(Finding(True, EXCEPTIONS, f"adds `{key[0]} {key[1]}`: fix the violation instead"))
        elif value > allowed:
            found.append(Finding(True, EXCEPTIONS, f"raises `{key[0]} {key[1]}` from {allowed} to {value}: ceilings only go down"))
    return found


def shrunk(shape: Shape, entries: dict[Key, int], renames: dict[str, str]) -> dict[Key, int]:
    """The list lowered to what the code measures now. Never adds or raises."""
    measured = {(LINES, path): lines for path, lines in shape.sources.items() if lines > SOURCE_HARD}
    measured |= {(FILES, folder): count for folder, count in shape.folders.items() if count > FOLDER_HARD}
    result = {}
    for key, value in measured.items():
        limit = ceiling(entries, key, renames)
        if limit is not None:
            result[key] = min(value, limit)
    return result


# --- git ----------------------------------------------------------------------


def git(root: Path, *args: str, check: bool = True) -> str:
    result = subprocess.run(["git", *args], cwd=root, capture_output=True, text=True)
    if check and result.returncode != 0:
        raise SystemExit(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout if result.returncode == 0 else ""


def rust_files(root: Path) -> dict[str, str]:
    listed = git(root, "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", "*.rs")
    files = {}
    for path in sorted(set(filter(None, listed.split("\0")))):
        if path.startswith("target/") or not (root / path).is_file():
            continue
        files[path] = (root / path).read_text(encoding="utf-8", errors="replace")
    return files


def default_base(root: Path) -> str | None:
    if not git(root, "rev-parse", "--verify", "--quiet", "mergegate/main", check=False).strip():
        return None
    return git(root, "merge-base", "HEAD", "mergegate/main").strip() or None


def renames_since(root: Path, base: str) -> dict[str, str]:
    """New path -> old path for Rust files renamed between `base` and the work tree."""
    output = git(root, "diff", "-M", "--name-status", "-z", base, "--", "*.rs")
    fields = output.split("\0")
    renames, i = {}, 0
    while i < len(fields) and fields[i]:
        status = fields[i]
        if status.startswith(("R", "C")):
            renames[fields[i + 2]] = fields[i + 1]
            i += 3
        else:
            i += 2
    return renames


def changed_since(root: Path, base: str) -> set[str]:
    """Paths changed between `base` and the work tree, new files included."""
    changed = git(root, "diff", "--name-only", "-z", base)
    untracked = git(root, "ls-files", "-z", "--others", "--exclude-standard")
    return set(filter(None, (changed + untracked).split("\0")))


def touched(finding: Finding, changed: set[str]) -> bool:
    """Whether a change touched what a finding is about: the file, a file
    directly in the folder, or the list."""
    return finding.path in changed or any(str(PurePosixPath(path).parent) == finding.path for path in changed)


def base_exceptions(root: Path, base: str) -> dict[Key, int] | None:
    """The base's list, or None when the base predates this guard: the change
    that adds the guard is the one that writes its first list."""
    if not git(root, "cat-file", "-t", f"{base}:{GUARD}", check=False).strip():
        return None
    return parse_exceptions(git(root, "show", f"{base}:{EXCEPTIONS}", check=False))


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base", help="the commit to follow renames from and hold the list to")
    parser.add_argument("--shrink", action="store_true", help="lower the list's ceilings to the code")
    parser.add_argument("--all", action="store_true", help="list warnings in files the change did not touch too")
    options = parser.parse_args(argv)
    root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel").strip())
    base = options.base or default_base(root)
    renames = renames_since(root, base) if base else {}
    path = root / EXCEPTIONS
    entries = parse_exceptions(path.read_text(encoding="utf-8")) if path.exists() else {}
    shape = shape_of(rust_files(root))

    if options.shrink:
        lowered = shrunk(shape, entries, renames)
        path.write_text(format_exceptions(lowered), encoding="utf-8")
        print(f"wrote {EXCEPTIONS}: {len(lowered)} entries, {len(entries) - len(lowered)} resolved")
        return 0

    found = findings(shape, entries, renames)
    held_to = base_exceptions(root, base) if base else None
    if held_to is not None:
        found += ratchet(entries, held_to, renames)
    # Errors always show; warnings only for what the change touched, so a run
    # points at the change's own files rather than at the backlog.
    changed = changed_since(root, base) if base and not options.all else None
    shown = [f for f in found if f.error or changed is None or touched(f, changed)]
    for finding in shown:
        print(f"{'error' if finding.error else 'warn '}  {finding.path}: {finding.message}")
    errors = sum(finding.error for finding in found)
    warnings = len(found) - errors
    hidden = len(found) - len(shown)
    elsewhere = f" ({hidden} in files this change did not touch; --all lists them)" if hidden else ""
    print(f"module shape: {errors} errors, {warnings} warnings{elsewhere}")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
