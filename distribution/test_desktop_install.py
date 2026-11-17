"""Tests for the CI desktop installation check."""

import json
import unittest

import desktop_install


def result(window):
    return json.dumps({"schema_version": 1, "data": {"session": "desktop", "window": window}})


class DesktopInstall(unittest.TestCase):
    def test_it_refuses_to_replace_an_installation_outside_ci(self):
        with self.assertRaises(SystemExit):
            desktop_install.require_ci({})
        desktop_install.require_ci({"GITHUB_ACTIONS": "true"})

    def test_an_opened_window_passes_after_installer_output(self):
        window = {"state": "opened", "app": "/Users/me/Applications/Elsecall.app"}
        output = "elsecall: Selecting a release\n" + result(window) + "\n"
        self.assertEqual(desktop_install.opened_window(output), window)

    def test_any_other_window_state_fails(self):
        for window in (
            {"state": "background_session"},
            {"state": "placed", "app": "/Users/me/Applications/Elsecall.app"},
            {"state": "failed", "error": {"code": "window_open_failed"}},
        ):
            with self.subTest(window=window["state"]), self.assertRaises(RuntimeError):
                desktop_install.opened_window(result(window))


if __name__ == "__main__":
    unittest.main()
