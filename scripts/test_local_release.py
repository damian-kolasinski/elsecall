"""Tests for the local release channel builder's pure parts."""

import argparse
import subprocess
import tempfile
import tomllib
import unittest
from pathlib import Path

import local_release


class LocalRelease(unittest.TestCase):
    def test_default_versions_are_cargo_s_and_the_next_patch(self):
        self.assertEqual(local_release.next_patch("0.2.1"), "0.2.2")
        self.assertEqual(local_release.next_patch("1.9.9"), "1.9.10")
        for value in ("1.2", "1.2.3-rc.1", "01.2.3", "1.2.3+build"):
            with self.assertRaises(argparse.ArgumentTypeError):
                local_release.stable(value)

    def test_channel_urls_are_http_https_or_absolute_files(self):
        self.assertEqual(local_release.channel_url("http://192.168.64.1:8765/"), "http://192.168.64.1:8765")
        self.assertEqual(
            local_release.channel_url("file:///Users/devbox/channel", home="/Users/me"), "file:///Users/devbox/channel"
        )
        for value in ("ftp://example.com", "/Users/devbox/channel", "file://host/channel", "http://",
                      "http://example.com/$(id)", "http://example.com/a b", "file:///Users/me/channel"):
            with self.assertRaises(argparse.ArgumentTypeError, msg=value):
                local_release.channel_url(value, home="/Users/me")

    def test_version_is_set_in_the_manifest_and_lock_only_for_elsecall(self):
        manifest = (local_release.ROOT / "Cargo.toml").read_text()
        lock = (local_release.ROOT / "Cargo.lock").read_text()
        new_manifest, new_lock = local_release.with_version(manifest, lock, "9.8.7")
        self.assertEqual(tomllib.loads(new_manifest)["package"]["version"], "9.8.7")
        packages = {item["name"]: item["version"] for item in tomllib.loads(new_lock)["package"]}
        self.assertEqual(packages["elsecall"], "9.8.7")
        old = {item["name"]: item["version"] for item in tomllib.loads(lock)["package"]}
        self.assertEqual({k: v for k, v in packages.items() if k != "elsecall"},
                         {k: v for k, v in old.items() if k != "elsecall"})

    def test_listing_is_the_release_list_the_updater_reads(self):
        self.assertEqual(local_release.listing(["0.2.10", "0.2.9"]), ["0.2.9", "0.2.10"])

    def test_installer_is_set_to_the_channel_version_and_key(self):
        template = (local_release.ROOT / "scripts" / "local-release-install.sh").read_text()
        key = "RWQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        text = local_release.installer(template, "http://192.168.64.1:8765", "0.2.1", key)
        self.assertIn('channel="http://192.168.64.1:8765"\n', text)
        self.assertIn('release="0.2.1"\n', text)
        self.assertIn(f'key="{key}"\n', text)
        for placeholder in ("@CHANNEL@", "@VERSION@", "@KEY@"):
            self.assertNotIn(placeholder, text)
        subprocess.run(["/bin/sh", "-n"], input=text, text=True, check=True)
        with self.assertRaises(ValueError):
            local_release.installer(template, "http://x/$(id)", "0.2.1", key)

    def test_installer_pins_the_packslip_the_service_verifies_with(self):
        template = (local_release.ROOT / "scripts" / "local-release-install.sh").read_text()
        self.assertIn(f'packslip="{local_release.PACKSLIP}"', template)
        channel = (local_release.ROOT / "src/release_update/local_channel/mod.rs").read_text()
        self.assertIn(f'pub const PACKSLIP: &str = "{local_release.PACKSLIP}";', channel)
        self.assertIn(f'pub const PROJECT: &str = "{local_release.PROJECT}";', channel)

    def test_mirror_copies_the_working_tree_and_drops_removed_files(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory, "source")
            source.mkdir()
            subprocess.run(["git", "init", "-q"], cwd=source, check=True)
            Path(source, ".gitignore").write_text("ignored\n")
            Path(source, "kept").write_text("one")
            Path(source, "ignored").write_text("secret")
            subprocess.run(["git", "add", "kept", ".gitignore"], cwd=source, check=True)
            Path(source, "new").write_text("untracked")
            Path(source, "removed").write_text("tracked, then deleted")
            subprocess.run(["git", "add", "removed"], cwd=source, check=True)
            Path(source, "removed").unlink()
            copy = Path(directory, "copy")
            Path(copy, "target").mkdir(parents=True)
            Path(copy, "target", "build").write_text("cache")
            Path(copy, "stale").write_text("gone")
            Path(copy, "removed").write_text("an old copy")
            local_release.mirror(source, copy)
            self.assertEqual(Path(copy, "kept").read_text(), "one")
            self.assertEqual(Path(copy, "new").read_text(), "untracked")
            self.assertFalse(Path(copy, "ignored").exists())
            self.assertFalse(Path(copy, "stale").exists())
            self.assertFalse(Path(copy, "removed").exists())
            self.assertTrue(Path(copy, "target", "build").exists())


    def test_output_stays_out_of_the_mirrored_tree(self):
        root = local_release.ROOT
        self.assertEqual(local_release.outside_the_tree(root / "target/local-release"), root / "target/local-release")
        with self.assertRaises(SystemExit):
            local_release.outside_the_tree(root / "local-release")


if __name__ == "__main__":
    unittest.main()
