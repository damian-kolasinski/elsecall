"""Tests for the local release channel builder's pure parts."""

import argparse
import json
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

    def test_version_is_set_in_the_manifest_and_lock_only_for_the_workspace(self):
        manifest = (local_release.ROOT / "Cargo.toml").read_text()
        lock = (local_release.ROOT / "Cargo.lock").read_text()
        packages = 1 + len(list((local_release.ROOT / "crates").glob("*/Cargo.toml")))
        new_manifest, new_lock = local_release.with_version(manifest, lock, "9.8.7", packages)
        self.assertEqual(tomllib.loads(new_manifest)["workspace"]["package"]["version"], "9.8.7")
        with self.assertRaises(ValueError):
            local_release.with_version(manifest, lock, "9.8.7", packages + 1)
        members = {"elsecall", "elsecall-ui"} | {
            tomllib.loads(path.read_text())["package"]["name"]
            for path in (local_release.ROOT / "crates").glob("*/Cargo.toml")
        }
        packages = {item["name"]: item["version"] for item in tomllib.loads(new_lock)["package"]}
        self.assertEqual({name: packages[name] for name in members}, {name: "9.8.7" for name in members})
        old = {item["name"]: item["version"] for item in tomllib.loads(lock)["package"]}
        self.assertEqual({k: v for k, v in packages.items() if k not in members},
                         {k: v for k, v in old.items() if k not in members})

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
        channel = (local_release.ROOT / "crates/elsecall-lifecycle/src/release_update/local_channel/mod.rs").read_text()
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


    def test_the_signing_key_is_kept_outside_the_build_tree(self):
        self.assertFalse(local_release.KEY.is_relative_to(local_release.ROOT))
        self.assertTrue(local_release.KEY.is_relative_to(Path.home()))

    def test_a_missing_signing_key_is_created_once_and_then_kept(self):
        packslip = local_release.tool("packslip")
        with tempfile.TemporaryDirectory() as directory:
            key = Path(directory, "settings", "local-release", "release.key")
            earlier = Path(directory, "out", "key", "release.key")
            self.assertEqual(local_release.signing_key(key, earlier, packslip), key)
            public = key.with_suffix(".pub").read_text()
            self.assertEqual(key.parent.stat().st_mode & 0o777, 0o700)
            local_release.signing_key(key, earlier, packslip)
            self.assertEqual(key.with_suffix(".pub").read_text(), public)
            self.assertFalse(earlier.exists())

    def test_a_key_an_earlier_build_kept_in_its_output_moves_to_the_settings(self):
        packslip = local_release.tool("packslip")
        with tempfile.TemporaryDirectory() as directory:
            earlier = Path(directory, "out", "key", "release.key")
            earlier.parent.mkdir(parents=True)
            subprocess.run([packslip, "keygen", "--out", str(earlier)], check=True, capture_output=True)
            private, public = earlier.read_bytes(), earlier.with_suffix(".pub").read_bytes()
            key = Path(directory, "settings", "local-release", "release.key")
            local_release.signing_key(key, earlier, packslip)
            self.assertEqual((key.read_bytes(), key.with_suffix(".pub").read_bytes()), (private, public))
            self.assertFalse(earlier.exists())
            self.assertFalse(earlier.with_suffix(".pub").exists())

    def test_half_of_an_earlier_key_is_refused_rather_than_moved(self):
        with tempfile.TemporaryDirectory() as directory:
            earlier = Path(directory, "out", "key", "release.key")
            earlier.parent.mkdir(parents=True)
            earlier.write_text("private")
            key = Path(directory, "settings", "release.key")
            with self.assertRaisesRegex(SystemExit, "half of a signing key"):
                local_release.signing_key(key, earlier, "packslip-is-not-run")
            self.assertTrue(earlier.exists())
            self.assertFalse(key.exists())

    def test_a_key_inside_the_repository_is_refused(self):
        root = local_release.ROOT
        target = (root / "target").resolve()
        for inside in (root / "release.key", root / "target" / "local-release" / "release.key",
                       target / "local-release" / "release.key"):
            with self.assertRaises(SystemExit, msg=inside):
                local_release.key_outside_the_tree(inside)
        self.assertEqual(local_release.key_outside_the_tree(local_release.KEY), local_release.KEY)

    def test_a_channel_keeps_its_url_and_signing_key(self):
        packslip = local_release.tool("packslip")
        url = "http://192.168.64.1:8765"
        with tempfile.TemporaryDirectory() as directory:
            keys = Path(directory, "keys")
            keys.mkdir()
            for name in ("one", "two"):
                subprocess.run([packslip, "keygen", "--out", str(keys / f"{name}.key")], check=True, capture_output=True)
            one, two = keys / "one.key", keys / "two.key"
            out = Path(directory, "out")
            out.mkdir()
            gone = out / "key" / "release.key"
            settings = local_release.channel_settings(out, url, one, gone)
            self.assertEqual(settings, {"url": url, "key": local_release.public_key(one)})
            (out / "channel.json").write_text(json.dumps(settings))
            self.assertEqual(local_release.channel_settings(out, url, one, gone), settings)
            with self.assertRaisesRegex(SystemExit, "holds builds for"):
                local_release.channel_settings(out, "http://192.168.64.1:9999", one, gone)
            with self.assertRaisesRegex(SystemExit, "signed with another key"):
                local_release.channel_settings(out, url, two, gone)

    def test_a_channel_recorded_without_its_key_is_checked_against_the_key_it_kept(self):
        packslip = local_release.tool("packslip")
        url = "http://192.168.64.1:8765"
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory, "out")
            earlier = out / "key" / "release.key"
            earlier.parent.mkdir(parents=True)
            shared = Path(directory, "shared.key")
            for key in (earlier, shared):
                subprocess.run([packslip, "keygen", "--out", str(key)], check=True, capture_output=True)
            (out / "channel.json").write_text(json.dumps({"url": url}))
            with self.assertRaisesRegex(SystemExit, "signed with another key"):
                local_release.channel_settings(out, url, shared, earlier)
            self.assertEqual(local_release.channel_settings(out, url, earlier, earlier)["key"],
                             local_release.public_key(earlier))

    def test_output_stays_out_of_the_mirrored_tree(self):
        root = local_release.ROOT
        self.assertEqual(local_release.outside_the_tree(root / "target/local-release"), root / "target/local-release")
        with self.assertRaises(SystemExit):
            local_release.outside_the_tree(root / "local-release")


if __name__ == "__main__":
    unittest.main()
