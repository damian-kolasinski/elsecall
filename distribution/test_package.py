"""Tests for the release archive."""

import gzip
import io
import stat
import tarfile
import tempfile
import unittest
from pathlib import Path

import archive_contents
import package


def executable(directory, version, body=b""):
    path = Path(directory) / "elsecall"
    path.write_bytes(b"#!/bin/sh\n" + body + f"echo elsecall {version}\n".encode())
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return path


class Package(unittest.TestCase):
    def test_filename_uses_the_native_target(self):
        self.assertEqual(
            package.filename("0.1.0"),
            "elsecall-v0.1.0-aarch64-apple-darwin.tar.gz",
        )

    def test_filename_rejects_another_target_or_version(self):
        with self.assertRaises(ValueError):
            package.filename("0.1.0", "x86_64-unknown-linux-gnu")
        with self.assertRaises(ValueError):
            package.filename("1.2")

    def test_cargo_version_is_a_release(self):
        self.assertEqual(package.version(), "0.1.0")

    def test_release_flags_remap_local_paths(self):
        root = Path("/work/elsecall")
        env = package.release_env(
            root,
            {
                "HOME": "/Users/me",
                "CARGO_HOME": "/Users/me/.cargo",
                "RUSTUP_HOME": "/Users/me/.rustup",
                "RUSTFLAGS": "--cfg leaked",
                "PATH": "/usr/bin",
            },
        )
        flags = env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
        home = "/Users/me"
        self.assertEqual(
            flags,
            [
                f"--remap-path-prefix={home}=/build/home",
                "--remap-path-prefix=/work/elsecall=/build/elsecall",
                f"--remap-path-prefix={home}/.cargo=/build/cargo",
                f"--remap-path-prefix={home}/.rustup=/build/rustup",
            ],
        )
        self.assertNotIn("RUSTFLAGS", env)
        self.assertEqual(env["MACOSX_DEPLOYMENT_TARGET"], "13.0")
        self.assertEqual(env["PATH"], "/usr/bin")

    def test_archive_is_deterministic_and_contains_the_executable(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = executable(directory, "0.1.0")
            first = package.archive(binary, Path(directory) / "a", "0.1.0")
            second = package.archive(binary, Path(directory) / "b", "0.1.0")
            self.assertEqual(first.read_bytes(), second.read_bytes())
            files = archive_contents.read(first)
            self.assertEqual(set(files), {"elsecall", "README.txt"})
            self.assertTrue(files["elsecall"].startswith(b"#!/bin/sh\n"))
            self.assertNotIn(str(Path.home()).encode(), first.read_bytes())

    def test_archive_rejects_a_private_build_path(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = executable(directory, "0.1.0", f"# {Path.home()}\n".encode())
            with self.assertRaisesRegex(ValueError, "private build path"):
                package.archive(binary, directory, "0.1.0")

    def test_archive_rejects_a_different_version(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = executable(directory, "0.9.0")
            with self.assertRaisesRegex(ValueError, "version mismatch"):
                package.archive(binary, directory, "0.1.0")

    def test_archive_rejects_a_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = executable(directory, "0.1.0")
            link = Path(directory) / "linked"
            link.symlink_to(binary)
            with self.assertRaisesRegex(ValueError, "unavailable"):
                package.archive(link, directory, "0.1.0")

    def test_archive_reader_rejects_a_path(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = executable(directory, "0.1.0")
            packed = package.archive(binary, directory, "0.1.0")
            rewritten = Path(directory) / "nested.tar.gz"
            with (
                rewritten.open("wb") as raw,
                gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0) as compressed,
            ):
                with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as tar:
                    data = b"elsecall\n"
                    item = tarfile.TarInfo("dir/elsecall")
                    item.size = len(data)
                    tar.addfile(item, io.BytesIO(data))
            with self.assertRaisesRegex(ValueError, "not a single file"):
                archive_contents.read(rewritten)
            self.assertEqual(archive_contents.read(packed)["elsecall"], binary.read_bytes())


if __name__ == "__main__":
    unittest.main()
