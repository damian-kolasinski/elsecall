"""Tests for changed_paths.py, against a temporary repository."""
from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path

import changed_paths


def git(root: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-c", "user.name=me", "-c", "user.email=me@example.com", *args],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    ).stdout


class ChangedPathsTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        git(self.root, "init", "-q", "-b", "main")
        for name in ["kept.rs", "moved.rs", "edited.rs", "staged.rs"]:
            (self.root / name).write_text(f"// {name}\n" * 20)
        git(self.root, "add", ".")
        git(self.root, "commit", "-q", "-m", "base")
        git(self.root, "switch", "-q", "-c", "work")

    def test_commits_staged_unstaged_and_untracked_changes_count_with_both_rename_names(self):
        git(self.root, "mv", "moved.rs", "renamed.rs")
        git(self.root, "commit", "-q", "-m", "rename")
        (self.root / "staged.rs").write_text("// staged\n")
        git(self.root, "add", "staged.rs")
        (self.root / "edited.rs").write_text("// edited\n")
        (self.root / "new.rs").write_text("// new\n")
        base, paths = changed_paths.changed(self.root, "main")
        self.assertEqual(base, git(self.root, "rev-parse", "main").strip())
        self.assertEqual(paths, ["edited.rs", "moved.rs", "new.rs", "renamed.rs", "staged.rs"])

    def test_the_default_base_is_the_first_known_target(self):
        (self.root / "edited.rs").write_text("// edited\n")
        self.assertEqual(changed_paths.changed(self.root)[1], ["edited.rs"])

    def test_an_unknown_base_is_reported(self):
        with self.assertRaises(changed_paths.NoBase):
            changed_paths.changed(self.root, "nowhere")


if __name__ == "__main__":
    unittest.main()
