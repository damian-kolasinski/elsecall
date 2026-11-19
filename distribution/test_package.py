"""Tests for the release archive."""

import gzip
import io
import stat
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import archive_contents
import package


def executable(directory, version, body=b""):
    path = Path(directory) / "elsecall"
    path.write_bytes(b"#!/bin/sh\n" + body + f"echo elsecall {version}\n".encode())
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    companion = Path(directory) / "elsecall-ui"
    companion.write_bytes(b"window-bytes")
    companion.chmod(0o755)
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
        self.assertIsNotNone(package.SEMVER.fullmatch(package.version()))

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
            self.assertEqual(set(files), {"elsecall", "elsecall-ui", "README.txt"})
            self.assertTrue(files["elsecall"].startswith(b"#!/bin/sh\n"))
            self.assertNotIn(str(Path.home()).encode(), first.read_bytes())

    def test_archive_rejects_a_private_build_path(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = executable(directory, "0.1.0", f"# {Path.home()}\n".encode())
            with self.assertRaisesRegex(ValueError, "private build path"):
                package.archive(binary, directory, "0.1.0")

    def test_archive_requires_the_companion(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = executable(directory, "0.1.0")
            binary.with_name("elsecall-ui").unlink()
            with self.assertRaisesRegex(ValueError, "UI executable unavailable"):
                package.archive(binary, directory, "0.1.0")

    def test_archive_rejects_a_private_ui_build_path(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = executable(directory, "0.1.0")
            binary.with_name("elsecall-ui").write_bytes(str(Path.home()).encode())
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

    def test_archive_refuses_a_local_channel_build_unless_asked_for_one(self):
        with tempfile.TemporaryDirectory() as directory:
            local = executable(directory, "0.1.0", b"# " + package.LOCAL_CHANNEL_MARKER + b"\n")
            with self.assertRaisesRegex(ValueError, "follows the local release channel"):
                package.archive(local, Path(directory) / "published", "0.1.0")
            self.assertTrue(package.archive(local, Path(directory) / "local", "0.1.0", local_channel=True).is_file())
        with tempfile.TemporaryDirectory() as directory:
            published = executable(directory, "0.1.0")
            with self.assertRaisesRegex(ValueError, "needs a local-release-channel build"):
                package.archive(published, directory, "0.1.0", local_channel=True)

    def test_release_builds_never_enable_the_local_channel(self):
        commands = []

        def record(command, **_):
            commands.append(command)

        with (
            mock.patch.object(package.subprocess, "check_output", return_value="host: aarch64-apple-darwin\n"),
            mock.patch.object(package, "cargo", return_value="cargo"),
            mock.patch.object(package.subprocess, "run", side_effect=record),
            mock.patch.object(package.sys, "argv", ["package.py", "--build"]),
            mock.patch("builtins.print"),
            mock.patch.object(package, "archive"),
        ):
            package.main()
        self.assertEqual(len(commands), 2)
        self.assertFalse(any("--features" in command for command in commands), commands)
        release = Path(package.ROOT, ".github/workflows/release.yml").read_text()
        self.assertNotIn("local-release-channel", release)
        self.assertNotIn("--features", release)
        self.assertNotIn("--all-features", release)

    def test_only_the_caller_makes_a_local_channel_build(self):
        calls = []
        with (
            mock.patch.object(package.subprocess, "check_output", return_value="host: aarch64-apple-darwin\n"),
            mock.patch.object(package, "cargo", return_value="cargo"),
            mock.patch.object(package.subprocess, "run", side_effect=lambda command, **kw: calls.append(kw["env"])),
            mock.patch.dict(package.os.environ, {"ELSECALL_LOCAL_RELEASE_FAULT": "serve"}),
        ):
            package.build(features=("local-release-channel",), env={"ELSECALL_LOCAL_RELEASE_CHANNEL": "http://x"})
            package.build()
        self.assertTrue(all("ELSECALL_LOCAL_RELEASE_FAULT" not in env for env in calls))
        self.assertEqual(calls[0]["ELSECALL_LOCAL_RELEASE_CHANNEL"], "http://x")
        self.assertTrue(all("ELSECALL_LOCAL_RELEASE_CHANNEL" not in env for env in calls[1:]))

    def test_builds_run_the_toolchains_cargo_from_any_directory(self):
        commands = []
        with (
            mock.patch.object(package.subprocess, "check_output", return_value="host: aarch64-apple-darwin\n"),
            mock.patch.object(package, "cargo", return_value="/toolchain/bin/cargo"),
            mock.patch.object(package.subprocess, "run", side_effect=lambda command, **_: commands.append(command)),
        ):
            package.build(root="/elsewhere/copy")
        self.assertEqual([command[0] for command in commands], ["/toolchain/bin/cargo"] * 2)

    def test_cargo_is_resolved_from_the_repository_and_falls_back_to_plain_cargo(self):
        found = mock.Mock(returncode=0, stdout="/toolchain/bin/cargo\n")
        with mock.patch.object(package.subprocess, "run", return_value=found) as run:
            self.assertEqual(package.cargo(), "/toolchain/bin/cargo")
        self.assertEqual(run.call_args.args[0], ["mise", "which", "cargo"])
        self.assertEqual(run.call_args.kwargs["cwd"], package.ROOT)
        with mock.patch.object(package.subprocess, "run", return_value=mock.Mock(returncode=1, stdout="")):
            self.assertEqual(package.cargo(), "cargo")
        with mock.patch.object(package.subprocess, "run", side_effect=FileNotFoundError):
            self.assertEqual(package.cargo(), "cargo")

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
