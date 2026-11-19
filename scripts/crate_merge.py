#!/usr/bin/env python3
"""Merge workspace crates into one, as modules of it.

Each member keeps its modules: a crate made of one flattened module (its
lib.rs is that module, such as elsecall-failure) becomes the module of that
name; a crate of several modules gives each of them to the merged crate. So
the merged crate's top-level modules are the library modules the members were
first moved out of, and `elsecall_failure::Failure` reads
`elsecall_contract::failure::Failure`. Every reference in the workspace is
rewritten, manifests are merged, and scripts/crate-graph.toml gets one entry
for the merged crate in place of its members'.

Usage: crate_merge.py NAME BAND MEMBER... [--doc TEXT]
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys
import textwrap
import tomllib
from pathlib import Path

import crate_split as split
import module_shape as shape

ROOT = Path(__file__).resolve().parent.parent
PLAN = ROOT / "scripts" / "crate-graph.toml"
LIBRARY = "elsecall"


def git(*args: str) -> None:
    subprocess.run(["git", *args], cwd=ROOT, check=True, capture_output=True)


def planned_modules(plan_text: str, name: str) -> list[str]:
    match = re.search(rf"\[crates\.{re.escape(name)}\]\n(.*?)(?=\n\[|\Z)", plan_text, re.S)
    if not match:
        raise SystemExit(f"no planned crate `{name}`")
    modules = re.search(r"^modules = (\[.*?\])", match.group(1), re.M)
    return eval(modules.group(1)) if modules else []  # a TOML list of strings is a Python list


def layout(member: str, plan_text: str) -> str | None:
    """The module a member becomes, or None when its modules join the merged
    crate's root.

    A member moved out of one library module becomes that module. A member
    of several top-level library modules, whose root only declares them,
    gives them to the merged crate's root. Any other member, carved out of a
    module's insides or made new, becomes a module named after itself.
    """
    lib = (ROOT / "crates" / f"{LIBRARY}-{member}" / "src" / "lib.rs").read_text()
    modules = planned_modules(plan_text, member)
    if len(modules) == 1 and "::" not in modules[0]:
        return modules[0]
    declared = re.findall(r"^(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;", lib, re.M)
    body = [line for line in lib.splitlines() if line.strip() and not line.startswith("//")]
    if modules and all("::" not in m for m in modules) and sorted(declared) == sorted(modules) and len(body) == len(declared):
        return None
    return member.replace("-", "_")


def rewrite(text: str, members: dict[str, str | None], merged: str, inside: bool, own: str | None) -> str:
    """Paths naming a member rewritten to the merged crate. `own` is the module
    this file's crate becomes, when the file belongs to a member that becomes
    one. Exported macros live at a crate's root, so paths to them keep none."""
    masked = split.mask(text)
    edits = []
    root = "crate" if inside else f"{LIBRARY}_{merged.replace('-', '_')}"
    macro = re.compile(r"::\s*\w+\s*!")
    for member, module in members.items():
        ident = f"{LIBRARY}_{member.replace('-', '_')}"
        for match in re.finditer(rf"(?<![\w:$]){ident}\b", masked):
            if macro.match(masked, match.end()):
                edits.append((match.start(), match.end(), root))
            else:
                edits.append((match.start(), match.end(), f"{root}::{module}" if module else root))
        for match in re.finditer(rf'"{ident}::', text):
            if masked[match.start()] == '"':
                edits.append((match.start() + 1, match.start() + 1 + len(ident), f"{root}::{module}" if module else root))
    if own:
        for match in re.finditer(r"(?<![\w:])\$?crate\s*::", masked):
            if not re.match(r"\s*\w+\s*!", masked[match.end():]):
                edits.append((match.end(), match.end(), f"{own}::"))
        for match in re.finditer(r'"crate::', text):
            if masked[match.start()] == '"':
                edits.append((match.end(), match.end(), f"{own}::"))
    for start, end, replacement in sorted(edits, reverse=True):
        text = text[:start] + replacement + text[end:]
    return text


def merge_manifests(merged: str, members: list[str]) -> str:
    names = {f"{LIBRARY}-{m}" for m in members}
    normal, dev, features = {}, {}, {}
    for member in members:
        manifest = tomllib.loads((ROOT / "crates" / f"{LIBRARY}-{member}" / "Cargo.toml").read_text())
        for table, into in (("dependencies", normal), ("dev-dependencies", dev)):
            for name, spec in manifest.get(table, {}).items():
                if name in names:
                    continue
                current = into.setdefault(name, {"workspace": True})
                for feature in spec.get("features", []) if isinstance(spec, dict) else []:
                    current.setdefault("features", [])
                    if feature not in current["features"]:
                        current["features"].append(feature)
        for feature, enables in manifest.get("features", {}).items():
            listed = features.setdefault(feature, [])
            for enable in enables:
                if enable.split("/")[0] not in names and enable not in listed:
                    listed.append(enable)

    def render(name: str, spec: dict) -> str:
        if spec.get("features"):
            return f'{name} = {{ workspace = true, features = [{", ".join(chr(34) + f + chr(34) for f in spec["features"])}] }}\n'
        return f"{name}.workspace = true\n"

    text = (
        "[package]\n"
        f'name = "{LIBRARY}-{merged}"\n'
        "version.workspace = true\nedition.workspace = true\nrust-version.workspace = true\n"
        "license.workspace = true\npublish = false\n\n[lib]\ndoctest = false\n\n[lints]\nworkspace = true\n\n"
    )
    if features:
        text += "[features]\n# Fixtures other crates' tests build with, and switches the binary forwards.\n" + "".join(f'{k} = [{", ".join(chr(34) + e + chr(34) for e in v)}]\n' for k, v in sorted(features.items())) + "\n"
    text += "[dependencies]\n" + "".join(render(n, s) for n, s in sorted(normal.items()))
    dev = {n: s for n, s in dev.items() if n not in normal or s.get("features")}
    if dev:
        text += "\n[dev-dependencies]\n" + "".join(render(n, s) for n, s in sorted(dev.items()))
    return text


def rename_dependencies(manifest: Path, members: list[str], merged: str, own: bool) -> None:
    """Dependencies on members become one on the merged crate; its own drop."""
    text = manifest.read_text()
    names = [f"{LIBRARY}-{m}" for m in members]
    target = f"{LIBRARY}-{merged}"
    lines = text.split("\n")
    out, table, seen = [], None, {}
    for line in lines:
        header = re.match(r"^\[(.+)\]$", line)
        if header:
            table = header.group(1)
        key = re.match(r"^([\w-]+)(\.workspace = true| = )", line)
        if key and key.group(1) in names:
            if own:
                continue
            features = re.findall(r'"([^"]+)"', line) if "features" in line else []
            slot = seen.get((table, target))
            if slot is not None:
                merged_features = sorted(set(out[slot][1]) | set(features))
                out[slot] = (out[slot][0], merged_features)
                continue
            seen[(table, target)] = len(out)
            out.append((target, features))
            continue
        if table == "features" and "=" in line and not line.startswith("#"):
            # A feature enabling members' features enables the merged crate's,
            # once; in the merged crate itself they are its own.
            feature, enables = line.split("=", 1)
            listed = []
            for enable in re.findall(r'"([^"]+)"', enables):
                package, _, rest = enable.partition("/")
                if package in names:
                    if own:
                        continue
                    enable = f"{target}/{rest}"
                if enable not in listed:
                    listed.append(enable)
            line = f"{feature.rstrip()} = [" + ", ".join(f'"{e}"' for e in listed) + "]"
        out.append(line)
    rendered = []
    for item in out:
        if isinstance(item, tuple):
            name, features = item
            rendered.append(f'{name} = {{ workspace = true, features = [{", ".join(chr(34) + f + chr(34) for f in features)}] }}' if features else f"{name}.workspace = true")
        else:
            rendered.append(item)
    manifest.write_text("\n".join(rendered))


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("name")
    parser.add_argument("band")
    parser.add_argument("members", nargs="+")
    parser.add_argument("--doc", required=True)
    options = parser.parse_args(argv)
    merged, members = options.name, options.members
    plan_text = PLAN.read_text()
    flattened = {m: layout(m, plan_text) for m in members}
    directory = ROOT / "crates" / f"{LIBRARY}-{merged}"
    if directory.exists() and merged not in members:
        raise SystemExit(f"{directory} exists")

    # Rewrite every Rust file in place first.
    for file in shape.rust_files(ROOT):
        crate = file.split("/")[1] if file.startswith("crates/") else None
        member = crate[len(LIBRARY) + 1 :] if crate and crate.startswith(LIBRARY + "-") else None
        inside = member in members
        own = flattened.get(member) if inside else None
        text = (ROOT / file).read_text()
        new = rewrite(text, flattened, merged, inside, own)
        if new != text:
            (ROOT / file).write_text(new)
    manifest = merge_manifests(merged, members)

    # Move files.
    staging = ROOT / "crates" / f".merge-{merged}"
    staging.mkdir()
    declarations = []
    for member in members:
        source = ROOT / "crates" / f"{LIBRARY}-{member}" / "src"
        module = flattened[member]
        for path in sorted(source.rglob("*.rs")):
            relative = path.relative_to(source)
            if module:
                target = staging / (f"{module}.rs" if relative.as_posix() == "lib.rs" else f"{module}/{relative.as_posix()}")
            else:
                if relative.as_posix() == "lib.rs":
                    continue
                target = staging / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            git("mv", str(path.relative_to(ROOT)), str(target.relative_to(ROOT)))
        if module:
            declarations.append(f"pub mod {module};")
        else:
            lib = (source / "lib.rs").read_text()
            for match in re.finditer(r"^((?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*;)", lib, re.M):
                declarations.append(re.sub(r"\s+", " ", match.group(1)).replace("] pub", "]\npub"))
            git("rm", "-q", str((source / "lib.rs").relative_to(ROOT)))
        git("rm", "-q", str((ROOT / "crates" / f"{LIBRARY}-{member}" / "Cargo.toml").relative_to(ROOT)))
        subprocess.run(["rm", "-rf", str(ROOT / "crates" / f"{LIBRARY}-{member}")], check=True)
    staging.rename(directory)
    doc = "\n".join("//! " + line for line in textwrap.wrap(options.doc, 80))
    (directory / "src" / "lib.rs").write_text(doc + "\n" + "\n".join(sorted(declarations, key=lambda d: d.split("mod ")[-1])) + "\n")
    (directory / "Cargo.toml").write_text(manifest)

    # Manifests elsewhere and the workspace's list.
    for other in [ROOT / "Cargo.toml", ROOT / "crates" / f"{LIBRARY}-ui" / "Cargo.toml"] + sorted((ROOT / "crates").glob("*/Cargo.toml")):
        if other.is_file():
            rename_dependencies(other, members, merged, own=other.parent == directory)
    root = ROOT / "Cargo.toml"
    text = root.read_text()
    for member in members:
        text = re.sub(rf'^{LIBRARY}-{re.escape(member)} = \{{ path = "[^"]+" \}}\n', "", text, flags=re.M)
    anchor = "# The workspace's own crates.\n"
    entry = f'{LIBRARY}-{merged} = {{ path = "crates/{LIBRARY}-{merged}" }}\n'
    if entry not in text:
        text = text.replace(anchor, anchor + entry, 1)
    lines = text.split("\n")
    i = lines.index(anchor.rstrip("\n")) + 1
    j = i
    while j < len(lines) and lines[j].startswith(f"{LIBRARY}-"):
        j += 1
    lines[i:j] = sorted(lines[i:j])
    root.write_text("\n".join(lines))

    # The plan: one entry for the merged crate.
    modules = []
    for member in members:
        modules += planned_modules(plan_text, member)
        plan_text = re.sub(rf"\[crates\.{re.escape(member)}\]\n.*?(?=\n\[|\Z)", "", plan_text, flags=re.S)
    plan_text = re.sub(r"\n{3,}", "\n\n", plan_text).rstrip("\n")
    listed = ", ".join(f'"{m}"' for m in modules)
    plan_text += f'\n\n[crates.{merged}]\nband = "{options.band}"\n# {options.doc}\nmodules = [{listed}]\n'
    def rename(line: re.Match) -> str:
        listed = []
        for name in re.findall(r'"([^"]+)"', line.group(2)):
            name = merged if name in members else name
            if name not in listed:
                listed.append(name)
        return line.group(1) + "[" + ", ".join(f'"{n}"' for n in listed) + "]"

    plan_text = re.sub(r"^(may_depend_on = )(\[.*\])$", rename, plan_text, flags=re.M)
    PLAN.write_text(plan_text)
    print(f"merged {', '.join(members)} into {LIBRARY}-{merged}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
