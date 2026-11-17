"""Build, sign and serve a local release channel for testing Elsecall's update path.

`build` makes release archives of the working tree at two or more versions, with
the `local-release-channel` feature pointing them at one channel, signs each with
a local packslip key, and writes the channel: the release list, each version's
archive and bundle, and an installer for the oldest version. `serve` publishes
the channel over HTTP. See scripts/local-release.sh.

Nothing here tags, publishes or touches GitHub. Builds made this way can't be
packaged as published releases (see distribution/package.py).
"""

import argparse
import functools
import http.server
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "distribution"))

import package  # noqa: E402

PROJECT = "elsecall.localhost"
PACKSLIP = "github:jdx/packslip@1.11.1"
FEATURE = "local-release-channel"
# The address a Tart guest reaches its host at.
DEFAULT_URL = "http://192.168.64.1:8765"
STABLE = re.compile(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)")


def stable(value):
    if not STABLE.fullmatch(value):
        raise argparse.ArgumentTypeError(f"{value} is not a stable version such as 1.2.3")
    return value


def next_patch(version):
    major, minor, patch = (int(part) for part in stable(version).split("."))
    return f"{major}.{minor}.{patch + 1}"


def channel_url(value, home=None):
    parts = urlsplit(value)
    if parts.scheme not in ("http", "https", "file") or (parts.scheme != "file" and not parts.hostname):
        raise argparse.ArgumentTypeError("the channel URL must be http://, https:// or file:///")
    if parts.scheme == "file" and (parts.netloc or not parts.path.startswith("/")):
        raise argparse.ArgumentTypeError("a file channel URL is file:///absolute/path")
    if re.search(r"[\s\"'`$\\]", value):
        raise argparse.ArgumentTypeError("the channel URL must not contain spaces, quotes, $, ` or \\")
    # The URL is compiled in, and packaging refuses executables naming this home.
    home = str(Path.home() if home is None else home)
    if parts.scheme == "file" and (parts.path == home or parts.path.startswith(home + "/")):
        raise argparse.ArgumentTypeError(
            f"a file channel inside {home} would put your home path in the build; "
            "serve it over http, or use a directory such as /Users/Shared/elsecall-channel"
        )
    return value.rstrip("/")


def with_version(manifest, lock, version):
    """Cargo.toml and Cargo.lock text with the elsecall package set to `version`."""
    manifest, count = re.subn(
        r'(\[package\]\nname = "elsecall"\nversion = )"[^"]+"', rf'\1"{version}"', manifest, count=1
    )
    lock, locked = re.subn(r'(\nname = "elsecall"\nversion = )"[^"]+"', rf'\1"{version}"', lock, count=1)
    if count != 1 or locked != 1:
        raise ValueError("could not find the elsecall package version to set")
    return manifest, lock


def listing(versions):
    """The release list the local channel serves: the versions it holds, oldest first."""
    return sorted(versions, key=lambda version: tuple(int(part) for part in version.split(".")))


def installer(template, channel, version, key):
    for placeholder, value in (("@CHANNEL@", channel), ("@VERSION@", version), ("@KEY@", key)):
        if template.count(placeholder) != 1 or re.search(r"[\s\"'`$\\]", value):
            raise ValueError(f"cannot set {placeholder} in the installer")
        template = template.replace(placeholder, value)
    return template


def public_key(key):
    return key.with_suffix(".pub").read_text().strip().splitlines()[-1]


def tool(name):
    """A tool from mise.toml, resolved from the repository so any directory works."""
    found = subprocess.run(["mise", "which", name], cwd=ROOT, capture_output=True, text=True)
    if found.returncode:
        raise SystemExit(f"{name} is unavailable: run `mise install` in {ROOT}")
    return found.stdout.strip()


def mirror(source_root, copy):
    """Copy the working tree's files (tracked, or untracked and not ignored) into
    `copy`, rewriting only files that differ so unchanged crates stay built."""
    listed = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=source_root,
        check=True,
        capture_output=True,
    ).stdout.split(b"\0")
    # A tracked file deleted from the working tree is still listed; leave it out.
    wanted = {name.decode() for name in listed if name and (source_root / name.decode()).is_file()}
    for name in sorted(wanted):
        source = source_root / name
        target = copy / name
        if target.is_file() and target.read_bytes() == source.read_bytes():
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
        shutil.copymode(source, target)
    for directory, children, files in os.walk(copy):
        if Path(directory) == copy and "target" in children:
            children.remove("target")
        for name in files:
            path = Path(directory, name)
            if str(path.relative_to(copy)) not in wanted:
                path.unlink()


def outside_the_tree(out):
    """The output may sit under target/ or outside the repository, never where
    the mirror or git would pick up its key and build tree."""
    if out.is_relative_to(ROOT) and not out.is_relative_to(ROOT / "target"):
        raise SystemExit(f"--out must be under {ROOT / 'target'} or outside the repository")
    return out


def build(args):
    out = outside_the_tree(args.out.resolve())
    channel = out / "channel"
    key = out / "key" / "release.key"
    packslip = tool("packslip")
    if not key.exists():
        key.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run([packslip, "keygen", "--out", str(key)], check=True)
    versions = args.versions or [package.version(), next_patch(package.version())]
    if not set(args.fail_to_start) <= set(versions):
        raise SystemExit("--fail-to-start names a version this build does not make")
    settings = out / "channel.json"
    if settings.exists() and json.loads(settings.read_text())["url"] != args.url:
        raise SystemExit(
            f"{channel} holds builds for {json.loads(settings.read_text())['url']}; "
            f"pass that --url, or remove {out} to start a channel for {args.url}"
        )
    source = out / "source"
    source.mkdir(parents=True, exist_ok=True)
    mirror(ROOT, source)
    manifest = (source / "Cargo.toml").read_text()
    lock = (source / "Cargo.lock").read_text()
    pinned = {"ELSECALL_LOCAL_RELEASE_CHANNEL": args.url, "ELSECALL_LOCAL_RELEASE_KEY": public_key(key)}
    for version in versions:
        broken = ", failing at startup" if version in args.fail_to_start else ""
        print(f"Building {version} for {args.url}{broken}", file=sys.stderr)
        versioned_manifest, versioned_lock = with_version(manifest, lock, version)
        (source / "Cargo.toml").write_text(versioned_manifest)
        (source / "Cargo.lock").write_text(versioned_lock)
        fault = {"ELSECALL_LOCAL_RELEASE_FAULT": "serve"} if version in args.fail_to_start else {}
        binary = package.build(source, features=(FEATURE,), env={**pinned, **fault})
        folder = channel / f"v{version}"
        archive = package.archive(binary, folder, version, local_channel=True)
        subprocess.run(
            [packslip, "create", "--project", PROJECT, "--version", version,
             "--bin", "elsecall", "--bin", "elsecall-ui", "--key", str(key), "--no-log",
             "--url-base", f"{args.url}/v{version}", "--out", str(folder),
             f"{archive}:macos/aarch64"],
            check=True,
        )
    (source / "Cargo.toml").write_text(manifest)
    (source / "Cargo.lock").write_text(lock)
    published = [
        path.name[1:] for path in channel.glob("v*")
        if STABLE.fullmatch(path.name[1:]) and (path / "packslip.sigstore.json").is_file()
    ]
    (channel / "releases.json").write_text(json.dumps(listing(published), indent=2) + "\n")
    startable = sorted(set(versions) - set(args.fail_to_start), key=lambda v: tuple(int(p) for p in v.split(".")))
    if startable:
        template = (ROOT / "scripts" / "local-release-install.sh").read_text()
        (channel / "install.sh").write_text(installer(template, args.url, startable[0], public_key(key)))
    settings.write_text(json.dumps({"url": args.url}) + "\n")
    print(
        f"\nThe channel is {channel}, listing {', '.join(sorted(published))}.\n"
        f"Builds trust only {key.with_suffix('.pub')}, and check {args.url} for updates.\n\n"
        f"Serve it:            scripts/local-release.sh serve\n"
        f"Then, in a test VM:  curl -fsSL {args.url}/install.sh | sh\n"
        f"                     elsecall update check\n"
        f"                     elsecall update install --reopen\n"
        "Never install local builds in an account whose Elsecall you rely on."
    )


def serve(args):
    out = args.out.resolve()
    settings = json.loads((out / "channel.json").read_text())
    parts = urlsplit(settings["url"])
    if parts.scheme != "http":
        raise SystemExit(f"The channel was built for {settings['url']}; serve that location yourself.")
    host = args.bind or parts.hostname
    port = parts.port or 80
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=str(out / "channel"))
    try:
        server = http.server.ThreadingHTTPServer((host, port), handler)
    except OSError as error:
        hint = ""
        if host == urlsplit(DEFAULT_URL).hostname:
            hint = " Start the Tart VM first: the host has this address only while a VM runs."
        raise SystemExit(f"Cannot listen on {host}:{port}: {error.strerror}.{hint}") from None
    print(f"Serving {out / 'channel'} at http://{host}:{port}; Ctrl-C stops.", file=sys.stderr)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--out", type=Path, default=ROOT / "target" / "local-release",
                        help="where the key, build tree and channel live (default: target/local-release)")
    commands = parser.add_subparsers(dest="command", required=True)
    build_parser = commands.add_parser("build", parents=[common], help="build and sign releases into the channel")
    build_parser.add_argument("--url", type=channel_url, default=DEFAULT_URL,
                              help=f"where installations will read the channel (default: {DEFAULT_URL}, "
                                   "the host as a Tart guest sees it)")
    build_parser.add_argument("--fail-to-start", action="append", default=[], type=stable, metavar="VERSION",
                              help="build VERSION so its service refuses to start; updating to it "
                                   "rehearses the updater's rollback (repeatable)")
    build_parser.add_argument("versions", nargs="*", type=stable,
                              help="versions to build (default: Cargo.toml's and the next patch)")
    serve_parser = commands.add_parser("serve", parents=[common], help="serve the channel over HTTP")
    serve_parser.add_argument("--bind", help="address to listen on (default: the channel URL's host)")
    args = parser.parse_args()
    {"build": build, "serve": serve}[args.command](args)


if __name__ == "__main__":
    os.umask(0o022)
    main()
