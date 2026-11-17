"""Tests for the tag and Cargo version check."""

import json
import subprocess
import tempfile
import unittest
from pathlib import Path

import package
import release_version


class ReleaseVersion(unittest.TestCase):
    def test_tag_must_match_the_cargo_version(self):
        self.assertEqual(
            release_version.metadata("0.1.0", "v0.1.0"),
            {"version": "0.1.0", "tag": "v0.1.0", "prerelease": "false"},
        )
        self.assertEqual(release_version.metadata("0.1.0-rc.1")["prerelease"], "true")
        with self.assertRaisesRegex(ValueError, "does not match"):
            release_version.metadata("0.1.0", "v0.2.0")
        with self.assertRaises(ValueError):
            release_version.metadata("0.1.0", "0.1.0")

    def test_cli_writes_github_output(self):
        root = Path(__file__).resolve().parents[1]
        version = package.version()
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "github-output"
            completed = subprocess.run(
                ["python3", "distribution/release_version.py", "--tag", f"v{version}", "--output", str(output)],
                cwd=root,
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(json.loads(completed.stdout)["tag"], f"v{version}")
            self.assertIn(f"version={version}\n", output.read_text())
            prerelease = str("-" in version.split("+", 1)[0]).lower()
            self.assertIn(f"prerelease={prerelease}\n", output.read_text())


if __name__ == "__main__":
    unittest.main()
