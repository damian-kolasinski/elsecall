"""Build and package the macOS release without local path metadata."""

import argparse
import gzip
import io
import os
import re
import subprocess
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


def build(root=ROOT, target=TARGET):
    if target != TARGET:
        raise ValueError("unsupported release target")
    host = subprocess.check_output(["rustc", "-vV"], text=True).split("host: ", 1)[1].splitlines()[0]
    if host != target:
        raise ValueError("release verification requires a native target")
    subprocess.run(
        ["cargo", "build", "--locked", "--release", "--target", target, "-p", TOOL],
        cwd=root,
        env=release_env(root),
        check=True,
    )
    return Path(root) / "target" / target / "release" / TOOL


def archive(binary, output, release_version, target=TARGET):
    binary = Path(binary)
    if binary.is_symlink() or not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("release executable unavailable")
    binary = binary.resolve(strict=True)
    reported = subprocess.check_output([str(binary), "--version"], text=True).strip()
    if reported != f"{TOOL} {release_version}":
        raise ValueError("release version mismatch")
    executable = binary.read_bytes()
    if str(Path.home()).encode() in executable:
        raise ValueError("release executable contains a private build path")
    readme = (
        b"Elsecall\n\n"
        b"This archive contains the elsecall executable.\n"
        b"Install a signed release with mise's packslip backend, then run elsecall install.\n"
    )
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    path = output / filename(release_version, target)
    members = ((TOOL, executable, 0o755), ("README.txt", readme, 0o644))
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
