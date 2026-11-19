#!/usr/bin/env python3
"""Hold the crate graph Elsecall is heading for: every module in a planned crate,
dependencies only downward, no cycles, a small foundation.

`scripts/crate-graph.toml` assigns every top-level module of the library, the
binary and every workspace crate to a planned crate, and every planned crate to
a band. The check reads production code (test-only items and test files are
left out), resolves each `crate::`, `super::`, `self::` and `elsecall::` path to
the planned crate it names, and reports:

- `unassigned`: a module, binary or package with no planned crate. Choose its
  slice when you create it.
- `upward`: a crate depending on a crate in a higher band.
- `cycle`: crates in one band depending on each other, directly or through
  others. (A cycle across bands always holds an upward edge.)
- `testedge`: the same two rules for test code, which compiles with its crate:
  an upward test reference becomes a dev-dependency that builds a crate twice.
- `allowlist`: a dependency outside a crate's `may_depend_on`.
- `extern`: a framework its band forbids (`forbid`), such as axum in the
  foundation.
- `lines`: a crate over its band's `max_lines`.
- `name`: a crate named for no responsibility (`common`, `types`, ...).

Violations older than the check are listed in
`scripts/crate-graph-exceptions.txt` with how many references each has. The
counts are exact: a violation above its count fails, and so does one below it
or one resolved, until `--shrink` records the lower count, so freed room
cannot be spent again. With a base (`--base REF`, else the merge base with `mergegate/main`) the
list may not gain or raise an entry. `--shrink` lowers the counts to the code. `--cycles` prints each cycle within a
band with a cheapest set of edges to cut, and `--why FROM TO` every reference
behind an edge.

Usage: crate_graph.py [--base REF] [--shrink] [--cycles] [--why FROM TO] [--graph]
"""
from __future__ import annotations

import argparse
import re
import sys
import tomllib
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath

import module_shape as shape

CONFIG = "scripts/crate-graph.toml"
EXCEPTIONS = "scripts/crate-graph-exceptions.txt"
GUARD = "scripts/crate_graph.py"
LIBRARY = "elsecall"
# Crate names that say where code sits rather than what it owns.
CATCH_ALL = shape.CATCH_ALL | {"types", "base", "prelude", "lib", "stuff", "general"}
HEADER = (
    "# <kind> <subject> <count>: crate-graph violations older than scripts/crate_graph.py.\n"
    "# `edge FROM TO`: references from one planned crate to another that point upward,\n"
    "# close a cycle or leave an allowlist; `testedge FROM TO`: the same from test code.\n"
    "# `extern CRATE DEP`: uses of a framework the crate's band forbids. `lines CRATE`:\n"
    "# non-test lines over the band's budget. Counts are exact and only go down:\n"
    "# scripts/crate_graph.py --shrink lowers them after a fix.\n"
)


# --- The plan -----------------------------------------------------------------


@dataclass(frozen=True)
class Band:
    name: str
    rank: int
    max_lines: int | None
    forbid: frozenset[str]


@dataclass(frozen=True)
class Planned:
    name: str
    band: Band
    may_depend_on: frozenset[str] | None


@dataclass
class Plan:
    crates: dict[str, Planned]
    modules: dict[tuple[str, ...], str]  # library module path -> planned crate
    binaries: dict[str, str]  # source file -> planned crate
    packages: dict[str, str]  # package name -> planned crate
    problems: list[str] = field(default_factory=list)


def read_plan(text: str) -> Plan:
    data = tomllib.loads(text)
    bands = {}
    for rank, name in enumerate(data.get("bands", [])):
        spec = data.get("band", {}).get(name, {})
        bands[name] = Band(name, rank, spec.get("max_lines"), frozenset(spec.get("forbid", [])))
    plan = Plan({}, {}, {}, {})
    for name, spec in data.get("crates", {}).items():
        band = bands.get(spec.get("band"))
        if band is None:
            plan.problems.append(f"crate `{name}` has band {spec.get('band')!r}; expected one of {list(bands)}")
            continue
        allow = spec.get("may_depend_on")
        plan.crates[name] = Planned(name, band, frozenset(allow) if allow is not None else None)
        for module in spec.get("modules", []):
            key = tuple(module.split("::"))
            if key in plan.modules:
                plan.problems.append(f"module `{module}` is in both `{plan.modules[key]}` and `{name}`")
            plan.modules[key] = name
        if "binary" in spec:
            plan.binaries[spec["binary"]] = name
        for package in spec.get("packages", []):
            plan.packages[package] = name
    for planned in plan.crates.values():
        for other in planned.may_depend_on or ():
            if other not in plan.crates:
                plan.problems.append(f"crate `{planned.name}` may depend on `{other}`, which is not planned")
    return plan


def assign(plan: Plan, module: tuple[str, ...]) -> str | None:
    """The planned crate of a library module: the longest listed path that
    starts it."""
    for end in range(len(module), 0, -1):
        crate = plan.modules.get(module[:end])
        if crate is not None:
            return crate
    return None


# --- Reading Rust paths ---------------------------------------------------------


def blank(lines: list[tuple[int, str]]) -> str:
    """Production code with comments and string or char literals blanked out,
    keeping every line on its original line number."""
    out: list[str] = []
    lexer = shape.Lexer()
    expected = 1
    for number, line in lines:
        while expected < number:  # test-only lines become blank lines
            out.append("")
            expected += 1
        out.append(blank_line(line, lexer))
        expected += 1
    return "\n".join(out)


def blank_line(line: str, lexer: shape.Lexer) -> str:
    chars, i, n = [], 0, len(line)
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
            chars.append(" ")
            continue
        if lexer.closing is not None:
            if lexer.escapes and c == "\\":
                i += 2
            elif line.startswith(lexer.closing, i):
                i += len(lexer.closing)
                lexer.closing = None
            else:
                i += 1
            chars.append(" ")
            continue
        if line.startswith("//", i):
            break
        if line.startswith("/*", i):
            lexer.comment_depth = 1
            i += 2
            continue
        prefix = 2 if line.startswith("br", i) else 1 if c == "r" else 0
        if prefix and shape.starts_word(line, i):
            j = i + prefix
            while j < n and line[j] == "#":
                j += 1
            if j < n and line[j] == '"':
                lexer.closing, lexer.escapes = '"' + "#" * (j - i - prefix), False
                i = j + 1
                chars.append('""')
                continue
        if c == '"':
            lexer.closing, lexer.escapes = '"', True
            i += 1
            chars.append('""')
            continue
        if c == "'":
            if i + 3 < n and line[i + 1] == "\\":
                end = line.find("'", i + 3)
                i = end + 1 if end != -1 else n
                chars.append("' '")
                continue
            if i + 2 < n and line[i + 2] == "'":
                i += 3
                chars.append("' '")
                continue
        chars.append(c)
        i += 1
    return "".join(chars)


USE = re.compile(r"\buse\s+([^;{]*(?:\{[^;]*\})?[^;]*);", re.S)
PATH = re.compile(r"(?<![\w:$])((?:[A-Za-z_]\w*)(?:\s*::\s*[A-Za-z_]\w*)+)")


def expand_use(tree: str) -> list[str]:
    """`a::{b, c::{d, self}}` -> [`a::b`, `a::c::d`, `a::c`]. Aliases (`as x`)
    must already be removed."""
    return [path for path in _expand("", re.sub(r"\s+", "", tree)) if path]


def _expand(prefix: str, tree: str) -> list[str]:
    brace = tree.find("{")
    if brace == -1:
        if tree.endswith("::*"):
            tree = tree[: -len("::*")]
        if tree in ("self", "*", ""):
            return [prefix.rstrip(":")]
        return [prefix + tree]
    head, body = tree[:brace], tree[brace + 1 : tree.rindex("}")]
    items, depth, start = [], 0, 0
    for index, c in enumerate(body):
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
        elif c == "," and depth == 0:
            items.append(body[start:index])
            start = index + 1
    items.append(body[start:])
    out = []
    for item in items:
        if item:
            out += _expand(prefix + head, item)
    return out


@dataclass(frozen=True)
class Site:
    file: str
    line: int
    path: str


def references(text: str) -> list[tuple[int, str]]:
    """Every path in code as (line, `a::b::c`), use trees expanded."""
    found: list[tuple[int, str]] = []
    rest = list(text)

    def line_of(offset: int) -> int:
        return text.count("\n", 0, offset) + 1

    for match in USE.finditer(text):
        body = re.sub(r"\s+as\s+[A-Za-z_]\w*", "", match.group(1))
        for path in expand_use(body):
            found.append((line_of(match.start()), path))
        for index in range(match.start(), match.end()):
            if rest[index] != "\n":
                rest[index] = " "
    remaining = "".join(rest)
    for match in PATH.finditer(remaining):
        found.append((line_of(match.start()), re.sub(r"\s+", "", match.group(1))))
    return found


def module_path(relative: str) -> tuple[str, ...]:
    """`scheduling/service/running.rs` -> (scheduling, service, running)."""
    parts = list(PurePosixPath(relative).with_suffix("").parts)
    if parts and parts[-1] == "mod":
        parts.pop()
    return tuple(parts)


def resolve(path: str, here: tuple[str, ...], root: str) -> tuple[str, ...] | None:
    """A path's module path from the library root, or None when it names
    something outside the library. `root` is how the file names the library:
    `crate` inside it, `elsecall` outside."""
    segments = path.split("::")
    if segments[0] == root:
        return tuple(segments[1:])
    if root != "crate":
        return None
    if segments[0] == "self":
        return here + tuple(segments[1:])
    if segments[0] == "super":
        base = list(here)
        while segments and segments[0] == "super":
            if not base:
                return None
            base.pop()
            segments = segments[1:]
        return tuple(base) + tuple(segments)
    return None


# --- The graph ----------------------------------------------------------------


@dataclass
class Graph:
    edges: dict[tuple[str, str], list[Site]]  # production code
    test_edges: dict[tuple[str, str], list[Site]]  # test code only
    externs: dict[tuple[str, str], list[Site]]
    lines: dict[str, int]
    unassigned: dict[str, str]  # subject -> what it is


# `#[cfg(test)]` (with any further attributes) on an out-of-line module: every
# file of that module is test code.
TEST_MODULE = re.compile(
    r"#\[cfg\((?:test|all\(\s*test\b[^\]]*\))\)\]\s*(?:#\[[^\]]*\]\s*)*"
    r"(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*;"
)


def test_modules(files: dict[str, str]) -> set[tuple[str, ...]]:
    """Library module paths declared test-only by their parent."""
    found = set()
    for path, text in files.items():
        if not path.startswith("src/") or path == "src/main.rs" or shape.is_test_file(path):
            continue
        here = module_path(path[len("src/") :])
        here = () if here == ("lib",) else here
        for match in TEST_MODULE.finditer(text):
            found.add(here + (match.group(1),))
    return found


def build(root: Path, plan: Plan, files: dict[str, str], packages: dict[str, str]) -> Graph:
    """`files`: path -> text for Rust files; `packages`: package directory
    (relative, '' for the root) -> package name."""
    graph = Graph(defaultdict(list), defaultdict(list), defaultdict(list), defaultdict(int), {})
    test_only = test_modules({p: t for p, t in files.items() if packages.get("") == LIBRARY})
    forbidden = {name for planned in plan.crates.values() for name in planned.band.forbid}
    extern_pattern = re.compile(r"(?<![\w:])(" + "|".join(sorted(map(re.escape, forbidden))) + r")\s*::") if forbidden else None
    directories = sorted(packages, key=len, reverse=True)
    for path, text in sorted(files.items()):
        directory = next((d for d in directories if d == "" or path.startswith(d + "/")), None)
        if directory is None:
            continue
        package = packages[directory]
        inside = path[len(directory) + 1 :] if directory else path
        if not inside.startswith("src/"):
            continue  # examples, benches and build scripts are not part of the graph
        if package == LIBRARY and inside != "src/main.rs" and not inside.startswith("src/bin/"):
            here = module_path(inside[len("src/") :])
            if here == ("lib",):
                continue
            crate = assign(plan, here)
            if crate is None:
                graph.unassigned[f"src/{here[0]}"] = f"library module `{here[0]}`"
                continue
            library_root = "crate"
        else:
            crate = plan.binaries.get(path) if package == LIBRARY else plan.packages.get(package)
            if crate is None:
                subject = path if package == LIBRARY else directory
                graph.unassigned[subject] = "binary" if package == LIBRARY else f"package `{package}`"
                continue
            here, library_root = (), LIBRARY
        whole_file_is_test = shape.is_test_file(path) or any(here[: len(m)] == m for m in test_only)
        numbered = list(enumerate(text.splitlines(), 1))
        production = [] if whole_file_is_test else shape.production_lines(text)
        kept = {number for number, _ in production}
        test_code = [(number, line) for number, line in numbered if number not in kept]
        graph.lines[crate] += len(production)
        code = blank(production)
        for lines, into in ((production, graph.edges), (test_code, graph.test_edges)):
            for line, reference in references(blank(lines)):
                target_path = resolve(reference, here, library_root)
                if target_path is None or not target_path:
                    continue
                target = assign(plan, target_path)
                if target is None or target == crate:
                    continue
                into[(crate, target)].append(Site(path, line, reference))
        if extern_pattern is not None:
            banned = plan.crates[crate].band.forbid
            for number, line_text in enumerate(code.splitlines(), 1):
                for match in extern_pattern.finditer(line_text):
                    if match.group(1) in banned:
                        graph.externs[(crate, match.group(1))].append(Site(path, number, match.group(0)))
    return graph


def cycles(nodes: set[str], edges: set[tuple[str, str]]) -> list[set[str]]:
    """Strongly connected components with more than one crate."""
    successors: dict[str, list[str]] = defaultdict(list)
    for a, b in edges:
        successors[a].append(b)
    index: dict[str, int] = {}
    low: dict[str, int] = {}
    stack: list[str] = []
    on_stack: set[str] = set()
    found: list[set[str]] = []
    counter = 0

    def visit(node: str) -> None:
        nonlocal counter
        index[node] = low[node] = counter
        counter += 1
        stack.append(node)
        on_stack.add(node)
        for successor in sorted(successors[node]):
            if successor not in index:
                visit(successor)
                low[node] = min(low[node], low[successor])
            elif successor in on_stack:
                low[node] = min(low[node], index[successor])
        if low[node] == index[node]:
            component = set()
            while True:
                member = stack.pop()
                on_stack.discard(member)
                component.add(member)
                if member == node:
                    break
            if len(component) > 1:
                found.append(component)

    sys.setrecursionlimit(max(1000, 4 * len(nodes) + 100))
    for node in sorted(nodes):
        if node not in index:
            visit(node)
    return found


def suggested_cuts(component: set[str], weights: dict[tuple[str, str], int]) -> list[tuple[str, str]]:
    """Edges whose removal leaves `component` without a cycle: drop the lightest
    edge still on a cycle until none is left, then put back every cut the others
    made unnecessary. Deterministic; small, though not always the smallest."""
    edges = {e: w for e, w in weights.items() if e[0] in component and e[1] in component}
    cuts: list[tuple[str, str]] = []
    while True:
        remaining = cycles(component, set(edges))
        if not remaining:
            break
        inside = {e: w for e, w in edges.items() if any(e[0] in c and e[1] in c for c in remaining)}
        lightest = min(inside, key=lambda e: (inside[e], e))
        cuts.append(lightest)
        del edges[lightest]
        if not cycles(component, set(edges)):
            break
    # Put back every cut the others make unnecessary, heaviest first.
    for edge in sorted(cuts, key=lambda e: (-weights[e], e)):
        if not cycles(component, set(edges) | {edge}):
            edges[edge] = weights[edge]
            cuts.remove(edge)
    return sorted(cuts, key=lambda e: (weights[e], e))


# --- Violations ---------------------------------------------------------------

Key = tuple[str, str, str]  # (kind, subject, detail)


@dataclass(frozen=True)
class Violation:
    key: Key
    count: int
    reason: str
    first: Site | None


def violations(plan: Plan, graph: Graph) -> list[Violation]:
    found: list[Violation] = []
    # Bands are ordered, so a cycle across bands holds an upward edge, which is
    # reported on its own. What is left are cycles inside one band, found among
    # that band's own edges so that one new upward edge does not drag a whole
    # band into the report.
    same_band = {(a, b) for a, b in graph.edges if plan.crates[a].band == plan.crates[b].band}
    components = cycles(set(plan.crates), same_band)
    member_of = {crate: i for i, component in enumerate(components) for crate in component}
    for (a, b), sites in sorted(graph.edges.items()):
        source, target = plan.crates[a], plan.crates[b]
        reasons = []
        if target.band.rank > source.band.rank:
            reasons.append(f"{source.band.name} crate depends on the higher {target.band.name} band")
        if (a, b) in same_band and a in member_of and member_of.get(b) == member_of[a]:
            reasons.append(f"closes a cycle of {len(components[member_of[a]])} crates within the {source.band.name} band")
        if source.may_depend_on is not None and b not in source.may_depend_on:
            reasons.append(f"`{b}` is not in `{a}`'s may_depend_on")
        if reasons:
            found.append(Violation(("edge", a, b), len(sites), "; ".join(reasons), sites[0]))
    # Test code compiles with its crate: a test reference to a higher crate, or one
    # that closes a cycle in one band, becomes a dev-dependency that builds a
    # crate twice. Fixtures belong to the crate whose types they build.
    combined = set(graph.edges) | set(graph.test_edges)
    combined_same_band = {(a, b) for a, b in combined if plan.crates[a].band == plan.crates[b].band}
    test_components = cycles(set(plan.crates), combined_same_band)
    test_member_of = {crate: i for i, component in enumerate(test_components) for crate in component}
    for (a, b), sites in sorted(graph.test_edges.items()):
        source, target = plan.crates[a], plan.crates[b]
        reasons = []
        if target.band.rank > source.band.rank:
            reasons.append(f"test code in a {source.band.name} crate uses the higher {target.band.name} band")
        if (a, b) in combined_same_band and a in test_member_of and test_member_of.get(b) == test_member_of[a]:
            reasons.append(f"test code closes a cycle of {len(test_components[test_member_of[a]])} crates within the {source.band.name} band")
        if reasons:
            found.append(Violation(("testedge", a, b), len(sites), "; ".join(reasons), sites[0]))
    for (crate, dependency), sites in sorted(graph.externs.items()):
        band = plan.crates[crate].band.name
        found.append(Violation(("extern", crate, dependency), len(sites), f"the {band} band forbids `{dependency}`", sites[0]))
    for crate, lines in sorted(graph.lines.items()):
        budget = plan.crates[crate].band.max_lines
        if budget is not None and lines > budget:
            found.append(Violation(("lines", crate, ""), lines, f"{lines} non-test lines, over the {plan.crates[crate].band.name} budget of {budget}", None))
    return found


def parse_exceptions(text: str) -> dict[Key, int]:
    entries = {}
    for number, line in enumerate(text.splitlines(), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        fields = line.split()
        if fields[0] in ("edge", "testedge", "extern") and len(fields) == 4 and fields[3].isdigit():
            entries[(fields[0], fields[1], fields[2])] = int(fields[3])
        elif fields[0] == "lines" and len(fields) == 3 and fields[2].isdigit():
            entries[("lines", fields[1], "")] = int(fields[2])
        else:
            raise SystemExit(f"{EXCEPTIONS}:{number}: expected `edge FROM TO n`, `testedge FROM TO n`, `extern CRATE DEP n` or `lines CRATE n`, got {line!r}")
    return entries


def format_exceptions(entries: dict[Key, int]) -> str:
    rows = [" ".join(part for part in (kind, subject, detail, str(count)) if part) for (kind, subject, detail), count in sorted(entries.items())]
    return HEADER + "".join(row + "\n" for row in rows)


@dataclass(frozen=True)
class Finding:
    error: bool
    subject: str
    message: str


ADVICE = {
    "edge": "move the shared item down to a crate both can use, or declare a port (a trait) in the lower crate and implement it above",
    "testedge": "move the fixture to the crate that owns the types it builds, or test through a lower crate's interface",
    "extern": "keep the framework in the slice or adapter that exposes it; pass plain data down",
    "lines": "split the crate by concept: everything above it rebuilds when it changes",
}


def describe(violation: Violation) -> str:
    kind, subject, detail = violation.key
    return f"{kind} {subject} {detail}".rstrip()


def findings(plan: Plan, graph: Graph, entries: dict[Key, int]) -> list[Finding]:
    found = [Finding(True, CONFIG, problem) for problem in plan.problems]
    for subject, what in sorted(graph.unassigned.items()):
        found.append(Finding(True, subject, f"{what} is in no planned crate: add it to {CONFIG} under the slice it belongs to"))
    for name in plan.crates:
        if any(part.replace("-", "_") in CATCH_ALL for part in name.split("-")):
            found.append(Finding(True, CONFIG, f"crate `{name}` is named for no responsibility: name it for what it owns"))
    current = {v.key: v for v in violations(plan, graph)}
    for key, violation in sorted(current.items()):
        allowed = entries.get(key)
        site = f" (first at {violation.first.file}:{violation.first.line}, `{violation.first.path}`)" if violation.first else ""
        if allowed is None:
            found.append(Finding(True, describe(violation), f"{violation.reason}, {violation.count} reference(s){site}: {ADVICE[key[0]]}"))
        elif violation.count > allowed:
            found.append(Finding(True, describe(violation), f"{violation.reason}: {violation.count} reference(s), over the listed {allowed}{site}"))
        elif violation.count < allowed:
            # An exact count leaves no slack for a later change to spend.
            found.append(Finding(True, EXCEPTIONS, f"`{describe(violation)}` is down to {violation.count} from {allowed}: run --shrink"))
    for key, allowed in sorted(entries.items()):
        if key not in current:
            found.append(Finding(True, EXCEPTIONS, f"`{' '.join(p for p in key if p)}` is resolved: run --shrink to drop it"))
    return sorted(found, key=lambda f: (not f.error, f.subject))


def ratchet(entries: dict[Key, int], base: dict[Key, int]) -> list[Finding]:
    found = []
    for key, value in sorted(entries.items()):
        label = " ".join(p for p in key if p)
        if key not in base:
            found.append(Finding(True, EXCEPTIONS, f"adds `{label}`: fix the violation instead"))
        elif value > base[key]:
            found.append(Finding(True, EXCEPTIONS, f"raises `{label}` from {base[key]} to {value}: counts only go down"))
    return found


def shrunk(plan: Plan, graph: Graph, entries: dict[Key, int]) -> dict[Key, int]:
    current = {v.key: v.count for v in violations(plan, graph)}
    return {key: min(count, entries[key]) for key, count in current.items() if key in entries}


# --- Reading the repository -----------------------------------------------------


def workspace_packages(root: Path) -> dict[str, str]:
    """Package directory (relative, '' for the root) -> package name."""
    path = root / "Cargo.toml"
    if not path.is_file():
        return {}
    manifest = tomllib.loads(path.read_text(encoding="utf-8"))
    packages = {}
    if "package" in manifest:
        packages[""] = manifest["package"]["name"]
    for member in manifest.get("workspace", {}).get("members", []):
        if member == ".":
            continue
        for directory in sorted(root.glob(member)):
            path = directory / "Cargo.toml"
            if path.is_file():
                packages[directory.relative_to(root).as_posix()] = tomllib.loads(path.read_text(encoding="utf-8"))["package"]["name"]
    return packages


def mermaid(plan: Plan, graph: Graph, flagged: set[tuple[str, str]]) -> str:
    out = ["flowchart BT"]
    for band in sorted({p.band for p in plan.crates.values()}, key=lambda b: b.rank):
        out.append(f"  subgraph {band.name}")
        for planned in sorted(p.name for p in plan.crates.values() if p.band == band):
            out.append(f"    {planned.replace('-', '_')}[{planned}]")
        out.append("  end")
    for (a, b), sites in sorted(graph.edges.items()):
        arrow = "-. {} .->" if (a, b) in flagged else "-- {} -->"
        out.append(f"  {a.replace('-', '_')} {arrow.format(len(sites))} {b.replace('-', '_')}")
    return "\n".join(out)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base", help="the commit to hold the exceptions list to")
    parser.add_argument("--shrink", action="store_true", help="lower the list's counts to the code")
    parser.add_argument("--write-baseline", action="store_true", help="list every current violation; only for the change that adds this check")
    parser.add_argument("--why", nargs=2, metavar=("FROM", "TO"), help="print every reference from one planned crate to another")
    parser.add_argument("--graph", action="store_true", help="print the planned crate graph as a Mermaid flowchart")
    parser.add_argument("--cycles", action="store_true", help="print each cycle within a band with edges to cut")
    options = parser.parse_args(argv)
    root = Path(shape.git(Path.cwd(), "rev-parse", "--show-toplevel").strip())
    plan = read_plan((root / CONFIG).read_text(encoding="utf-8"))
    graph = build(root, plan, shape.rust_files(root), workspace_packages(root))
    path = root / EXCEPTIONS
    entries = parse_exceptions(path.read_text(encoding="utf-8")) if path.exists() else {}

    if options.why:
        a, b = options.why
        sites = graph.edges.get((a, b), [])
        for site in sites:
            print(f"{site.file}:{site.line}: {site.path}")
        print(f"{a} -> {b}: {len(sites)} reference(s)")
        return 0
    if options.cycles:
        for label, edges in (("code", graph.edges), ("code and tests", {**graph.test_edges, **graph.edges})):
            weights = {e: len(graph.edges.get(e, [])) + len(graph.test_edges.get(e, [])) for e in edges}
            same_band = {e for e in weights if plan.crates[e[0]].band == plan.crates[e[1]].band}
            for component in sorted(cycles(set(plan.crates), same_band), key=lambda c: sorted(c)):
                band = plan.crates[next(iter(component))].band.name
                print(f"{label}: {band} cycle of {len(component)}: {', '.join(sorted(component))}")
                for a, b in suggested_cuts(component, {e: w for e, w in weights.items() if e in same_band}):
                    print(f"  cut {a} -> {b} ({weights[(a, b)]} reference(s))")
        return 0
    if options.graph:
        print(mermaid(plan, graph, {v.key[1:] for v in violations(plan, graph) if v.key[0] == "edge"}))
        return 0
    if options.write_baseline:
        current = {v.key: v.count for v in violations(plan, graph)}
        path.write_text(format_exceptions(current), encoding="utf-8")
        print(f"wrote {EXCEPTIONS}: {len(current)} entries")
        return 0
    if options.shrink:
        lowered = shrunk(plan, graph, entries)
        path.write_text(format_exceptions(lowered), encoding="utf-8")
        print(f"wrote {EXCEPTIONS}: {len(lowered)} entries, {len(entries) - len(lowered)} resolved")
        return 0

    found = findings(plan, graph, entries)
    base = options.base or shape.default_base(root)
    if base and shape.git(root, "cat-file", "-t", f"{base}:{GUARD}", check=False).strip():
        found += ratchet(entries, parse_exceptions(shape.git(root, "show", f"{base}:{EXCEPTIONS}", check=False)))
    for finding in found:
        print(f"{'error' if finding.error else 'warn '}  {finding.subject}: {finding.message}")
    errors = sum(f.error for f in found)
    components = cycles(set(plan.crates), set(graph.edges))
    largest = max((len(c) for c in components), default=0)
    listed_edges = [k for k in entries if k[0] == "edge"]
    listed_tests = [k for k in entries if k[0] == "testedge"]
    print(
        f"crate graph: {errors} errors, {len(found) - errors} warnings · {len(plan.crates)} planned crates · "
        f"largest cycle {largest} crates · {len(listed_edges)} listed edges with "
        f"{sum(entries[k] for k in listed_edges)} references and {len(listed_tests)} test edges to go"
    )
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
