"""Tests for check_selection.py, against a small workspace in a temporary repository."""
from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path

import check_selection

FILES = {
    "Cargo.toml": """
[workspace]
members = [".", "crates/desk", "crates/tool"]
[package]
name = "acme"
""",
    "Cargo.lock": "",
    "crates/desk/Cargo.toml": """
[package]
name = "acme-desk"
[dependencies]
acme = { path = "../.." }
""",
    "crates/desk/src/main.rs": "fn main() {}\n",
    "crates/tool/Cargo.toml": '[package]\nname = "acme-tool"\n',
    "crates/tool/src/lib.rs": "",
    "src/lib.rs": "pub mod reader;\npub mod settings;\npub mod fleet;\npub mod moved;\n",
    "src/main.rs": "fn main() {}\n",
    "src/reader.rs": 'pub const NAME: &str = "state.json";\n',
    "src/settings.rs": """pub fn parse() {}

#[cfg(test)]
mod tests {
    #[test]
    fn readme_examples_parse() {
        let readme = include_str!("../README.md");
        // Not a read: "../docs/guide.md"
    }
}
""",
    "src/fleet.rs": """pub fn copies() {}

#[cfg(test)]
mod helpers {
    pub const SAMPLE: &str = include_str!("../tests/members/sample.toml");
}

#[cfg(test)]
mod copies_tests {}
""",
    "src/moved.rs": '#[path = "elsewhere/mod.rs"]\nmod inner;\n',
    "src/elsewhere/mod.rs": """#[cfg(test)]
mod tests {
    const DATA: &str = include_str!("../../tests/fixtures/remapped.json");
}
""",
    "tests/fleet.rs": "mod members;\n",
    "tests/trace.rs": 'pub(crate) mod clock;\nconst SAMPLE: &str = include_str!("fixtures/sample.rs");\n',
    "tests/multi/main.rs": "",
    "tests/members/sample.toml": "",
    "tests/fixtures/sample.rs": "",
    "tests/fixtures/remapped.json": "",
    "tests/report.rs": "mod members;\nmod clock;\n",
    "tests/install.rs": 'const FIXTURE: &str = include_str!("fixtures/install.toml");\n',
    "tests/scripts.rs": 'const CHECK: &str = include_str!("../scripts/check.sh");\n',
    "tests/members/mod.rs": "",
    "tests/clock/mod.rs": "",
    "tests/fixtures/install.toml": "",
    "tests/fixtures/state.json": "",
    "tests/fixtures/orphan.toml": "",
    "README.md": "See [the guide](docs/guide.md).\n",
    "docs/guide.md": "",
    ".github/workflows/check.yml": "",
    "scripts/check.sh": ". scripts/lib/common.sh\n",
    "scripts/lib/common.sh": "",
    "scripts/scan.py": "",
    "scripts/test_scan.py": "import scan\n",
    "scripts/test-environment.py": "",
    "distribution/package.py": "",
    "distribution/install.sh": "",
    "mise.toml": '[tasks.check]\nrun = "./scripts/check.sh"\n',
}


class SelectionTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory()
        cls.root = Path(cls.directory.name)
        for name, content in FILES.items():
            path = cls.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
        subprocess.run(["git", "init", "-q"], cwd=cls.root, check=True)

    @classmethod
    def tearDownClass(cls):
        cls.directory.cleanup()

    def select(self, *paths: str) -> check_selection.Selection:
        return check_selection.select(self.root, list(paths))

    def assertRuns(self, paths, rust=(), python=()):
        selection = self.select(*paths)
        self.assertIsNone(selection.everything, selection.reasons)
        self.assertEqual(selection.targets.rust, set(rust))
        self.assertEqual(selection.targets.python, set(python))

    def assertEverything(self, *paths):
        selection = self.select(*paths)
        self.assertIsNotNone(selection.everything, selection.reasons)
        return selection.everything.reason

    def test_a_test_binary_selects_itself(self):
        self.assertRuns(["tests/fleet.rs"], rust={"binary_id(acme::fleet)"})

    def test_a_deleted_test_binary_selects_nothing(self):
        self.assertRuns(["tests/retired.rs"])

    def test_a_shared_test_module_selects_the_binaries_declaring_it(self):
        self.assertRuns(
            ["tests/members/mod.rs"],
            rust={"binary_id(acme::fleet)", "binary_id(acme::report)"},
        )
        self.assertRuns(
            ["tests/clock/mod.rs"],
            rust={"binary_id(acme::report)", "binary_id(acme::trace)"},
        )

    def test_a_test_binary_in_its_own_directory_selects_itself(self):
        self.assertRuns(["tests/multi/main.rs"], rust={"binary_id(acme::multi)"})

    def test_rust_data_in_tests_selects_whoever_names_it(self):
        self.assertRuns(["tests/fixtures/sample.rs"], rust={"binary_id(acme::trace)"})
        # Inside a shared module's directory: its binaries, and a unit test
        # helper whose sibling modules use it.
        self.assertRuns(
            ["tests/members/sample.toml"],
            rust={
                "binary_id(acme::fleet)",
                "binary_id(acme::report)",
                "(package(acme) & kind(lib) & test(/^fleet::/))",
            },
        )

    def test_a_directory_a_path_attribute_moves_selects_every_unit_test(self):
        self.assertRuns(["tests/fixtures/remapped.json"], rust={"(package(acme) & kind(lib))"})

    def test_a_fixture_selects_the_tests_naming_it(self):
        self.assertRuns(["tests/fixtures/install.toml"], rust={"binary_id(acme::install)"})

    def test_a_name_in_a_unit_test_module_selects_that_module(self):
        self.assertRuns(
            ["README.md"],
            rust={"(package(acme) & kind(lib) & test(/^settings::/))"},
        )

    def test_a_name_in_library_code_selects_everything(self):
        reason = self.assertEverything("tests/fixtures/state.json")
        self.assertIn("src/reader.rs", reason)

    def test_names_in_comments_and_documents_do_not_select(self):
        # The README links the guide; a comment in a unit test quotes it.
        self.assertRuns(["docs/guide.md"])
        self.assertRuns([".github/workflows/check.yml"])

    def test_a_script_selects_the_tests_including_it_and_what_runs_it(self):
        self.assertRuns(["scripts/check.sh"], rust={"binary_id(acme::scripts)"})
        # common.sh is reached through check.sh; mise.toml naming check.sh is
        # configuration, not an input of every test.
        self.assertRuns(["scripts/lib/common.sh"], rust={"binary_id(acme::scripts)"})

    def test_python_sources_select_their_unit_tests(self):
        self.assertRuns(["scripts/scan.py"], python={"scripts"})
        self.assertRuns(["distribution/install.sh"], python={"distribution"})

    def test_a_crate_nothing_depends_on_selects_its_package(self):
        self.assertRuns(["crates/tool/src/lib.rs"], rust={"package(acme-tool)"})
        self.assertRuns(["crates/desk/src/main.rs"], rust={"package(acme-desk)"})

    def test_a_crate_others_depend_on_selects_them_too(self):
        files = dict(FILES)
        files["crates/tool/Cargo.toml"] = '[package]\nname = "acme-tool"\n[dependencies]\nacme-base.workspace = true\n'
        files["crates/base/Cargo.toml"] = '[package]\nname = "acme-base"\n'
        files["crates/base/src/lib.rs"] = ""
        files["Cargo.toml"] = (
            '[workspace]\nmembers = [".", "crates/*"]\n'
            '[workspace.dependencies]\nacme-base = { path = "crates/base" }\n'
            '[package]\nname = "acme"\n'
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, content in files.items():
                (root / name).parent.mkdir(parents=True, exist_ok=True)
                (root / name).write_text(content)
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            selection = check_selection.select(root, ["crates/base/src/lib.rs"])
        self.assertIsNone(selection.everything, selection.reasons)
        self.assertEqual(selection.targets.rust, {"rdeps(acme-base)"})

    def test_library_sources_and_workspace_inputs_select_everything(self):
        for path in [
            "src/reader.rs",
            "Cargo.lock",
            # Cargo unifies features across packages.
            "crates/desk/Cargo.toml",
            "mise.toml",
            "scripts/test-environment.py",
            ".config/nextest.toml",
        ]:
            with self.subTest(path):
                self.assertEverything(path)

    def test_an_unknown_path_selects_everything(self):
        self.assertIn("no test declares or names", self.assertEverything("tests/fixtures/orphan.toml"))
        self.assertIn("no rule", self.assertEverything("assets/logo.bin"))

    def test_selections_combine_and_everything_wins(self):
        selection = self.select("tests/fleet.rs", "scripts/scan.py")
        self.assertEqual(selection.nextest_filter(), "binary_id(acme::fleet)")
        self.assertEqual(selection.targets.python, {"scripts"})
        self.assertIsNotNone(self.select("tests/fleet.rs", "src/reader.rs").everything)


class RepositoryTest(unittest.TestCase):
    """The rules against this repository."""

    root = Path(__file__).resolve().parent.parent

    def test_names_in_the_selection_fixture_do_not_count_as_reads(self):
        # test_check_selection.py names orphan.toml in its fixture repository.
        selection = check_selection.select(self.root, ["tests/fixtures/orphan.toml"])
        self.assertIsNotNone(selection.everything, selection.reasons)


if __name__ == "__main__":
    unittest.main()
