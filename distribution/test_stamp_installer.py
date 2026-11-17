"""Tests for setting the installer to its release."""

import unittest

import stamp_installer


class StampInstaller(unittest.TestCase):
    def test_the_installer_is_set_to_exactly_its_release(self):
        stamped = stamp_installer.stamp(stamp_installer.SOURCE.read_text(), "0.2.1")
        self.assertIn("\nrelease=0.2.1\n", stamped)
        self.assertNotIn("\nrelease=latest\n", stamped)

    def test_only_a_release_version_is_written(self):
        with self.assertRaises(ValueError):
            stamp_installer.stamp(stamp_installer.SOURCE.read_text(), "0.2.1; rm -rf ~")

    def test_an_installer_without_its_release_line_is_refused(self):
        with self.assertRaises(ValueError):
            stamp_installer.stamp("#!/bin/sh\n", "0.2.1")


if __name__ == "__main__":
    unittest.main()
