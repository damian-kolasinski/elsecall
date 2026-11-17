"""Which checks a set of changed repository paths can affect.

`select(root, changed)` maps paths, relative to the repository root, to the test
suites that can observe them: nextest filtersets for Rust test binaries and
library unit tests, and the Python unit test directories. When no rule knows a
path, the selection is the whole suite. Over-selecting costs minutes;
under-selecting hides a failure until the full check, so every rule errs wide.

The rules:

- Workspace inputs (every package's manifest, as Cargo unifies features across
  them, the lock file, toolchain and test environment) and the root package's
  own sources select everything: every test binary and the desktop crate link
  the library, and most tests run its executable.
- A file in another workspace crate selects that crate's package, or everything
  when another package depends on it.
- `tests/NAME.rs` and `tests/NAME/main.rs` select that test binary. Anything
  else under `tests/DIR/` selects the binaries declaring `mod DIR;` and what
  names it, or everything when neither does.
- Files under `distribution/`, Python files under `scripts/`, and `.githooks/`
  select their Python unit tests.
- Any other file is followed through the Rust, Python and shell sources naming
  it: by its base name, or by one of its directories as `parent/dir`. A name
  inside a `#[cfg(test)]` library module selects the unit tests of that file's
  module, whose sibling and child test modules may use it, a
  name in a test source selects its binary or suite, and a name anywhere else
  selects what the naming file affects. Comments do not count. Documentation,
  CI files and scripts nothing names select nothing.
- A path matching no rule selects everything.
"""
from __future__ import annotations

import re
import subprocess

try:
    import tomllib
except ModuleNotFoundError:  # Python before 3.11
    tomllib = None
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath

# Inputs every build and test run reads. Every package's `Cargo.toml` is one
# too: Cargo unifies dependency features across the workspace's packages.
WORKSPACE_INPUTS = {
    "Cargo.lock",
    "rust-toolchain.toml",
    "mise.toml",
    "build.rs",
    "scripts/test-environment.py",
}
WORKSPACE_DIRECTORIES = (".cargo/", ".config/")

# Read or run by people and CI, never by a check, unless a source names them.
# Clippy still compiles the examples.
INERT_DIRECTORIES = ("docs/", ".github/", "examples/", "scripts/", ".githooks/")
INERT_SUFFIXES = (".md", ".txt")
INERT_FILES = {"LICENSE", ".gitignore"}

# Sources whose names describe files of another repository: the selection's
# own test fixture. They read none of this repository's files.
NOT_READERS = {"scripts/test_check_selection.py"}

# Files that can read, include or run another file. Documents, data and
# configuration naming a file do not make it part of a check.
SOURCE_SUFFIXES = (".rs", ".py", ".sh")


def python_suite(path: str) -> str | None:
    """The Python unit test directory exercising `path`, if any."""
    if path.startswith("distribution/"):
        return "distribution"
    if path.startswith(".githooks/") or (path.startswith("scripts/") and path.endswith(".py")):
        return "scripts"
    return None


@dataclass(frozen=True)
class Everything:
    reason: str


@dataclass
class Targets:
    """Suites to run: nextest filterset atoms and Python unit test directories."""

    rust: set[str] = field(default_factory=set)
    python: set[str] = field(default_factory=set)

    def add(self, other: "Targets") -> None:
        self.rust |= other.rust
        self.python |= other.python

    def empty(self) -> bool:
        return not self.rust and not self.python


@dataclass
class Selection:
    everything: Everything | None
    targets: Targets
    # Each changed path and what it selected, for people to check the choice.
    reasons: list[tuple[str, str]]

    def nextest_filter(self) -> str | None:
        """The filterset for `cargo nextest run -E`, or None for no Rust tests.

        Only meaningful without `everything`."""
        if not self.targets.rust:
            return None
        return " | ".join(sorted(self.targets.rust))


@dataclass(frozen=True)
class Package:
    name: str
    directory: str  # "" for the root package
    dependents: frozenset[str]  # names of workspace packages with a path dependency on it


class Workspace:
    """The repository's packages and text sources, read once."""

    def __init__(self, root: Path):
        self.root = root
        self.files = tracked_files(root)
        self.packages = read_packages(root)
        self._sources: dict[str, list[str]] | None = None

    def package_for(self, path: str) -> Package:
        best = None
        for package in self.packages:
            prefix = package.directory + "/" if package.directory else ""
            if path.startswith(prefix) and (best is None or len(package.directory) > len(best.directory)):
                best = package
        assert best is not None, "the root package owns every path"
        return best

    def sources(self) -> dict[str, list[str]]:
        if self._sources is None:
            self._sources = {}
            for name in self.files:
                if not name.endswith(SOURCE_SUFFIXES):
                    continue
                try:
                    text = (self.root / name).read_text()
                except (UnicodeDecodeError, FileNotFoundError, IsADirectoryError):
                    continue
                self._sources[name] = text.splitlines()
        return self._sources


def tracked_files(root: Path) -> list[str]:
    output = subprocess.run(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=root,
        check=True,
        capture_output=True,
    ).stdout
    return [name for name in output.decode().split("\0") if name]


def read_packages(root: Path) -> list[Package]:
    workspace = tomllib.loads((root / "Cargo.toml").read_text())
    directories = workspace.get("workspace", {}).get("members", ["."])
    manifests = {}
    for directory in directories:
        directory = "" if directory == "." else directory.rstrip("/")
        manifests[directory] = tomllib.loads((root / directory / "Cargo.toml").read_text())
    names = {directory: manifest["package"]["name"] for directory, manifest in manifests.items()}
    dependents: dict[str, set[str]] = {directory: set() for directory in manifests}
    for directory, manifest in manifests.items():
        tables = [manifest.get(key, {}) for key in ("dependencies", "dev-dependencies", "build-dependencies")]
        for table in tables:
            for spec in table.values():
                if isinstance(spec, dict) and "path" in spec:
                    target = (PurePosixPath(directory) / spec["path"]).as_posix()
                    target = normalize(target)
                    if target in dependents:
                        dependents[target].add(names[directory])
    return [
        Package(names[directory], directory, frozenset(dependents[directory]))
        for directory in manifests
    ]


def normalize(path: str) -> str:
    parts: list[str] = []
    for part in PurePosixPath(path).parts:
        if part == "..":
            parts.pop()
        elif part != ".":
            parts.append(part)
    return "/".join(parts)


def select(root: Path, changed: list[str]) -> Selection:
    if tomllib is None:
        reason = "reading Cargo manifests needs Python 3.11 or newer"
        return Selection(Everything(reason), Targets(), [(path, f"everything: {reason}") for path in changed])
    workspace = Workspace(root)
    targets = Targets()
    reasons = []
    everything = None
    for path in sorted(set(changed)):
        effect = affects(workspace, path, set())
        if isinstance(effect, Everything):
            everything = everything or effect
            reasons.append((path, f"everything: {effect.reason}"))
        elif effect.empty():
            reasons.append((path, "nothing: no check reads it"))
        else:
            targets.add(effect)
            reasons.append((path, ", ".join(sorted(effect.rust | {f"python {s}" for s in effect.python}))))
    return Selection(everything, targets, reasons)


def affects(workspace: Workspace, path: str, visiting: set[str]) -> Everything | Targets:
    """What a change to `path` can affect. `visiting` breaks reference cycles."""
    if path in visiting:
        return Targets()
    visiting = visiting | {path}
    if path in WORKSPACE_INPUTS or path.startswith(WORKSPACE_DIRECTORIES) or path.endswith("Cargo.toml"):
        return Everything(f"{path} is an input of every build and test run")
    package = workspace.package_for(path)
    relative = path[len(package.directory) + 1 :] if package.directory else path
    in_package_sources = relative.startswith("src/") or relative == "build.rs"
    if in_package_sources and not package.directory:
        return Everything(f"{path} is in the {package.name} library or executable, which every test uses")
    if package.directory:
        if package.dependents:
            names = ", ".join(sorted(package.dependents))
            return Everything(f"{path} is in {package.name}, which {names} depend on")
        named = referenced(workspace, path, visiting, outside=package.directory + "/")
        if isinstance(named, Everything):
            return named
        named.rust.add(f"package({package.name})")
        return named
    if relative.startswith("tests/"):
        parts = relative.split("/")[1:]
        # Cargo's test binaries: tests/NAME.rs and tests/NAME/main.rs.
        if len(parts) == 1 and parts[0].endswith(".rs") or parts[1:] == ["main.rs"]:
            if path not in workspace.files:
                # A deleted test binary has nothing left to run.
                return Targets()
            return Targets(rust={binary(package, parts[0].removesuffix(".rs"))})
        # Anything else in tests/ is a shared module or data: the binaries that
        # declare its directory as a module, and whatever names it.
        named = referenced(workspace, path, visiting)
        if isinstance(named, Everything):
            return named
        named.rust |= {binary(package, stem) for stem in declaring(workspace, package, parts[0])}
        if named.empty():
            return Everything(f"no test declares or names {path}")
        return named
    suite = python_suite(path)
    targets = referenced(workspace, path, visiting)
    if suite and not isinstance(targets, Everything):
        targets.python.add(suite)
    if isinstance(targets, Everything) or not targets.empty():
        return targets
    if path.startswith(INERT_DIRECTORIES) or path.endswith(INERT_SUFFIXES) or path in INERT_FILES:
        return Targets()
    return Everything(f"no rule says what {path} affects")


def binary(package: Package, stem: str) -> str:
    return f"binary_id({package.name}::{stem})"


def declaring(workspace: Workspace, package: Package, module: str) -> set[str]:
    """Test binaries of `package` declaring the shared module `module`."""
    prefix = f"{package.directory}/tests/" if package.directory else "tests/"
    pattern = re.compile(rf"^\s*(#\[[^\]]*\]\s*)*(pub(\([^)]*\))?\s+)?mod\s+{re.escape(module)}\s*;")
    found = set()
    for name, lines in workspace.sources().items():
        rest = name[len(prefix) :] if name.startswith(prefix) else None
        if rest and "/" not in rest and rest.endswith(".rs") and any(pattern.match(line) for line in lines):
            found.add(rest[: -len(".rs")])
    return found


def naming_pattern(path: str) -> re.Pattern[str]:
    """How a source names `path`: its base name as a whole word or the end of a
    string, or one of its directories below the top level as `parent/dir`, with or
    without a trailing slash, as code reading the whole directory would."""
    parts = PurePosixPath(path).parts
    # Every module directory has a `mod.rs`; its directory tells them apart.
    names = [re.escape("/".join(parts[-2:]) if parts[-1] == "mod.rs" else parts[-1])]
    for end in range(len(parts) - 1, 1, -1):
        names.append(re.escape("/".join(parts[end - 2 : end])) + "/?")
    return re.compile(rf"""(^|["'/\s=])({"|".join(names)})(["'\s;)]|$)""")


def referenced(workspace: Workspace, path: str, visiting: set[str], outside: str = "") -> Everything | Targets:
    """What the sources naming `path` affect. With `outside`, names inside that
    directory are skipped: the caller already selected it."""
    targets = Targets()
    pattern = naming_pattern(path)
    for name, lines in workspace.sources().items():
        if name == path or name in NOT_READERS or (outside and name.startswith(outside)):
            continue
        comment = "//" if name.endswith(".rs") else "#"
        hits = [
            index
            for index, line in enumerate(lines)
            if pattern.search(line) and not line.lstrip().startswith(comment)
        ]
        if not hits:
            continue
        effect = naming_effect(workspace, name, lines, hits, visiting)
        if isinstance(effect, Everything):
            return Everything(f"{name} names {path}; {effect.reason}")
        targets.add(effect)
    return targets


def naming_effect(workspace: Workspace, name: str, lines: list[str], hits: list[int], visiting: set[str]) -> Everything | Targets:
    """What `name` affects through the lines (`hits`) naming a changed file."""
    package = workspace.package_for(name)
    relative = name[len(package.directory) + 1 :] if package.directory else name
    if name.endswith(".rs") and in_library(workspace, package, relative):
        # Only test code names the file, so the library itself is unchanged. A
        # test module's helpers serve its sibling and child modules too, so the
        # whole file's module is selected.
        if all(in_test_module(lines, hit) for hit in hits):
            path = module_path(workspace, package, relative)
            if path:
                return Targets(rust={f"(package({package.name}) & kind(lib) & test(/^{'::'.join(path)}::/))"})
            return Targets(rust={f"(package({package.name}) & kind(lib))"})
    return affects(workspace, name, visiting)


def in_library(workspace: Workspace, package: Package, relative: str) -> bool:
    """Whether `relative` is only ever compiled into `package`'s library: it is
    under `src/`, and the executable's root declares no modules of its own."""
    prefix = f"{package.directory}/" if package.directory else ""
    if f"{prefix}src/lib.rs" not in workspace.files or not relative.startswith("src/"):
        return False
    if relative == "src/main.rs" or relative.startswith("src/bin/"):
        return False
    main = workspace.sources().get(f"{prefix}src/main.rs", [])
    return not any(re.match(r"\s*(pub(\(\w+\))?\s+)?mod\s", line) for line in main)


def in_test_module(lines: list[str], hit: int) -> bool:
    """Whether line `hit` is inside a `#[cfg(test)]` module, by rustfmt's layout:
    the nearest line above it at column 0 is `mod NAME {`, after the attribute."""
    for start in range(hit, 0, -1):
        line = lines[start]
        if not line or line[0].isspace() or line.startswith("//"):
            continue
        return bool(re.match(r"mod \w+ \{$", line)) and lines[start - 1] == "#[cfg(test)]"
    return False


def module_path(workspace: Workspace, package: Package, relative: str) -> list[str] | None:
    """The module path of a library source file, or None when a `#[path]`
    attribute could place it, or the directory it is in, elsewhere."""
    prefix = f"{package.directory}/" if package.directory else ""
    for name, lines in workspace.sources().items():
        if not name.startswith(f"{prefix}src/"):
            continue
        declaring_file = PurePosixPath(name[len(prefix) :])
        for line in lines:
            match = re.search(r'#\[path\s*=\s*"([^"]+)"', line)
            if not match:
                continue
            # Relative to the declaring file's directory, or for a non-`mod.rs`
            # file, its own subdirectory.
            for base in (declaring_file.parent, declaring_file.with_suffix("")):
                target = PurePosixPath(normalize((base / match.group(1)).as_posix()))
                if relative == target.as_posix() or PurePosixPath(relative).is_relative_to(target.parent):
                    return None
    file = PurePosixPath(relative)
    parts = list(file.with_suffix("").parts[1:])
    if parts[-1] == "mod":
        parts.pop()
    if parts in (["lib"], ["main"]):
        return []
    return parts
