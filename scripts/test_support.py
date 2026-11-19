#!/usr/bin/env python3
"""Expose a crate's test-only module to other crates' tests: `#[cfg(test)]` on
`mod NAME;` becomes `#[cfg(any(test, feature = "test-support"))]`, the crate
gains a `test-support` feature, and the root package's tests turn it on.

Usage: test_support.py CRATE MODULE
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def main(crate: str, module: str) -> None:
    directory = ROOT / "crates" / f"elsecall-{crate}"
    for source in sorted((directory / "src").rglob("*.rs")):
        text = source.read_text()
        new = re.sub(
            rf"#\[cfg\(test\)\](\s*)(pub(?:\([^)]*\))?\s+)?mod {re.escape(module)}\s*;",
            rf'#[cfg(any(test, feature = "test-support"))]\1pub mod {module};',
            text,
        )
        if new != text:
            source.write_text(new)
            break
    else:
        raise SystemExit(f"no #[cfg(test)] mod {module}; in {directory}")
    manifest = directory / "Cargo.toml"
    text = manifest.read_text()
    if "test-support" not in text:
        text = text.replace(
            "[dependencies]",
            "[features]\n# Fixtures other crates' tests build with.\ntest-support = []\n\n[dependencies]",
            1,
        )
        manifest.write_text(text)
    root = ROOT / "Cargo.toml"
    text = root.read_text()
    line = f'elsecall-{crate} = {{ workspace = true, features = ["test-support"] }}\n'
    if line not in text:
        text = text.replace("[dev-dependencies]\n", "[dev-dependencies]\n" + line, 1)
        root.write_text(text)


if __name__ == "__main__":
    main(*sys.argv[1:])
