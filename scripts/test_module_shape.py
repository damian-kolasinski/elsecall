"""Tests for module_shape.py: the line counter, the limits, the ratchet, and the
command end to end in a scratch git repository."""
from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import module_shape as shape
from module_shape import FILES, LINES

SCRIPT = Path(__file__).with_name("module_shape.py")


def lines(count: int, text: str = "let x = 1;") -> str:
    return "".join(f"    {text}\n" for _ in range(count))


class SourceLines(unittest.TestCase):
    def test_a_test_module_is_not_counted(self):
        text = "fn a() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n    }\n}\nfn b() {}\n"
        self.assertEqual(shape.source_lines(text), 2)

    def test_a_test_module_declared_by_path_is_one_item(self):
        text = "#[cfg(test)]\n#[path = \"x_tests.rs\"]\nmod tests;\nfn a() {}\n"
        self.assertEqual(shape.source_lines(text), 1)

    def test_a_test_only_function_with_its_brace_on_a_later_line(self):
        text = "#[cfg(test)]\nfn fixture(\n    a: u8,\n) -> u8\n{\n    a\n}\nfn a() {}\n"
        self.assertEqual(shape.source_lines(text), 1)

    def test_an_inline_test_only_import(self):
        self.assertEqual(shape.source_lines("#[cfg(test)] use std::fmt;\nfn a() {}\n"), 1)

    def test_braces_in_strings_chars_and_comments_do_not_end_the_item_early(self):
        text = (
            "#[cfg(test)]\nmod tests {\n"
            "    const A: &str = \"}}\";\n"
            "    const B: char = '}';\n"
            "    const C: &str = r#\"}\"#;\n"
            "    // }\n"
            "    /* } */\n"
            "    fn f<'a>(x: &'a str) -> &'a str { x }\n"
            "}\nfn after() {}\n"
        )
        self.assertEqual(shape.source_lines(text), 1)

    def test_other_cfgs_are_counted(self):
        self.assertEqual(shape.source_lines("#[cfg(target_os = \"macos\")]\nmod m {\n}\n"), 3)

    def test_a_test_and_platform_cfg_is_test_code(self):
        text = "#[cfg(all(test, target_os = \"macos\"))]\nmod tests {\n    fn t() {}\n}\nfn a() {}\n"
        self.assertEqual(shape.source_lines(text), 1)

    def test_a_test_only_last_field_does_not_swallow_the_next_item(self):
        text = (
            "struct S {\n    a: u8,\n    #[cfg(test)]\n    probe: u8,\n}\n"
            "struct T {\n    #[cfg(test)]\n    probe: u8\n}\n"
            "impl S {\n    fn f() {}\n}\n"
        )
        # Every line but the two `#[cfg(test)]` attributes and their fields.
        self.assertEqual(shape.source_lines(text), 12 - 4)

    def test_raw_and_byte_strings_and_escaped_quotes_end_where_they_should(self):
        text = (
            "const A: &str = r\"C:\\\";\n"
            "const B: &[u8] = br#\"a\"{b\"#;\n"
            "const C: char = '\\'';\n"
            "#[cfg(test)]\nmod tests {\n    fn t() {}\n}\nfn a() {}\n"
        )
        self.assertEqual(shape.source_lines(text), 4)


class Shape(unittest.TestCase):
    def test_test_files_are_measured_apart_and_not_counted_in_folders(self):
        result = shape.shape_of({"src/a.rs": "fn a() {}\n", "src/a_tests.rs": "", "src/b/tests.rs": "", "tests/x.rs": ""})
        self.assertEqual(set(result.sources), {"src/a.rs"})
        self.assertEqual(result.folders, {"src": 1})
        self.assertEqual(set(result.tests), {"src/a_tests.rs", "src/b/tests.rs", "tests/x.rs"})

    def test_catch_all_names_as_files_folders_and_mod_rs(self):
        result = shape.shape_of({
            "src/utils.rs": "", "src/common/mod.rs": "", "src/core/x.rs": "",
            "src/fleet_upgrade/helper.rs": "", "src/scoring.rs": "",
            "crates/design-system/src/lib.rs": "", "tests/common/mod.rs": "",
        })
        self.assertEqual(result.catch_all, ("crates/design-system", "src/common", "src/core", "src/utils.rs"))


def a_shape(sources=None, folders=None) -> shape.Shape:
    return shape.Shape(sources or {}, {}, folders or {}, ())


class Findings(unittest.TestCase):
    def test_limits(self):
        found = shape.findings(a_shape({"ok.rs": 800, "soft.rs": 801, "hard.rs": 1501}, {"wide": 41, "too_wide": 61}), {}, {})
        self.assertEqual(
            [(f.error, f.path) for f in found],
            [(True, "hard.rs"), (True, "too_wide"), (False, "soft.rs"), (False, "wide")],
        )

    def test_a_listed_file_fails_only_above_its_ceiling(self):
        entries = {(LINES, "big.rs"): 2000, (FILES, "src"): 146}
        at = shape.findings(a_shape({"big.rs": 2000}, {"src": 146}), entries, {})
        self.assertFalse(any(f.error for f in at))
        above = shape.findings(a_shape({"big.rs": 2001}, {"src": 147}), entries, {})
        self.assertEqual([f.path for f in above if f.error], ["big.rs", "src"])

    def test_a_renamed_file_keeps_its_ceiling(self):
        entries = {(LINES, "src/big.rs"): 2000}
        moved = shape.findings(a_shape({"crates/x/src/big.rs": 1900}), entries, {"crates/x/src/big.rs": "src/big.rs"})
        # The file passes; the list still names the old path until --shrink.
        self.assertEqual([f.path for f in moved if f.error], [shape.EXCEPTIONS])


class Ratchet(unittest.TestCase):
    def test_lowering_and_dropping_pass(self):
        base = {(LINES, "a.rs"): 2000, (LINES, "b.rs"): 1700}
        self.assertEqual(shape.ratchet({(LINES, "a.rs"): 1900}, base, {}), [])

    def test_adding_or_raising_fails(self):
        base = {(LINES, "a.rs"): 2000}
        found = shape.ratchet({(LINES, "a.rs"): 2001, (FILES, "src"): 70}, base, {})
        self.assertEqual(len(found), 2)
        self.assertIn("raises `lines a.rs` from 2000 to 2001", found[1].message + found[0].message)

    def test_a_renamed_entry_is_held_to_its_old_ceiling(self):
        base = {(LINES, "src/a.rs"): 2000}
        self.assertEqual(shape.ratchet({(LINES, "x/a.rs"): 2000}, base, {"x/a.rs": "src/a.rs"}), [])
        self.assertEqual(len(shape.ratchet({(LINES, "x/a.rs"): 2001}, base, {"x/a.rs": "src/a.rs"})), 1)


class Shrink(unittest.TestCase):
    def test_lowers_moves_and_drops_but_never_adds(self):
        entries = {(LINES, "a.rs"): 2000, (LINES, "old/b.rs"): 1800, (LINES, "fixed.rs"): 1600}
        measured = a_shape({"a.rs": 1700, "new/b.rs": 1900, "fixed.rs": 900, "fresh.rs": 1600})
        self.assertEqual(
            shape.shrunk(measured, entries, {"new/b.rs": "old/b.rs"}),
            {(LINES, "a.rs"): 1700, (LINES, "new/b.rs"): 1800},
        )


class Command(unittest.TestCase):
    """The script in a scratch repository with a base commit."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "me@example.com")
        self.git("config", "user.name", "me")
        self.write("src/big.rs", "fn a() {\n" + lines(1600) + "}\n")
        self.write("scripts/module-shape-exceptions.txt", "lines src/big.rs 1602\n")
        # The base has the guard, so the list is held to the base's.
        self.write("scripts/module_shape.py", "")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "base")
        self.base = self.git("rev-parse", "HEAD").strip()

    def tearDown(self):
        self.directory.cleanup()

    def git(self, *args: str) -> str:
        return subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True, text=True).stdout

    def write(self, path: str, text: str) -> None:
        (self.root / path).parent.mkdir(parents=True, exist_ok=True)
        (self.root / path).write_text(text)

    def run_script(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, "-I", str(SCRIPT), "--base", self.base, *args],
            cwd=self.root, capture_output=True, text=True,
        )

    def test_a_listed_file_at_its_ceiling_passes(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_a_move_carries_its_ceiling_into_the_list_and_the_next_change_passes(self):
        (self.root / "src/placing").mkdir()
        self.git("mv", "src/big.rs", "src/placing/big.rs")
        # Left as it was, the list would lose the file for every later change.
        result = self.run_script()
        self.assertEqual(result.returncode, 1)
        self.assertIn("lists `lines src/big.rs`, which no longer exists", result.stdout)
        self.assertNotIn("error  src/placing/big.rs", result.stdout)
        self.assertEqual(self.run_script("--shrink").returncode, 0)
        self.assertIn("lines src/placing/big.rs 1602", (self.root / "scripts/module-shape-exceptions.txt").read_text())
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stdout)
        # The next change's base has the move; nothing is renamed any more.
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "move")
        self.base = self.git("rev-parse", "HEAD").strip()
        self.write("src/other.rs", "fn b() {}\n")
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_warnings_show_only_for_touched_files_unless_all(self):
        self.write("src/old.rs", "fn a() {\n" + lines(900) + "}\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "a big file")
        self.base = self.git("rev-parse", "HEAD").strip()
        self.write("src/new.rs", "fn a() {\n" + lines(900) + "}\n")
        result = self.run_script()
        self.assertIn("warn   src/new.rs", result.stdout)
        self.assertNotIn("src/old.rs", result.stdout)
        self.assertIn("in files this change did not touch", result.stdout)
        self.assertIn("warn   src/old.rs", self.run_script("--all").stdout)

    def test_a_new_oversized_file_fails_and_listing_it_fails_too(self):
        self.write("src/new.rs", "fn a() {\n" + lines(1600) + "}\n")
        self.assertIn("error  src/new.rs", self.run_script().stdout)
        with open(self.root / "scripts/module-shape-exceptions.txt", "a") as listed:
            listed.write("lines src/new.rs 1602\n")
        result = self.run_script()
        self.assertEqual(result.returncode, 1)
        self.assertIn("adds `lines src/new.rs`", result.stdout)

    def test_the_change_that_adds_the_guard_writes_its_first_list(self):
        self.git("rm", "-q", "scripts/module_shape.py", "scripts/module-shape-exceptions.txt")
        self.git("commit", "-q", "-m", "before the guard")
        self.base = self.git("rev-parse", "HEAD").strip()
        self.write("scripts/module-shape-exceptions.txt", "lines src/big.rs 1602\n")
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_raising_a_ceiling_by_hand_fails(self):
        self.write("scripts/module-shape-exceptions.txt", "lines src/big.rs 1700\n")
        result = self.run_script()
        self.assertEqual(result.returncode, 1)
        self.assertIn("ceilings only go down", result.stdout)


if __name__ == "__main__":
    unittest.main()
