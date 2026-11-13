"""Ask packslip to describe a fixture archive when the CLI is installed."""

import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

import package


def packslip():
    configured = os.environ.get("PACKSLIP")
    if configured:
        return configured
    found = shutil.which("packslip")
    if not found:
        return None
    probe = subprocess.run([found, "version"], capture_output=True, text=True, check=False)
    if probe.returncode != 0:
        return None
    return found


class PackslipArchive(unittest.TestCase):
    def test_create_infers_apple_silicon(self):
        tool = packslip()
        if tool is None:
            self.skipTest("packslip is not installed")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "elsecall"
            binary.write_bytes(b"#!/bin/sh\necho elsecall 0.1.0\n")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            archive = package.archive(binary, root / "dist", "0.1.0")
            subprocess.run([tool, "keygen", "--out", str(root / "release.key")], check=True)
            subprocess.run(
                [
                    tool,
                    "create",
                    "--project",
                    "github.com/example/elsecall",
                    "--version",
                    "0.1.0",
                    "--url-base",
                    "https://github.com/example/elsecall/releases/download/v0.1.0",
                    "--bin",
                    "elsecall",
                    "--key",
                    str(root / "release.key"),
                    "--no-log",
                    "--out",
                    "bundle",
                    archive.relative_to(root).as_posix(),
                ],
                check=True,
                cwd=root,
            )
            shown = subprocess.check_output(
                [tool, "show", str(root / "bundle" / "packslip.sigstore.json")],
                text=True,
            )
            self.assertIn("aarch64", shown)
            self.assertIn("darwin", shown)
            self.assertIn("elsecall", shown)
            self.assertNotIn(str(Path.home()), shown)


if __name__ == "__main__":
    unittest.main()
