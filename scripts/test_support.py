#!/usr/bin/env python3
"""Expose a crate's test-only module to other crates' tests, and turn it on
where they use it.

`#[cfg(test)]` on `mod MODULE;` becomes `#[cfg(any(test, feature =
"test-support"))]` (with MODULE omitted, only the `test-support` feature is
added, for fixtures gated by hand), the crate gains the feature, and every
package whose code names `elsecall_<crate>::...MODULE` gets a dev-dependency on
the crate with the feature on, so `cargo test -p` builds them on their own.
`scripts/crate_graph.py` checks that every user of a test-support module asks
for it.

Usage: test_support.py CRATE [MODULE]
"""
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def expose(directory: Path, module: str) -> None:
    for source in sorted((directory / "src").rglob("*.rs")):
        text = source.read_text()
        new = re.sub(
            rf"#\[cfg\(test\)\](\s*)(pub(?:\([^)]*\))?\s+)?mod {re.escape(module)}\s*;",
            rf'#[cfg(any(test, feature = "test-support"))]\1pub mod {module};',
            text,
        )
        if new != text:
            source.write_text(new)
            return
    if not any(
        re.search(rf'feature = "test-support"\)\)\]\s*pub mod {re.escape(module)}\b', s.read_text())
        for s in (directory / "src").rglob("*.rs")
    ):
        raise SystemExit(f"no #[cfg(test)] mod {module}; in {directory}")


def add_feature(manifest: Path, enables: list[str]) -> None:
    text = manifest.read_text()
    if "test-support" in text:
        return
    value = "[" + ", ".join(f'"{e}"' for e in enables) + "]"
    text = text.replace(
        "[dependencies]",
        f"[features]\n# Fixtures other crates' tests build with.\ntest-support = {value}\n\n[dependencies]",
        1,
    )
    manifest.write_text(text)


def enable_in(manifest: Path, crate: str) -> None:
    """A dev-dependency on elsecall-<crate> with test-support, in `manifest`."""
    text = manifest.read_text()
    line = f'elsecall-{crate} = {{ workspace = true, features = ["test-support"] }}\n'
    if line in text:
        return
    if "[dev-dependencies]\n" in text:
        text = text.replace("[dev-dependencies]\n", "[dev-dependencies]\n" + line, 1)
    else:
        text = text.rstrip("\n") + "\n\n[dev-dependencies]\n" + line
    manifest.write_text(text)


def users(crate: str, module: str) -> list[Path]:
    """Manifests of the packages whose code names the module."""
    ident = f"elsecall_{crate.replace('-', '_')}"
    pattern = re.compile(rf"\b{ident}::(?:\w+::)*(?:\{{[^}}]*\b{re.escape(module)}\b|{re.escape(module)}\b)")
    found = set()
    packages = [ROOT] + sorted(p.parent for p in (ROOT / "crates").glob("*/Cargo.toml"))
    for package in packages:
        if package.name == f"elsecall-{crate}":
            continue
        roots = [package / "src", package / "tests"] if package != ROOT else [ROOT / "src", ROOT / "tests"]
        for root in roots:
            if root.is_dir() and any(pattern.search(s.read_text()) for s in root.rglob("*.rs")):
                found.add(package / "Cargo.toml")
    return sorted(found)


def main(crate: str, module: str | None = None) -> None:
    directory = ROOT / "crates" / f"elsecall-{crate}"
    manifest = directory / "Cargo.toml"
    if module:
        expose(directory, module)
    # A crate's fixtures may build on another crate's.
    dependencies = tomllib.loads(manifest.read_text()).get("dependencies", {})
    enables = []
    for name in dependencies:
        other = ROOT / "crates" / name / "Cargo.toml"
        if other.is_file() and "test-support" in other.read_text():
            enables.append(f"{name}/test-support")
    add_feature(manifest, enables)
    for user in users(crate, module or ""):
        enable_in(user, crate)


if __name__ == "__main__":
    main(*sys.argv[1:])
