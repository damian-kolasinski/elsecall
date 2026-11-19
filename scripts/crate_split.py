#!/usr/bin/env python3
"""Move one planned crate out of the `elsecall` library into
crates/elsecall-<name>/, as scripts/crate-graph.toml plans it.

The move is mechanical and repeatable, so work that started before it can be
rebased by running it again:

- The crate's modules move with `git mv`. A crate of one top-level module is
  flattened: the module's file becomes lib.rs and its children the crate's
  modules, so paths read `elsecall_failure::Failure`, not
  `elsecall_failure::failure::Failure`. A module planned below another module
  (`scheduling::timestamp`) becomes a top-level module of its crate.
- Every reference in the workspace is rewritten: `crate::`, `$crate::` and
  `elsecall::` paths, `super::`/`self::` paths of files whose parent changes,
  use trees (split by destination, keeping each imported name with `as` where
  flattening drops it), macro paths and serde attribute strings.
- A module planned for a crate still to come, but sitting inside one that
  moves now, is moved up to a top-level module of the library first.
- The new crate's manifest lists the workspace crates and dependencies its code
  uses, test-only ones as dev-dependencies; the root and the desktop app gain
  the crates they now name. `include_str!` paths are recomputed; `pub(crate)`
  items become `pub`, since other crates may use them now (`unreachable_pub`
  narrows them again).

Usage: crate_split.py NAME [--dry-run]
"""
from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

import crate_graph as cg
import module_shape as shape

ROOT = Path(__file__).resolve().parent.parent
LIBRARY = "elsecall"
PLAN = ROOT / "scripts" / "crate-graph.toml"
UI = "crates/elsecall-ui"

Path_ = tuple[str, ...]


# --- Masking Rust source ----------------------------------------------------------


def mask(text: str) -> str:
    """`text` with comments and the contents of string and char literals replaced
    by spaces; every character keeps its offset."""
    out = list(text)
    i, n = 0, len(text)
    depth = 0
    closing: str | None = None
    escapes = True

    def blank(a: int, b: int) -> None:
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        c = text[i]
        if depth:
            if text.startswith("*/", i):
                depth -= 1
                blank(i, i + 2)
                i += 2
            elif text.startswith("/*", i):
                depth += 1
                blank(i, i + 2)
                i += 2
            else:
                blank(i, i + 1)
                i += 1
            continue
        if closing is not None:
            if escapes and c == "\\":
                blank(i, min(i + 2, n))
                i += 2
            elif text.startswith(closing, i):
                i += len(closing)
                closing = None
            else:
                blank(i, i + 1)
                i += 1
            continue
        if text.startswith("//", i):
            end = text.find("\n", i)
            end = n if end == -1 else end
            blank(i, end)
            i = end
            continue
        if text.startswith("/*", i):
            depth = 1
            blank(i, i + 2)
            i += 2
            continue
        prefix = 2 if text.startswith("br", i) else 1 if c == "r" else 0
        if prefix and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            j = i + prefix
            while j < n and text[j] == "#":
                j += 1
            if j < n and text[j] == '"':
                closing, escapes = '"' + "#" * (j - i - prefix), False
                i = j + 1
                continue
        if c == '"':
            closing, escapes = '"', True
            i += 1
            continue
        if c == "'":
            if i + 3 < n and text[i + 1] == "\\":
                end = text.find("'", i + 3)
                if end != -1:
                    blank(i + 1, end)
                    i = end + 1
                    continue
            if i + 2 < n and text[i + 2] == "'":
                blank(i + 1, i + 2)
                i += 3
                continue
        i += 1
    return "".join(out)


# --- The move ---------------------------------------------------------------------


@dataclass
class Move:
    """The crate being moved out and how its modules are laid out in it."""

    name: str
    modules: list[Path_]
    flatten: bool

    @property
    def package(self) -> str:
        return f"{LIBRARY}-{self.name}"

    @property
    def ident(self) -> str:
        return self.package.replace("-", "_")

    @property
    def directory(self) -> str:
        return f"crates/{self.package}"

    def owner(self, path: Path_) -> Path_ | None:
        """The module of this crate `path` lies in, if any."""
        for module in sorted(self.modules, key=len, reverse=True):
            if path[: len(module)] == module:
                return module
        return None

    def relative(self, path: Path_) -> Path_:
        """`path` (from the library root) as a path inside this crate."""
        module = self.owner(path)
        assert module is not None
        base: Path_ = () if self.flatten else (module[-1],)
        return base + path[len(module) :]


def module_files(root: Path, module: Path_) -> list[str]:
    """The library files of `module` and its descendants, relative to `root`."""
    stem = Path("src", *module)
    files = []
    for candidate in (stem.with_suffix(".rs"), stem / "mod.rs"):
        if (root / candidate).is_file():
            files.append(candidate.as_posix())
    directory = root / stem
    if directory.is_dir():
        for path in sorted(directory.rglob("*.rs")):
            relative = path.relative_to(root).as_posix()
            if relative not in files:
                files.append(relative)
    return files


def file_module(relative_to_src: str) -> Path_:
    here = cg.module_path(relative_to_src)
    return () if here == ("lib",) else here


def destination(move: Move, old: str) -> str:
    """Where a library file of the crate lands."""
    inside = PurePosixPath(old).relative_to("src")
    for module in sorted(move.modules, key=len, reverse=True):
        parts = module
        as_file = PurePosixPath(*parts).with_suffix(".rs")
        as_dir = PurePosixPath(*parts)
        if inside == as_file or inside == as_dir / "mod.rs":
            if move.flatten:
                return f"{move.directory}/src/lib.rs"
            return f"{move.directory}/src/{parts[-1]}.rs"
        if as_dir in inside.parents:
            rest = inside.relative_to(as_dir)
            if move.flatten:
                return f"{move.directory}/src/{rest.as_posix()}"
            return f"{move.directory}/src/{parts[-1]}/{rest.as_posix()}"
    raise SystemExit(f"{old} is not in {move.name}")


# --- Paths -------------------------------------------------------------------------


USE = re.compile(r"\b(?P<vis>pub(?:\s*\([^)]*\))?\s+)?use\s+(?P<tree>[^;]+);")
PATH = re.compile(r"(?<![\w$])(?<!::)(?P<root>\$crate|crate|elsecall|super|self)(?P<rest>(?:\s*::\s*[A-Za-z_]\w*)+)")
STRING_PATH = re.compile(r'"(crate::[A-Za-z_]\w*(?:::[A-Za-z_]\w*)*)"')
MACRO_AFTER = re.compile(r"\s*!")


@dataclass
class Context:
    """How one file names the library: its crate after the move, and where it
    sits for `super::` and `self::`."""

    file: str
    root: str  # "crate" inside the library or the moved crate; "elsecall" outside
    here: Path_  # its module path in the library before the move
    moving: bool  # the file moves into the new crate
    reparented: bool  # its parent module changes, so `super::` must be resolved


def absolute(segments: list[str], context: Context, nested: Path_) -> Path_ | None:
    first, rest = segments[0], tuple(segments[1:])
    if first in ("crate", "$crate"):
        return rest if context.root == "crate" else None
    if first == LIBRARY:
        return rest if context.root == LIBRARY else None
    # Files that stay may reach moved siblings through `super::`; files that
    # move keep their structure, except those whose parent changes.
    if first in ("super", "self") and (context.reparented or not context.moving):
        here = context.here + nested
        if first == "self":
            return here + rest
        base = list(here)
        while segments and segments[0] == "super":
            if not base:
                return None
            base.pop()
            segments = segments[1:]
        return tuple(base) + tuple(segments)
    return None


@dataclass
class Rewriter:
    move: Move
    relocations: dict[Path_, str]  # module path -> new top-level module name
    problems: list[str]

    def target(self, path: Path_, context: Context, macro: bool) -> tuple[str, Path_] | None:
        """The new root and segments for an absolute library path, or None to
        leave it."""
        for old, new in self.relocations.items():
            if path[: len(old)] == old:
                return (("crate" if context.root == "crate" and not context.moving else context.root), (new,) + path[len(old) :])
        if self.move.owner(path) is None:
            if context.moving:
                self.problems.append(f"{context.file}: `{'::'.join(path)}` stays in the library, which {self.move.name} may not depend on")
            return None
        relative = (path[-1],) if macro else self.move.relative(path)
        root = "crate" if context.moving else self.move.ident
        if context.moving and context.root == "crate" and macro:
            root = "crate"
        return root, relative

    def render(self, root: str, segments: Path_, dollar: bool) -> str:
        if root == "crate" and dollar:
            root = "$crate"
        return "::".join((root,) + segments) if segments else root

    def rewrite(self, text: str, context: Context) -> tuple[str, set[str]]:
        """`text` with its library paths rewritten, and the crates it now names."""
        masked = mask(text)
        nested_of_line = cg.inline_modules(masked)
        line_starts = [0]
        for k, c in enumerate(masked):
            if c == "\n":
                line_starts.append(k + 1)

        def nested_at(offset: int) -> Path_:
            line = sum(1 for s in line_starts if s <= offset) - 1
            return nested_of_line[line] if line < len(nested_of_line) else ()

        named: set[str] = set()
        edits: list[tuple[int, int, str]] = []
        taken: list[tuple[int, int]] = []
        for match in USE.finditer(masked):
            tree = re.sub(r"\s+", " ", match.group("tree")).strip()
            groups = self.rewrite_use(tree, context, nested_at(match.start()), named)
            if groups is not None:
                vis = match.group("vis") or ""
                line_start = masked.rfind("\n", 0, match.start()) + 1
                indent = re.match(r"[ \t]*", masked[line_start:]).group(0)
                # An attribute governs one statement, so an attributed import
                # stays one statement.
                attributed = masked[: match.start()].rstrip().endswith("]")
                if len(groups) == 1 or attributed:
                    body = groups[0] if len(groups) == 1 else "{" + ", ".join(groups) + "}"
                    replacement = f"{vis}use {body};"
                else:
                    replacement = f"\n{indent}".join(f"{vis}use {group};" for group in groups)
                edits.append((match.start(), match.end(), replacement))
            taken.append((match.start(), match.end()))
        for match in PATH.finditer(masked):
            if any(a <= match.start() < b for a, b in taken):
                continue
            segments = [s.strip() for s in (match.group("root") + match.group("rest")).split("::")]
            path = absolute(segments, context, nested_at(match.start()))
            if path is None or not path:
                continue
            macro = bool(MACRO_AFTER.match(masked, match.end()))
            new = self.target(path, context, macro)
            if new is None:
                continue
            root, relative = new
            if root not in ("crate", "$crate", LIBRARY):
                named.add(root)
            edits.append((match.start(), match.end(), self.render(root, relative, segments[0] == "$crate")))
        for match in STRING_PATH.finditer(text):
            if masked[match.start()] != '"':
                continue
            segments = match.group(1).split("::")
            path = absolute(segments, context, ())
            if path is None:
                continue
            new = self.target(path, context, False)
            if new is None:
                continue
            root, relative = new
            if root not in ("crate", LIBRARY):
                named.add(root)
            edits.append((match.start(), match.end(), '"' + self.render(root, relative, False) + '"'))
        for start, end, replacement in sorted(edits, reverse=True):
            text = text[:start] + replacement + text[end:]
        return text, named

    def rewrite_use(self, tree: str, context: Context, nested: Path_, named: set[str]) -> list[str] | None:
        """The use tree rewritten as one tree per root, or None if unchanged."""
        squeezed = tree.replace(" ", "")
        if not re.match(r"(\$crate|crate|elsecall|super|self)(::|$)|\{", squeezed):
            return None
        leaves = []
        for leaf in split_tree(tree):
            alias = None
            m = re.match(r"(.+?)\s+as\s+([A-Za-z_]\w*)$", leaf)
            if m:
                leaf, alias = m.group(1).strip(), m.group(2)
            leaves.append((leaf.replace(" ", ""), alias))
        changed = False
        out = []
        for leaf, alias in leaves:
            segments = leaf.split("::")
            glob = segments[-1] == "*"
            path_segments = segments[:-1] if glob else segments
            path = absolute(path_segments, context, nested)
            new = self.target(path, context, False) if path else None
            if new is None:
                out.append(leaf + (f" as {alias}" if alias else ""))
                continue
            changed = True
            root, relative = new
            if root not in ("crate", LIBRARY):
                named.add(root)
            rendered = self.render(root, relative, False) + ("::*" if glob else "")
            keep = alias
            if keep is None and not glob:
                old_name = path[-1]
                new_name = relative[-1] if relative else root
                if new_name != old_name:
                    keep = old_name
            out.append(rendered + (f" as {keep}" if keep else ""))
        if not changed:
            return None
        roots: dict[str, list[str]] = {}
        for leaf in out:
            head = re.split(r"::| as ", leaf, maxsplit=1)[0]
            roots.setdefault(head, []).append(leaf)
        return [nest(leaves) for leaves in roots.values()]


def nest(leaves: list[str]) -> str:
    """Leaves sharing their first segment as one use tree:
    [`a::b::c`, `a::b::d as e`, `a::f`] -> `a::{b::{c, d as e}, f}`."""
    tree: dict = {}
    for leaf in leaves:
        path, _, alias = leaf.partition(" as ")
        node = tree
        segments = path.split("::")
        for segment in segments[:-1]:
            node = node.setdefault(segment, {})
        node.setdefault(segments[-1], {}).setdefault("", []).append(alias)

    def render(name: str, node: dict) -> list[str]:
        own = [f"{name} as {alias}" if alias else name for alias in node.get("", [])]
        children = [(k, v) for k, v in node.items() if k != ""]
        if not children:
            return own
        inner = [("self" if alias == "" else f"self as {alias}") for alias in node.get("", [])]
        inner += [item for key, child in children for item in render(key, child)]
        return [f"{name}::{inner[0]}" if len(inner) == 1 else f"{name}::{{{', '.join(inner)}}}"]

    (root,) = tree
    rendered = render(root, tree[root])
    return rendered[0] if len(rendered) == 1 else "{" + ", ".join(rendered) + "}"


def split_tree(tree: str) -> list[str]:
    """`a::{b, c::{d, self}}` -> [`a::b`, `a::c::d`, `a::c::self` as `a::c`]."""
    tree = tree.strip()
    brace = tree.find("{")
    if brace == -1:
        if tree.endswith("::self"):
            return [tree[: -len("::self")]]
        return [tree]
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
        item = item.strip()
        if not item:
            continue
        if item == "self":
            out.append(head.rstrip(":").strip())
            continue
        out += split_tree(head + item)
    return out


# --- Manifests ----------------------------------------------------------------------


def external_dependencies(root: Path) -> dict[str, str]:
    """Rust identifier -> dependency name, for [workspace.dependencies] outside the workspace."""
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    found = {}
    for name, spec in manifest["workspace"]["dependencies"].items():
        if isinstance(spec, dict) and "path" in spec:
            continue
        found[name.replace("-", "_")] = name
    return found


def uses(masked: str, ident: str) -> bool:
    return re.search(rf"(?<![\w:]){re.escape(ident)}\s*::|\buse\s+{re.escape(ident)}\b|\bextern\s+crate\s+{re.escape(ident)}\b", masked) is not None


def crate_manifest(move: Move, normal: set[str], dev: set[str], features: dict[str, list[str]]) -> str:
    def lines(names: set[str]) -> str:
        return "".join(f"{name}.workspace = true\n" for name in sorted(names))

    text = (
        "[package]\n"
        f'name = "{move.package}"\n'
        "version.workspace = true\n"
        "edition.workspace = true\n"
        "rust-version.workspace = true\n"
        "license.workspace = true\n"
        "publish = false\n\n"
        "[lib]\n"
        "doctest = false\n\n"
    )
    if features:
        text += "[features]\n" + "".join(f"{k} = {v!r}\n".replace("'", '"') for k, v in features.items()) + "\n"
    text += "[dependencies]\n" + lines(normal)
    dev = dev - normal
    if dev:
        text += "\n[dev-dependencies]\n" + lines(dev)
    return text


def add_dependency(manifest: Path, table: str, name: str) -> None:
    text = manifest.read_text()
    line = f"{name}.workspace = true\n"
    header = f"[{table}]\n"
    if line in text:
        return
    if header not in text:
        text = text.rstrip("\n") + f"\n\n{header}{line}"
    else:
        start = text.index(header) + len(header)
        end = text.find("\n[", start)
        end = len(text) if end == -1 else end + 1
        block = text[start:end].rstrip("\n").split("\n") if text[start:end].strip() else []
        block = sorted(set(b for b in block if b.strip()) | {line.rstrip("\n")}, key=lambda b: b.split(".")[0].split(" ")[0])
        text = text[:start] + "\n".join(block) + "\n" + ("\n" if end < len(text) else "") + text[end:].lstrip("\n")
    manifest.write_text(text)


def register(root: Path, move: Move) -> None:
    """The new crate in [workspace.dependencies]."""
    manifest = root / "Cargo.toml"
    text = manifest.read_text()
    entry = f'{move.package} = {{ path = "{move.directory}" }}\n'
    if entry in text:
        return
    anchor = 'elsecall = { path = "." }\n'
    assert anchor in text
    text = text.replace(anchor, anchor + entry, 1)
    manifest.write_text(text)


# --- Declarations ---------------------------------------------------------------------


def remove_declaration(path: Path, name: str) -> None:
    """Drops `mod name;` (any visibility, with its attributes and docs) from `path`."""
    lines = path.read_text().split("\n")
    pattern = re.compile(rf"^\s*(pub(\([^)]*\))?\s+)?mod\s+{re.escape(name)}\s*;\s*$")
    for index, line in enumerate(lines):
        if pattern.match(line):
            start = index
            while start > 0 and re.match(r"^\s*(#\[|///)", lines[start - 1]):
                start -= 1
            del lines[start : index + 1]
            path.write_text("\n".join(lines))
            return
    raise SystemExit(f"no `mod {name};` in {path}")


def parent_file(root: Path, module: Path_) -> Path:
    if len(module) == 1:
        return root / "src" / "lib.rs"
    stem = Path("src", *module[:-1])
    for candidate in (stem.with_suffix(".rs"), stem / "mod.rs"):
        if (root / candidate).is_file():
            return root / candidate
    raise SystemExit(f"no parent file for {'::'.join(module)}")


def git(*args: str) -> None:
    subprocess.run(["git", *args], cwd=ROOT, check=True, capture_output=True)


INCLUDE = re.compile(r'(include_(?:str|bytes)!\s*\(\s*")([^"]+)("\s*\))')


# --- Main -------------------------------------------------------------------------------


def run(name: str, dry_run: bool) -> int:
    root = ROOT
    plan_text = PLAN.read_text()
    plan = cg.read_plan(plan_text)
    if name not in plan.crates:
        raise SystemExit(f"no planned crate `{name}`")
    modules = sorted(path for path, crate in plan.modules.items() if crate == name)
    if not modules:
        raise SystemExit(f"`{name}` has no library modules left to move")
    move = Move(name, modules, flatten=len(modules) == 1 and len(modules[0]) == 1)
    if (root / move.directory).exists():
        raise SystemExit(f"{move.directory} exists already")

    # Modules of crates still to come that sit inside this crate's modules move
    # up to the library's top level first.
    relocations: dict[Path_, str] = {}
    for path, crate in plan.modules.items():
        already = (root / f"crates/{LIBRARY}-{crate}").exists()
        if crate != name and not already and move.owner(path) is not None and len(path) > 1:
            relocations[path] = "_".join(path)
    files = {}
    for module in modules:
        for file in module_files(root, module):
            inside = file_module(file[len("src/") :])
            foreign = next((r for r in relocations if inside[: len(r)] == r), None)
            if foreign is None:
                files[file] = destination(move, file)
    relocated_files = {}
    for path, new in relocations.items():
        for file in module_files(root, path):
            rest = PurePosixPath(file).relative_to(PurePosixPath("src", *path[:-1]))
            parts = rest.parts
            first = parts[0].replace(path[-1], new, 1)
            relocated_files[file] = "src/" + "/".join((first,) + parts[1:])

    rewriter = Rewriter(move, relocations, [])
    externals = external_dependencies(root)
    texts: dict[str, str] = {}
    named_by: dict[str, set[str]] = {}
    normal, dev = set(), set()
    workspace_files = [p for p in shape.rust_files(root) if not p.startswith("crates/") or p.startswith(UI + "/")]
    for file in workspace_files:
        text = (root / file).read_text()
        if file.startswith(UI + "/") or file.startswith("tests/") or file == "src/main.rs":
            context = Context(file, LIBRARY, (), False, False)
        elif file.startswith("src/"):
            here = file_module(file[len("src/") :])
            moving = file in files
            relocating = file in relocated_files
            # A module file whose parent changes: the moved modules themselves,
            # and modules moved up to the library's top level.
            reparented = (moving and here in modules) or (relocating and here in relocations)
            context = Context(file, "crate", here, moving, reparented)
        else:
            continue
        new, named = rewriter.rewrite(text, context)
        if new != text:
            texts[file] = new
        if named:
            named_by[file] = named
    if rewriter.problems:
        for problem in rewriter.problems:
            print("error  " + problem)
        return 1

    # The new crate's own dependencies, by what its code names.
    for file in files:
        text = texts.get(file, (root / file).read_text())
        masked = mask(text)
        production = {n for n, _ in shape.production_lines(text)}
        test_file = shape.is_test_file(file)
        for number, line in enumerate(masked.split("\n"), 1):
            into = dev if test_file or number not in production else normal
            for ident in re.findall(rf"(?<![\w:]){LIBRARY}_[a-z0-9_]+(?=\s*::|\s*;|\s+as\b)", line):
                if ident != move.ident:
                    into.add(ident.replace("_", "-"))
            for ident, dependency in externals.items():
                if uses(line, ident):
                    into.add(dependency)
    features = {}
    root_manifest = tomllib.loads((root / "Cargo.toml").read_text())
    if name == "release-update":
        features["local-release-channel"] = []

    print(f"{move.package}: {len(files)} files, flatten={move.flatten}, relocating {sorted(relocations.values())}")
    print(f"  dependencies: {sorted(normal)}")
    print(f"  dev-dependencies: {sorted(dev - normal)}")
    print(f"  rewritten: {len(texts)} files")
    if dry_run:
        return 0

    for file, text in texts.items():
        (root / file).write_text(text)
    # Relocate foreign modules to the library's top level.
    for path, new in relocations.items():
        remove_declaration(parent_file(root, path), path[-1])
        lib = root / "src" / "lib.rs"
        lib.write_text(lib.read_text().rstrip("\n") + f"\npub mod {new};\n")
    for old, new in relocated_files.items():
        (root / new).parent.mkdir(parents=True, exist_ok=True)
        git("mv", old, new)
    # Drop the moved modules' declarations, then move their files.
    for module in modules:
        remove_declaration(parent_file(root, module), module[-1])
    for old, new in sorted(files.items()):
        (root / new).parent.mkdir(parents=True, exist_ok=True)
        git("mv", old, new)
        source = root / new
        text = source.read_text()
        old_dir = (root / old).parent
        text = INCLUDE.sub(
            lambda m: m.group(1) + os.path.relpath((old_dir / m.group(2)).resolve(), source.parent) + m.group(3),
            text,
        )
        text = re.sub(r"\bpub\s*\(\s*crate\s*\)", "pub", text)
        if any(file_module(old[len("src/") :]) == m for m in modules if len(m) > 1):
            text = re.sub(r"\bpub\s*\(\s*super\s*\)", "pub", text)
        source.write_text(text)
    if not move.flatten:
        lib = root / move.directory / "src" / "lib.rs"
        declared = "".join(f"pub mod {m[-1]};\n" for m in modules)
        lib.write_text(f"//! The {name} crate.\n{declared}")
    (root / move.directory / "Cargo.toml").write_text(crate_manifest(move, normal, dev, features))
    register(root, move)
    # Whoever names the crate now depends on it.
    for file, named in named_by.items():
        for ident in named:
            dependency = ident.replace("_", "-")
            if file.startswith(UI + "/"):
                add_dependency(root / UI / "Cargo.toml", "dependencies", dependency)
            elif file.startswith("src/") and file not in files or file.startswith("tests/"):
                add_dependency(root / "Cargo.toml", "dependencies", dependency)
    # The plan follows relocated modules.
    for path, new in relocations.items():
        plan_text = plan_text.replace(f'"{"::".join(path)}"', f'"{new}"')
    PLAN.write_text(plan_text)
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("name", help="the planned crate to move out")
    parser.add_argument("--dry-run", action="store_true")
    options = parser.parse_args(argv)
    return run(options.name, options.dry_run)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
