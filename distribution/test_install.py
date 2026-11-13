"""Tests for the published-release install check."""

import unittest

import install


class Install(unittest.TestCase):
    def test_mise_configuration_pins_the_signing_workflow(self):
        text = install.mise_configuration(
            "github.com/example/elsecall",
            "0.1.0",
            install.identity_for("example/elsecall", "0.1.0"),
        )
        self.assertIn('"packslip:github.com/example/elsecall"', text)
        self.assertIn('"0.1.0"', text)
        self.assertIn(
            "https://github.com/example/elsecall/.github/workflows/release.yml@refs/tags/v0.1.0",
            text,
        )
        self.assertIn(install.ISSUER, text)

    def test_statement_must_name_the_same_release(self):
        statement = {
            "predicate": {
                "project": "github.com/example/elsecall",
                "version": "0.1.0",
                "source": {"commit": "a" * 40},
            }
        }
        install.validate_statement(statement, "github.com/example/elsecall", "0.1.0", "a" * 40)
        with self.assertRaises(RuntimeError):
            install.validate_statement(statement, "github.com/example/elsecall", "0.1.0", "b" * 40)

    def test_identity_rejects_a_malformed_repository(self):
        with self.assertRaises(ValueError):
            install.verify("example/elsecall;touch pwned", "0.1.0", "a" * 40)
        with self.assertRaises(ValueError):
            install.verify("example/elsecall", "0.1.0", "not-a-commit")
        with self.assertRaises(ValueError):
            install.verify("example/elsecall", "1.2", "a" * 40)


if __name__ == "__main__":
    unittest.main()
