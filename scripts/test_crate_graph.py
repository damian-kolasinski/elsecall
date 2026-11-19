"""Tests for crate_graph.py: reading paths from Rust, resolving them to planned
crates, finding cycles, and the exceptions ratchet."""
from __future__ import annotations

import unittest

import crate_graph as graph
import module_shape as shape

PLAN = """
bands = ["foundation", "core", "feature", "entry"]
[band.foundation]
max_lines = 3
forbid = ["axum"]
[band.core]
[band.feature]
[band.entry]
[crates.failure]
band = "foundation"
modules = ["failure"]
[crates.scheduling-model]
band = "foundation"
modules = ["scheduling::timestamp"]
[crates.scheduling]
band = "core"
modules = ["scheduling"]
[crates.rehearsal]
band = "feature"
modules = ["rehearsal"]
[crates.cli]
band = "entry"
binary = "src/main.rs"
[crates.ui]
band = "entry"
packages = ["elsecall-ui"]
may_depend_on = ["failure"]
"""


def build(files: dict[str, str]) -> tuple[graph.Plan, graph.Graph]:
    plan = graph.read_plan(PLAN)
    packages = {"": "elsecall", "crates/elsecall-ui": "elsecall-ui"}
    return plan, graph.build(None, plan, files, packages)


class ReadingPaths(unittest.TestCase):
    def test_use_trees_expand_with_self_and_globs(self):
        self.assertEqual(
            graph.expand_use("crate::{a, b::{c, self}, d::*}"),
            ["crate::a", "crate::b::c", "crate::b", "crate::d"],
        )

    def test_paths_in_strings_and_comments_are_not_references(self):
        code = graph.blank(shape.production_lines(
            '/// See crate::rehearsal::synthetic.\nlet s = "crate::rehearsal::x"; // crate::rehearsal\nlet t = crate::failure::Failure::io;\n'
        ))
        self.assertEqual([path for _, path in graph.references(code)], ["crate::failure::Failure::io"])

    def test_aliases_and_multiline_uses(self):
        code = "use crate::{\n    failure::Failure as F,\n    rehearsal,\n};\nfn f() {}\n"
        self.assertEqual(sorted(path for _, path in graph.references(code)), ["crate::failure::Failure", "crate::rehearsal"])

    def test_references_keep_their_line_numbers_past_test_code(self):
        text = "#[cfg(test)]\nmod tests {\n    use crate::rehearsal;\n}\nuse crate::failure;\n"
        code = graph.blank(shape.production_lines(text))
        self.assertEqual(graph.references(code), [(5, "crate::failure")])

    def test_relative_paths_resolve_from_the_file(self):
        here = ("scheduling", "service", "running")
        self.assertEqual(graph.resolve("super::super::timestamp::Timestamp", here, "crate"), ("scheduling", "timestamp", "Timestamp"))
        self.assertEqual(graph.resolve("self::x", here, "crate"), here + ("x",))
        self.assertEqual(graph.resolve("elsecall::failure::Failure", (), "elsecall"), ("failure", "Failure"))
        self.assertIsNone(graph.resolve("serde::Serialize", here, "crate"))

    def test_super_inside_an_inline_module_means_the_file_itself(self):
        code = "fn a() {}\nmod tests {\n    use super::*;\n    mod deeper {\n        use super::super::x;\n    }\n}\nuse super::y;\n"
        nested = graph.inline_modules(code)
        self.assertEqual(nested[2], ("tests",))
        self.assertEqual(nested[4], ("tests", "deeper"))
        self.assertEqual(nested[7], ())
        here = ("scheduling", "capacity")
        self.assertEqual(graph.resolve("super::*", here + nested[2], "crate"), ("scheduling", "capacity", "*"))
        self.assertEqual(graph.resolve("super::super::x", here + nested[4], "crate"), ("scheduling", "capacity", "x"))
        self.assertEqual(graph.resolve("super::y", here + nested[7], "crate"), ("scheduling", "y"))

    def test_the_longest_listed_module_path_wins(self):
        plan = graph.read_plan(PLAN)
        self.assertEqual(graph.assign(plan, ("scheduling", "timestamp", "Timestamp")), "scheduling-model")
        self.assertEqual(graph.assign(plan, ("scheduling", "record")), "scheduling")
        self.assertIsNone(graph.assign(plan, ("nowhere",)))


class Checking(unittest.TestCase):
    def kinds(self, files: dict[str, str]) -> dict[graph.Key, str]:
        plan, built = build(files)
        return {v.key: v.reason for v in graph.violations(plan, built)}

    def test_an_upward_reference_is_the_edge_to_cut_not_the_downward_one(self):
        found = self.kinds({
            "src/scheduling/request.rs": "fn f() { crate::rehearsal::synthetic(None); }\n",
            "src/rehearsal.rs": "use crate::scheduling::request;\n",
        })
        self.assertIn("higher feature band", found[("edge", "scheduling", "rehearsal")])
        self.assertNotIn(("edge", "rehearsal", "scheduling"), found)

    def test_a_cycle_within_one_band(self):
        plan = graph.read_plan(PLAN + '[crates.fleet]\nband = "feature"\nmodules = ["fleet"]\n')
        files = {"src/fleet.rs": "use crate::rehearsal::x;\n", "src/rehearsal.rs": "use crate::fleet::y;\n"}
        built = graph.build(None, plan, files, {"": "elsecall"})
        found = {v.key: v.reason for v in graph.violations(plan, built)}
        self.assertIn("cycle of 2 crates within the feature band", found[("edge", "fleet", "rehearsal")])
        self.assertIn(("edge", "rehearsal", "fleet"), found)
        del files["src/rehearsal.rs"]
        built = graph.build(None, plan, files, {"": "elsecall"})
        self.assertEqual(graph.violations(plan, built), [])

    def test_a_downward_reference_is_fine(self):
        self.assertEqual(self.kinds({"src/rehearsal.rs": "use crate::scheduling::timestamp::Timestamp;\n"}), {})

    def test_test_code_is_held_apart_from_production_code(self):
        files = {
            "src/scheduling.rs": "#[cfg(test)]\nmod tests {\n    use crate::rehearsal;\n}\n",
            "src/scheduling/service_tests.rs": "use crate::rehearsal;\n",
        }
        found = self.kinds(files)
        self.assertEqual(list(found), [("testedge", "scheduling", "rehearsal")])
        self.assertIn("test code in a core crate uses the higher feature band", found[("testedge", "scheduling", "rehearsal")])

    def test_an_out_of_line_test_module_is_test_code(self):
        files = {
            "src/scheduling.rs": "#[cfg(test)]\npub(crate) mod fixtures;\n",
            "src/scheduling/fixtures.rs": "use crate::rehearsal;\n",
            "src/scheduling/fixtures/deeper.rs": "use crate::rehearsal::x;\n",
        }
        plan, built = build(files)
        self.assertEqual(dict(built.edges), {})
        self.assertEqual(len(built.test_edges[("scheduling", "rehearsal")]), 2)
        self.assertEqual(built.lines["scheduling"], 0)

    def test_the_allowlist_holds_another_package_to_its_contract(self):
        found = self.kinds({"crates/elsecall-ui/src/view.rs": "use elsecall::{failure::Failure, scheduling::request};\n"})
        self.assertEqual(list(found), [("edge", "ui", "scheduling")])

    def test_forbidden_frameworks_and_the_line_budget(self):
        found = self.kinds({"src/failure.rs": "use axum::Router;\nfn a() {}\nfn b() {}\nfn c() {}\n"})
        self.assertIn(("extern", "failure", "axum"), found)
        self.assertIn(("lines", "failure", ""), found)

    def test_an_unplanned_module_or_package_is_reported(self):
        plan = graph.read_plan(PLAN)
        packages = {"": "elsecall", "crates/elsecall-new": "elsecall-new"}
        built = graph.build(None, plan, {"src/brand_new.rs": "", "crates/elsecall-new/src/lib.rs": ""}, packages)
        self.assertEqual(sorted(built.unassigned), ["crates/elsecall-new", "src/brand_new"])

    def test_catch_all_crate_names_fail(self):
        plan = graph.read_plan(PLAN + '[crates.shared-types]\nband = "core"\nmodules = []\n')
        built = graph.build(None, plan, {}, {"": "elsecall"})
        messages = [f.message for f in graph.findings(plan, built, {}) if f.error]
        self.assertTrue(any("shared-types" in message for message in messages))


class Cuts(unittest.TestCase):
    def test_suggested_cuts_break_every_cycle_and_skip_unneeded_ones(self):
        weights = {("a", "b"): 5, ("b", "a"): 1, ("b", "c"): 4, ("c", "a"): 2, ("c", "b"): 3}
        cuts = graph.suggested_cuts({"a", "b", "c"}, weights)
        left = set(weights) - set(cuts)
        self.assertEqual(graph.cycles({"a", "b", "c"}, left), [])
        for cut in cuts:
            self.assertNotEqual(graph.cycles({"a", "b", "c"}, left | {cut}), [], cut)


class Ratchet(unittest.TestCase):
    FILES = {
        "src/scheduling/request.rs": "fn f() { crate::rehearsal::a(); crate::rehearsal::b(); }\n",
        "src/rehearsal.rs": "use crate::scheduling::request;\n",
    }

    def errors(self, listed):
        plan, built = build(self.FILES)
        return [f.message for f in graph.findings(plan, built, listed) if f.error]

    def test_a_listed_edge_passes_only_at_its_exact_count(self):
        self.assertEqual(self.errors({("edge", "scheduling", "rehearsal"): 2}), [])
        self.assertTrue(any("over the listed 1" in m for m in self.errors({("edge", "scheduling", "rehearsal"): 1})))
        # Room freed by a fix must be recorded, or a later change could spend it.
        self.assertTrue(any("run --shrink" in m for m in self.errors({("edge", "scheduling", "rehearsal"): 3})))
        resolved = {("edge", "scheduling", "rehearsal"): 2, ("edge", "rehearsal", "fleet"): 1}
        self.assertTrue(any("is resolved" in m for m in self.errors(resolved)))

    def test_shrinking_lowers_counts_and_drops_resolved_edges(self):
        plan, built = build({"src/scheduling/request.rs": "fn f() { crate::rehearsal::a(); }\n", "src/rehearsal.rs": ""})
        listed = {("edge", "scheduling", "rehearsal"): 2, ("edge", "rehearsal", "scheduling"): 1}
        self.assertEqual(graph.shrunk(plan, built, listed), {("edge", "scheduling", "rehearsal"): 1})

    def test_the_list_may_not_gain_or_raise_entries_against_the_base(self):
        base = {("edge", "a", "b"): 2}
        self.assertEqual(graph.ratchet({("edge", "a", "b"): 2}, base), [])
        self.assertEqual(len(graph.ratchet({("edge", "a", "b"): 3}, base)), 1)
        self.assertEqual(len(graph.ratchet({("edge", "a", "c"): 1}, base)), 1)

    def test_the_list_round_trips(self):
        entries = {("edge", "a", "b"): 2, ("extern", "a", "axum"): 1, ("lines", "a", ""): 1600}
        self.assertEqual(graph.parse_exceptions(graph.format_exceptions(entries)), entries)


if __name__ == "__main__":
    unittest.main()
