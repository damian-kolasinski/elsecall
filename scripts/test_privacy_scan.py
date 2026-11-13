"""Tests for privacy_scan.py.

Samples that must be caught are assembled at run time, so this file itself
passes the scan it tests.
"""
from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import privacy_scan

SCRIPT = Path(__file__).with_name("privacy_scan.py")

JANE = "ja" + "ne"
REAL_HOME = "/Users/" + JANE
REAL_V4 = "100.93" + ".7.21"
REAL_V6 = "fd7a:115c:a1e0" + "::4a1:9c2e"
REAL_V6_FULL = "fd7a:115c:a1e0" + ":ab12:4843:cd96:6258:b240"
REAL_TAILNET = "laptop.tail1a2b3c" + ".ts" + ".net"
REAL_EMAIL = JANE + "@corp.io"


def rules(text: str) -> list[str]:
    return [finding.rule for finding in privacy_scan.scan_text("sample", text)]


class PublicRules(unittest.TestCase):
    def assertCaught(self, rule: str, samples: list[str]) -> None:
        for text in samples:
            with self.subTest(text=text):
                self.assertEqual(rules(text), [rule])

    def test_home_paths(self):
        self.assertCaught("home-path", [
            f"cd {REAL_HOME}/src/app",
            f"/home/{JANE}/.config",
            f"in ~{JANE}/src",
            f'"root": "\\/Users\\/{JANE}\\/app"',
            f"C:\\Users\\{JANE}\\AppData",
        ])

    def test_owner_named_machines(self):
        self.assertCaught("owner-machine", [
            f"ssh {JANE.title()}s-MacBook-Pro.local",
            f"ComputerName: {JANE.title()}'s Mac mini",
            f"node {JANE}s-imac",
        ])

    def test_tailnet_addresses(self):
        self.assertCaught("tailnet-address", [
            f"listening on {REAL_V4}:47821",
            f"tcp4  0  0  {REAL_V4}.47821  *.*  LISTEN",
            f"host ip-{REAL_V4.replace('.', '-')}",
            f"and [{REAL_V6}]:47821",
            f"bound {REAL_V6}:47821 too",
            f"bound {REAL_V6_FULL}:47821 too",
            f"full form {REAL_V6_FULL}.",
            f"ends a clause: {REAL_V6}:",
        ])

    def test_tailnet_names(self):
        self.assertCaught("tailnet-name", [
            f"DNSName {REAL_TAILNET}.",
            "old name laptop.corp-net" + ".beta.tailscale" + ".net",
        ])

    def test_emails(self):
        self.assertCaught("email", [f"mail {REAL_EMAIL} today", "clone git" + "@git.corp.io:app.git"])

    def test_credentials(self):
        self.assertCaught("credential", [
            "gh" + "p_" + "A1b2C3d4" * 4 + "E5f6",
            "github" + "_pat_" + "A1b2C3d4" * 3,
            "glpat" + "-" + "A1b2C3d4" * 3,
            "tskey" + "-auth-" + "kA1b2C3d4E5f6",
            "sk" + "-ant-api03-" + "A1b2C3d4" * 3,
            "sk" + "-proj-" + "A1b2C3d4" * 3,
            "AI" + "za" + "A1b2C3d4" * 4 + "E5f",
            "AK" + "IA" + "ABCDEFGH23456789",
            "xox" + "b-" + "1234567890-abcdef",
            "xa" + "pp-" + "1-A0123456789-abc",
            "https://hooks.slack" + ".com/services/T000/B000/XXXX",
            "-----BEGIN OPENSSH " + "PRIVATE KEY-----",
            "-----BEGIN PGP " + "PRIVATE KEY BLOCK-----",
        ])

    def test_allows_documented_placeholders_and_public_constants(self):
        for text in [
            "/Users/me/src/app and /Users/devbox/.elsecall and ~/.config/acme-ios",
            "/Users/a b/Library and /Users/x and /home/user and /home/runner/work",
            "/Users/<name>/src, /Users/$USER, /Users/{account}, /Users/.../x, /Users/Shared",
            "C:\\Users\\username and ~me/src and ~user/src",
            "ssh devbox@macbook-pro.local and mac-mini.local; the MacBook and the Mac mini",
            "100.64.0.1, 100.64.0.20 and 100.127.255.254; resolver 100.100.100.100",
            "outside the tailnet: 100.63.255.255, 100.128.0.0, 192.168.1.10, 10.0.0.1, 1.2.3.4.5",
            "fd7a:115c:a1e0::a, [fd7a:115c:a1e0::b]:47821, fd7a:115c:a1e0::/48, fd7a:115c:a1e1::1",
            "Mac-Mini.example.ts.net. and a .ts.net name",
            "test@example.com, ci@mail.example.org, git@github.com:org/repo",
            "81234058+someone@users.noreply.github.com and noreply@github.com",
            "react@18.3.1, serde@1.0, sk-short, a commit 3f1c2a9e8b7d6c5f4e3a2b1c0d9e8f7a6b5c4d3e",
        ]:
            with self.subTest(text=text):
                self.assertEqual(rules(text), [])

    def test_reports_line_column_and_text(self):
        [finding] = privacy_scan.scan_text("notes.md", f"first\n  see {REAL_V4}\n")
        self.assertEqual((finding.source, finding.line, finding.column), ("notes.md", 2, 7))
        self.assertEqual(finding.text, REAL_V4)
        self.assertEqual(finding.render(reveal=False), "notes.md:2:7: tailnet-address")
        self.assertEqual(finding.render(reveal=True), f"notes.md:2:7: tailnet-address: {REAL_V4}")


class PrivateTerms(unittest.TestCase):
    def test_terms_come_from_the_variable_then_the_file_numbered_by_line(self):
        with tempfile.TemporaryDirectory() as directory:
            listed = Path(directory) / "terms"
            listed.write_text("# comment\n\n  Contoso  \nFabrikam\n")
            self.assertEqual(
                privacy_scan.load_terms({"PRIVACY_SCAN_TERMS": "Northwind", "PRIVACY_SCAN_TERMS_FILE": str(listed)}),
                privacy_scan.Terms(((1, "Northwind"),), "$PRIVACY_SCAN_TERMS"),
            )
            self.assertEqual(
                privacy_scan.load_terms({"PRIVACY_SCAN_TERMS": " \n", "PRIVACY_SCAN_TERMS_FILE": str(listed)}),
                privacy_scan.Terms(((3, "Contoso"), (4, "Fabrikam")), str(listed)),
            )
            missing = str(Path(directory) / "missing")
            self.assertEqual(
                privacy_scan.load_terms({"PRIVACY_SCAN_TERMS_FILE": missing}), privacy_scan.Terms((), missing)
            )
            with self.assertRaises(privacy_scan.TermsError):
                privacy_scan.load_terms({"PRIVACY_SCAN_TERMS_FILE": directory})

    def test_terms_match_any_case_accent_or_separator(self):
        terms = privacy_scan.Terms(((1, "Contoso Ltd"), (5, "a.b")), "test")
        text = "contoso-ltd, CONTOSOLTD, Cóntoso_Ltd and contoso.ltd; axb, a.b, ab"
        findings = privacy_scan.scan_text("s", text, terms.rules())
        self.assertEqual(
            [(finding.rule, finding.text) for finding in findings],
            [
                ("private-term 1", "contoso-ltd"), ("private-term 1", "CONTOSOLTD"),
                ("private-term 1", "Cóntoso_Ltd"), ("private-term 1", "contoso.ltd"),
                ("private-term 5", "a.b"), ("private-term 5", "ab"),
            ],
        )


    def test_terms_survive_unusual_letters_doubled_separators_and_invisible_characters(self):
        terms = privacy_scan.Terms(((1, "Lodz Ltd"),), "test")
        text = "\u0141\u00f3d\u017a Ltd, lodz--ltd, lo\u200bdz-ltd, lod\u00adzltd"
        self.assertEqual(len(privacy_scan.scan_text("s", text, terms.rules())), 4)
        with self.assertRaises(privacy_scan.TermsError):
            privacy_scan.Terms(((1, " -./_"),), "test").rules()


class Diffs(unittest.TestCase):
    def test_quoted_paths_are_unquoted(self):
        self.assertEqual(privacy_scan.unquote('"caf\\303\\251 \\"q\\".md"'), 'caf\u00e9 "q".md')
        self.assertEqual(privacy_scan.raw_path(":000000 100644 0000000 e69de29 A\t\"a\\tb.md\""), "a\tb.md")
        self.assertEqual(privacy_scan.unquote("plain.md"), "plain.md")

    def test_added_lines_follow_hunk_sizes(self):
        patch = [
            "--- a/docs/x.md", "+++ b/docs/x.md", "@@ -3 +3,2 @@", "-old", "+new", "++++ b/looks like a header",
            "@@ -10,0 +12 @@", "+later", "\\ No newline at end of file",
            "--- a/with space.md\t", "+++ b/with space.md\t", "@@ -0,0 +1 @@", "+spaced",
            "--- a/gone.md", "+++ /dev/null", "@@ -1 +0,0 @@", "-bye",
        ]
        self.assertEqual(
            list(privacy_scan.added_lines(patch)),
            [
                ("docs/x.md", 3, "new"), ("docs/x.md", 4, "+++ b/looks like a header"),
                ("docs/x.md", 12, "later"), ("with space.md", 1, "spaced"),
            ],
        )


class CommandLine(unittest.TestCase):
    """Runs the scanner, its hooks and git in a throwaway repository, isolated
    from the operator's git configuration, hooks and private terms."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        root = Path(self.directory.name)
        self.repo, self.home, self.bin = root / "repo", root / "home", root / "bin"
        for directory in (self.repo, self.home, self.bin):
            directory.mkdir()
        (self.bin / "python3").symlink_to(sys.executable)  # the hooks run this interpreter
        self.env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("PRIVACY_SCAN_", "GIT_", "XDG_CONFIG_HOME"))
        }
        self.env.update(
            HOME=str(self.home), GIT_CONFIG_NOSYSTEM="1", PYTHONDONTWRITEBYTECODE="1",
            PATH=f"{self.bin}{os.pathsep}{os.environ['PATH']}",
        )
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Dev")
        self.git("config", "user.email", "dev@example.com")
        self.git("config", "commit.gpgsign", "false")

    def tearDown(self):
        self.directory.cleanup()

    def git(self, *args: str, check: bool = True, cwd: Path | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["git", *args], cwd=cwd or self.repo, check=check, capture_output=True, text=True, env=self.env
        )

    def scan(self, *args: str, stdin: str = "", cwd: Path | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(SCRIPT), *args],
            cwd=cwd or self.repo,
            input=stdin,
            capture_output=True,
            text=True,
            env=self.env,
        )

    def write_terms(self, *terms: str) -> None:
        """The terms land on lines 3 onward of the list."""
        path = self.home / ".config/elsecall/private-terms"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("# what must never appear\n\n" + "".join(f"{term}\n" for term in terms))

    def commit_all(self, message: str, *extra: str) -> str:
        self.git("add", "-A")
        self.git("commit", "-q", "--no-verify", "-m", message, *extra)
        return self.git("rev-parse", "HEAD").stdout.strip()

    def install_hooks(self) -> None:
        installed = subprocess.run(
            ["sh", str(SCRIPT.with_name("install-hooks.sh"))],
            cwd=self.repo, check=True, capture_output=True, text=True, env=self.env,
        )
        self.assertIn("Privacy hooks installed", installed.stdout)

    def test_tree_scans_every_tracked_file_and_path_from_anywhere_in_the_repository(self):
        self.write_terms("Contoso")
        (self.repo / "docs").mkdir()
        (self.repo / "clean.md").write_text("Run it on mac-mini.\n")
        (self.repo / "leak.md").write_text(f"ok\n{REAL_HOME}/src\n")
        (self.repo / "cache.pyc").write_bytes(b"\0\xff\x01" + REAL_HOME.encode() + b"/x.py\0")
        (self.repo / "contoso-notes.md").write_text("fine\n")
        (self.repo / "untracked.md").write_text(REAL_HOME)
        self.git("add", "clean.md", "leak.md", "cache.pyc", "contoso-notes.md")

        result = self.scan("tree", cwd=self.repo / "docs")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            result.stdout,
            "tracked paths:3:1: private-term 3\ncache.pyc:1:4: home-path\nleak.md:2:1: home-path\n",
        )
        self.assertNotIn(JANE, result.stdout + result.stderr)
        revealed = self.scan("--reveal", "tree")
        self.assertIn(f"leak.md:2:1: home-path: {REAL_HOME}\n", revealed.stdout)

        self.git("rm", "-q", "--cached", "contoso-notes.md")
        (self.repo / "leak.md").write_text("ok\n/Users/me/src\n")
        (self.repo / "cache.pyc").write_bytes(b"\0\xff\x01/Users/me/x.py\0")
        clean = self.scan("tree")
        self.assertEqual(clean.returncode, 0, clean.stdout)
        self.assertIn("clean, 4 input(s)", clean.stderr)  # the path list and three files

    def test_reports_without_reveal_withhold_private_file_names(self):
        self.write_terms("Contoso")
        (self.repo / "contoso-setup.md").write_text(f"mail {REAL_EMAIL}\n")
        self.git("add", "-A")
        result = self.scan("tree")
        self.assertEqual(
            result.stdout,
            "tracked paths:1:1: private-term 3\n<withheld>-setup.md:1:6: email\n",
        )

    def test_files_reads_stdin_under_a_name(self):
        result = self.scan("files", "--name", "pull request", "-", stdin=f"Title\n\nSent from {REAL_EMAIL}\n")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "pull request:3:11: email\n")

    def test_private_terms_are_reported_by_line_and_can_be_required(self):
        required = self.scan("--require-terms", "files", "-", stdin="anything\n")
        self.assertEqual(required.returncode, 2)
        self.assertIn("no private terms configured", required.stderr)

        self.write_terms("Fabrikam", "Contoso")
        result = self.scan("--require-terms", "files", "--name", "notes", "-", stdin="Built for CONTOSO.\n")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "notes:1:11: private-term 4\n")
        self.assertNotIn("CONTOSO", result.stdout + result.stderr)
        self.assertIn("2 private term(s)", result.stderr)

        self.env["PRIVACY_SCAN_TERMS"] = "Northwind\n"
        from_variable = self.scan("--reveal", "files", "-", stdin="Contoso and Northwind\n")
        self.assertEqual(from_variable.stdout, "stdin:1:13: private-term 1: Northwind\n")

    def test_staged_scans_what_the_index_will_commit_by_object(self):
        self.write_terms("Contoso")
        (self.repo / "notes.md").write_text("intro\nfor Contoso\n")
        (self.repo / "1:odd name.md").write_text("fine\n")
        self.git("add", "-A")
        (self.repo / "notes.md").write_text("intro\nfor acme\n")  # the working copy is clean; the index is not

        result = self.scan("staged")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stdout, "notes.md:2:5: private-term 3\n")

        self.git("add", "notes.md")
        (self.repo / "contoso.md").write_text("fine\n")
        self.git("add", "contoso.md")
        self.assertEqual(self.scan("staged").stdout, "staged paths:2:1: private-term 3\n")

    def test_message_scans_comments_but_not_the_verbose_diff(self):
        self.write_terms("Contoso")
        message = self.repo / "COMMIT_EDITMSG"
        message.write_text(
            "Add the scanner\n\n## Contoso notes\n"
            f"{privacy_scan.SCISSORS}\ndiff --git a/x b/x\n+Contoso\n"
        )
        result = self.scan("message", str(message))
        self.assertEqual(result.stdout, "commit message:3:4: private-term 3\n")

    def test_commits_scans_authors_messages_paths_and_every_added_line(self):
        self.write_terms("Contoso")
        (self.repo / "a.md").write_text("one\n")
        base = self.commit_all("Start")
        (self.repo / "a.md").write_text(f"one\n{REAL_HOME}/src\n++ Contoso after a plus\n")
        added = self.commit_all("Add a path")
        (self.repo / "a.md").write_text("one\n")
        self.commit_all("Remove it again\n\ndiff --git a/x b/x\nthen Contoso\n")
        (self.repo / ".gitattributes").write_text("*.dat binary\n")
        (self.repo / "data.dat").write_bytes(b"\0" + REAL_HOME.encode() + b"/blob\n")
        (self.repo / "contoso notes.md").write_text("two\n")
        authored = self.commit_all(f"Mention {REAL_V4}", f"--author=Jane <{REAL_EMAIL}>")
        self.git("checkout", "-q", "-b", "side", base)
        (self.repo / "side.md").write_text("side\n")
        self.commit_all("Side")
        self.git("checkout", "-q", "main")
        self.git("merge", "-q", "--no-commit", "side")
        (self.repo / "side.md").write_text("side\nmerged in Contoso\n")
        merge = self.commit_all("Merge side")
        self.git("config", "diff.noprefix", "true")
        (self.repo / "Contoso-empty.md").write_text("")
        (self.repo / 'q"contoso.md').write_text("fine\n")
        oddly_named = self.commit_all("Odd names")

        result = self.scan("commits", f"{base}..main")
        self.assertEqual(result.returncode, 1)
        found = result.stdout.splitlines()
        removed = self.git("rev-parse", f"{authored}~1").stdout.strip()
        for expected in [
            f"commit {added[:12]} a.md:2:1: home-path",
            f"commit {added[:12]} a.md:3:4: private-term 3",
            f"commit {removed[:12]}:6:6: private-term 3",
            f"commit {authored[:12]}:1:7: email",
            f"commit {authored[:12]}:3:9: tailnet-address",
            f"commit {authored[:12]} paths:2:1: private-term 3",
            f"commit {authored[:12]} data.dat:1:2: home-path",
            f"commit {merge[:12]} side.md:2:11: private-term 3",
            f"commit {oddly_named[:12]} paths:1:1: private-term 3",
            f"commit {oddly_named[:12]} paths:2:3: private-term 3",
        ]:
            self.assertIn(expected, found)

    def test_hooks_refuse_private_details_in_files_messages_and_pushes(self):
        self.write_terms("Contoso")
        self.install_hooks()
        remote = Path(self.directory.name) / "remote.git"
        self.git("init", "-q", "--bare", str(remote))
        self.git("remote", "add", "origin", str(remote))
        (self.repo / "a.md").write_text("one\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "Start")
        self.git("push", "-q", "origin", "main")
        start = self.git("rev-parse", "HEAD").stdout.strip()

        (self.repo / "a.md").write_text("one\nContoso\n")
        self.git("add", "-A")
        refused = self.git("commit", "-q", "-m", "Add a line", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("a.md:2:1: private-term 3: Contoso", refused.stdout + refused.stderr)

        (self.repo / "a.md").write_text("one\ntwo\n")
        self.git("add", "-A")
        refused = self.git("commit", "-q", "-m", "Add a line for Contoso", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("commit message:1:16: private-term 3: Contoso", refused.stdout + refused.stderr)
        self.assertEqual(self.git("rev-parse", "HEAD").stdout.strip(), start)

        # A commit that skipped the commit hooks still can't leave.
        self.git("commit", "-q", "--no-verify", "-m", "Add a line for Contoso")
        refused = self.git("push", "origin", "main", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("private-term 3: Contoso", refused.stdout + refused.stderr)
        self.assertEqual(self.git("rev-parse", "main", cwd=remote).stdout.strip(), start)

        self.git("commit", "-q", "--amend", "-m", "Add a line")
        self.git("push", "-q", "origin", "main")
        self.git("branch", "contoso-fix")
        refused = self.git("push", "origin", "contoso-fix", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("pushed refs:1:12: private-term 3: contoso", refused.stdout + refused.stderr)

        self.git("tag", "-a", "v1", "-m", "For Contoso")
        refused = self.git("push", "origin", "v1", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("tag refs/tags/v1:6:5: private-term 3: Contoso", refused.stdout + refused.stderr)

    def test_a_new_branch_pushed_by_url_scans_only_what_no_remote_has(self):
        remote = Path(self.directory.name) / "remote.git"
        self.git("init", "-q", "--bare", str(remote))
        self.git("remote", "add", "origin", str(remote))
        (self.repo / "a.md").write_text(f"{REAL_HOME}/src\n")  # old history, already published
        self.commit_all("Start")
        self.git("push", "-q", "origin", "main")
        self.write_terms("Contoso")
        self.install_hooks()

        self.git("switch", "-q", "-c", "topic")
        (self.repo / "b.md").write_text("clean\n")
        self.commit_all("Clean work")
        pushed = self.git("push", str(remote), "topic", check=False)
        self.assertEqual(pushed.returncode, 0, pushed.stderr)

        (self.repo / "c.md").write_text("Contoso\n")
        self.commit_all("Leaky work")
        refused = self.git("push", str(remote), "topic", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("c.md:1:1: private-term 3", refused.stdout + refused.stderr)
        self.assertNotIn("a.md", refused.stdout + refused.stderr)

    def test_hooks_installed_with_terms_refuse_to_run_without_them(self):
        self.write_terms("Contoso")
        self.install_hooks()
        (self.home / ".config/elsecall/private-terms").unlink()
        (self.repo / "a.md").write_text("one\n")
        self.git("add", "-A")
        refused = self.git("commit", "-q", "-m", "Start", check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("no private terms configured", refused.stderr)

    def test_hooks_run_the_installed_copy_not_the_checked_out_branch(self):
        self.install_hooks()
        hooks = Path(self.git("config", "core.hooksPath").stdout.strip())
        self.assertEqual(hooks.parent, Path(self.git("rev-parse", "--absolute-git-dir").stdout.strip()))
        self.assertEqual(
            sorted(path.name for path in hooks.iterdir()),
            ["commit-msg", "pre-commit", "pre-push", "privacy_scan.py"],
        )
        (self.repo / "subdirectory").mkdir()
        (self.repo / "subdirectory/a.md").write_text(f"{REAL_HOME}/src\n")
        self.git("add", "-A")
        refused = self.git("commit", "-q", "-m", "From below", check=False, cwd=self.repo / "subdirectory")
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("subdirectory/a.md:1:1: home-path", refused.stdout + refused.stderr)


if __name__ == "__main__":
    unittest.main()
