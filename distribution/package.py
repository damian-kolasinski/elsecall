"""Build and package the macOS release without local path metadata."""

import argparse
import gzip
import io
import os
import re
import subprocess
import sys
import tarfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TARGET = "aarch64-apple-darwin"
TOOL = "elsecall"
NUMBER = r"(?:0|[1-9]\d*)"
PRERELEASE = rf"(?:{NUMBER}|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
SEMVER = re.compile(
    rf"{NUMBER}\.{NUMBER}\.{NUMBER}(?:-{PRERELEASE}(?:\.{PRERELEASE})*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)
# Builds made with the local-release-channel feature carry this. They follow a
# developer's own release channel and must never be published; see
# src/release_update/channel.rs.
LOCAL_CHANNEL_MARKER = b"ELSECALL-LOCAL-RELEASE-CHANNEL-BUILD"


def version(root=ROOT):
    value = tomllib.loads((Path(root) / "Cargo.toml").read_text())["package"]["version"]
    if not isinstance(value, str) or not SEMVER.fullmatch(value):
        raise ValueError("invalid release version")
    return value


def filename(release_version, target=TARGET):
    if not SEMVER.fullmatch(release_version) or target != TARGET:
        raise ValueError("unsupported release identity")
    return f"{TOOL}-v{release_version}-{target}.tar.gz"


def release_env(root, env=None):
    root = Path(root)
    base = os.environ if env is None else env
    prepared = base.copy()
    home = Path(prepared.get("HOME", str(Path.home())))
    mappings = [
        (home, "/build/home"),
        (root, "/build/elsecall"),
        (Path(prepared.get("CARGO_HOME", home / ".cargo")), "/build/cargo"),
        (Path(prepared.get("RUSTUP_HOME", home / ".rustup")), "/build/rustup"),
    ]
    prepared.pop("RUSTFLAGS", None)
    prepared["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(
        f"--remap-path-prefix={source}={alias}" for source, alias in mappings
    )
    prepared["MACOSX_DEPLOYMENT_TARGET"] = "13.0"
    return prepared


def cargo():
    """The toolchain's own cargo, which mise.toml pins, rather than mise's cargo
    wrapper: the wrapper adds the build cache, and it reads mise.toml where it
    runs, which a copied source tree (scripts/local_release.py) is not trusted to
    provide. Plain `cargo` without mise."""
    try:
        found = subprocess.run(["mise", "which", "cargo"], cwd=ROOT, capture_output=True, text=True)
    except FileNotFoundError:
        return "cargo"
    return found.stdout.strip() if found.returncode == 0 and found.stdout.strip() else "cargo"


def build(root=ROOT, target=TARGET, features=(), env=None):
    """Build both executables; features and env apply to the CLI only."""
    if target != TARGET:
        raise ValueError("unsupported release target")
    host = subprocess.check_output(["rustc", "-vV"], text=True).split("host: ", 1)[1].splitlines()[0]
    if host != target:
        raise ValueError("release verification requires a native target")
    # Only the caller's env may make a local-channel build, never the shell's.
    base = {
        key: value
        for key, value in release_env(root).items()
        if not key.startswith("ELSECALL_LOCAL_RELEASE_")
    }
    toolchain = cargo()
    for crate, deployment in ((TOOL, "13.0"), ("elsecall-ui", "14.0")):
        command = [toolchain, "build","--locked", "--release", "--target", target, "-p", crate]
        extra = {}
        if crate == TOOL and features:
            command += ["--features", ",".join(features)]
            extra = env or {}
        subprocess.run(
            command,
            cwd=root,
            env={**base, **extra, "MACOSX_DEPLOYMENT_TARGET": deployment},
            check=True,
        )
    return Path(root) / "target" / target / "release" / TOOL


def archive(binary, output, release_version, target=TARGET, local_channel=False):
    """Package a release; local_channel archives only builds that follow the local channel."""
    binary = Path(binary)
    if binary.is_symlink() or not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("release executable unavailable")
    binary = binary.resolve(strict=True)
    reported = subprocess.check_output([str(binary), "--version"], text=True).strip()
    if reported != f"{TOOL} {release_version}":
        raise ValueError("release version mismatch")
    companion = binary.with_name("elsecall-ui")
    if companion.is_symlink() or not companion.is_file() or not os.access(companion, os.X_OK):
        raise ValueError("release UI executable unavailable")
    executable = binary.read_bytes()
    window = companion.read_bytes()
    if any(str(Path.home()).encode() in data for data in (executable, window)):
        raise ValueError("release executable contains a private build path")
    if (LOCAL_CHANNEL_MARKER in executable) != local_channel:
        raise ValueError(
            "release executable follows the local release channel"
            if not local_channel
            else "local channel archive needs a local-release-channel build"
        )
    readme = (
        b"Elsecall\n\n"
        b"This archive contains the elsecall and elsecall-ui executables.\n"
        b"Install with mise, then run elsecall install. Open the window with elsecall ui.\n"
    )
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    path = output / filename(release_version, target)
    members = ((TOOL, executable, 0o755), ("elsecall-ui", window, 0o755), ("README.txt", readme, 0o644))
    with (
        path.open("wb") as raw,
        gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0) as compressed,
    ):
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as tar:
            for name, data, mode in members:
                item = tarfile.TarInfo(name)
                item.size = len(data)
                item.mode = mode
                item.mtime = item.uid = item.gid = 0
                item.uname = item.gname = ""
                tar.addfile(item, io.BytesIO(data))
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build", action="store_true")
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--out", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    binary = build() if args.build else args.binary
    if binary is None:
        parser.error("choose --build or --binary")
    print(archive(binary, args.out, version()).name)


if __name__ == "__main__":
    main()
