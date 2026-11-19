"""Tests for crate_split.py: masking Rust source, use trees, and rewriting a
library's paths onto a moved crate."""
from __future__ import annotations

import unittest

import crate_split as split


class Masking(unittest.TestCase):
    def test_comments_and_literals_are_blanked_in_place(self):
        text = 'let a = "crate::x"; // crate::y\nlet b = crate::z; /* crate::w */\n'
        masked = split.mask(text)
        self.assertEqual(len(masked), len(text))
        self.assertNotIn("crate::x", masked)
        self.assertNotIn("crate::y", masked)
        self.assertNotIn("crate::w", masked)
        self.assertIn("crate::z", masked)


class UseTrees(unittest.TestCase):
    def test_split_and_nest_round_trip(self):
        leaves = split.split_tree("crate::{a::{b, self}, c::d}")
        self.assertEqual(leaves, ["crate::a::b", "crate::a", "crate::c::d"])
        self.assertEqual(split.nest(leaves), "crate::{a::{self, b}, c::d}")

    def test_nest_keeps_aliases(self):
        self.assertEqual(split.nest(["x::y as z", "x::w"]), "x::{y as z, w}")


class Rewriting(unittest.TestCase):
    def rewrite(self, text: str, moving: bool, here=("scheduling",), root="crate") -> str:
        move = split.Move("failure", [("failure",)], flatten=True)
        rewriter = split.Rewriter(move, {}, [])
        context = split.Context("src/x.rs", root, here, moving, False)
        new, _ = rewriter.rewrite(text, context)
        self.assertEqual(rewriter.problems, [])
        return new

    def test_a_flattened_module_keeps_its_imported_name(self):
        # One statement per destination; rustfmt orders them.
        self.assertEqual(
            sorted(self.rewrite("use crate::{failure, scheduling::x};\n", moving=False).splitlines()),
            ["use crate::scheduling::x;", "use elsecall_failure as failure;"],
        )

    def test_paths_after_a_colon_and_macros_are_rewritten(self):
        text = 'json!({"code":crate::failure::Failure::new()});\ncrate::failure!(x);\n'
        new = self.rewrite(text, moving=False)
        self.assertIn('"code":elsecall_failure::Failure::new()', new)
        self.assertIn("elsecall_failure::failure!(x)", new)

    def test_code_outside_the_library_names_the_crate(self):
        self.assertEqual(
            self.rewrite("use elsecall::failure::Failure;\n", moving=False, here=(), root="elsecall"),
            "use elsecall_failure::Failure;\n",
        )

    def test_the_moved_crate_names_itself_crate(self):
        self.assertEqual(
            self.rewrite("let f = crate::failure::Failure::new();\n", moving=True, here=("failure",)),
            "let f = crate::Failure::new();\n",
        )


if __name__ == "__main__":
    unittest.main()
