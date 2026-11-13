"""Install the published release through mise's packslip backend and check it."""

import argparse
import hashlib
import json
import os
import re
import subprocess
import tempfile
from pathlib import Path

import archive_contents
import package

ISSUER = "https://token.actions.githubusercontent.com"
REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
COMMIT = re.compile(r"[0-9a-f]{40}")


def mise_configuration(project, release_version, identity):
    return (
        "[tools]\n"
        + json.dumps("packslip:" + project)
        + " = { version = "
        + json.dumps(release_version)
        + ", identity = "
        + json.dumps(identity)
        + ", issuer = "
        + json.dumps(ISSUER)
        + " }\n"
    )


def identity_for(repository, release_version):
    return (
        f"https://github.com/{repository}/.github/workflows/release.yml"
        f"@refs/tags/v{release_version}"
    )


def validate_statement(statement, project, release_version, commit):
    release = statement.get("predicate", statement)
    source = release.get("source", {})
    if (
        release.get("project") != project
        or release.get("version") != release_version
        or source.get("commit") != commit
    ):
        raise RuntimeError("published release source identity mismatch")


def command(arguments, cwd=None, env=None):
    result = subprocess.run(
        arguments,
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        timeout=180,
        check=False,
    )
    if result.returncode:
        detail = (result.stderr or result.stdout).strip().splitlines()
        suffix = f": {detail[-1]}" if detail else ""
        raise RuntimeError(f"{arguments[0]} {arguments[1]} failed (exit {result.returncode}){suffix}")
    return result.stdout


def verify(repository, release_version, commit):
    if not REPOSITORY.fullmatch(repository) or not COMMIT.fullmatch(commit):
        raise ValueError("invalid release identity")
    if not package.SEMVER.fullmatch(release_version):
        raise ValueError("invalid release version")
    archive_name = package.filename(release_version)
    project = "github.com/" + repository
    identity = identity_for(repository, release_version)
    with tempfile.TemporaryDirectory(prefix="elsecall-mise-") as directory:
        root = Path(directory).resolve(strict=True)
        assets = root / "assets"
        assets.mkdir()
        command(
            [
                "gh",
                "release",
                "download",
                "v" + release_version,
                "--repo",
                repository,
                "--dir",
                str(assets),
                "--pattern",
                archive_name,
                "--pattern",
                "packslip.sigstore.json",
            ]
        )
        bundle = assets / "packslip.sigstore.json"
        archive = assets / archive_name
        command(
            [
                "packslip",
                "verify",
                str(bundle),
                "--identity",
                identity,
                "--issuer",
                ISSUER,
                "--artifact",
                str(archive),
            ]
        )
        statement = json.loads(command(["packslip", "show", str(bundle)]))
        validate_statement(statement, project, release_version, commit)
        configuration = root / "mise.toml"
        configuration.write_text(mise_configuration(project, release_version, identity), encoding="utf-8")
        configuration.chmod(0o600)
        empty = root / "empty.toml"
        empty.write_text("", encoding="utf-8")
        env = os.environ.copy()
        for key in list(env):
            if key.startswith("MISE_") and key != "MISE_GITHUB_TOKEN":
                env.pop(key)
        for key, path in {
            "MISE_DATA_DIR": root / "data",
            "MISE_CACHE_DIR": root / "cache",
            "MISE_STATE_DIR": root / "state",
            "MISE_CONFIG_DIR": root / "config",
            "MISE_GLOBAL_CONFIG_FILE": empty,
            "MISE_CONFIG_FILE": configuration,
        }.items():
            env[key] = str(path)
        env["MISE_YES"] = "1"
        command(["mise", "trust", str(configuration)], cwd=root, env=env)
        command(["mise", "install"], cwd=root, env=env)
        binary = Path(command(["mise", "which", "elsecall"], cwd=root, env=env).strip()).resolve(strict=True)
        if not binary.is_relative_to(root / "data") or not (binary.parent / ".mise-packslip.json").is_file():
            raise RuntimeError("mise did not install a verified release")
        installed = binary.read_bytes()
        packaged = archive_contents.read(archive)["elsecall"]
        if hashlib.sha256(installed).digest() != hashlib.sha256(packaged).digest():
            raise RuntimeError("installed bytes differ from the verified archive")
        reported = command([str(binary), "--version"], env=env).strip()
        if reported != f"elsecall {release_version}":
            raise RuntimeError("installed executable reports a different version")
        return {
            "version": reported,
            "signature_verified": True,
            "mise_install": True,
            "archive_sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
        }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit", required=True)
    args = parser.parse_args()
    print(json.dumps(verify(args.repository, args.version, args.commit)))
