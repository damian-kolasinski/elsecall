"""Tests that the release workflow publishes only what Check tested and the build installed."""

import re
import unittest
from pathlib import Path

WORKFLOW = Path(__file__).resolve().parents[1] / ".github" / "workflows" / "release.yml"


def jobs(text):
    """Map each job to its own lines, using the workflow's two-space job indentation."""
    body = text.split("\njobs:\n", 1)[1]
    result = {}
    name = None
    for line in body.splitlines():
        header = re.fullmatch(r"  ([A-Za-z0-9_-]+):", line)
        if header:
            name = header.group(1)
            result[name] = []
        elif name and not re.match(r"\S", line):
            result[name].append(line)
    return result


def needs(lines):
    for line in lines:
        match = re.fullmatch(r"    needs: (?:\[(.*)\]|(\S+))", line)
        if match:
            listed = match.group(1) if match.group(1) is not None else match.group(2)
            return {item.strip() for item in listed.split(",")}
    return set()


class ReleaseWorkflow(unittest.TestCase):
    def setUp(self):
        self.jobs = jobs(WORKFLOW.read_text(encoding="utf-8"))

    def test_nothing_is_signed_or_published_before_check_passed_and_the_build_installed(self):
        self.assertLessEqual({"tested", "build"}, needs(self.jobs["sign"]))
        self.assertLessEqual({"tested", "sign"}, needs(self.jobs["publish"]))
        self.assertIn("publish", needs(self.jobs["verify-published"]))

    def test_the_gate_waits_for_check_on_the_released_commit(self):
        gate = "\n".join(self.jobs["tested"])
        self.assertIn("if: github.ref_type == 'tag'", gate)
        self.assertIn('distribution/checked_commit.py --repository "$GITHUB_REPOSITORY" --commit "$GITHUB_SHA"', gate)

    def test_the_build_smoke_tests_and_installs_the_archive(self):
        build = "\n".join(self.jobs["build"])
        smoke = build.index("distribution/smoke.py")
        install = build.index("distribution/desktop_install.py")
        upload = build.index("actions/upload-artifact")
        self.assertLess(smoke, upload)
        self.assertLess(install, upload)
        self.assertNotIn("if:", build.split("steps:", 1)[0])


if __name__ == "__main__":
    unittest.main()
