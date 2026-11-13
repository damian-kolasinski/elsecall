"""Run the packaged executable and require its reported version."""

import argparse
import stat
import subprocess
import tempfile
from pathlib import Path

import archive_contents
import package


def check(archive, release_version):
    files = archive_contents.read(archive)
    if set(files) != {"elsecall", "README.txt"}:
        raise ValueError("release archive has unexpected files")
    with tempfile.TemporaryDirectory(prefix="elsecall-smoke-") as directory:
        binary = Path(directory) / "elsecall"
        binary.write_bytes(files["elsecall"])
        binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
        reported = subprocess.check_output([str(binary), "--version"], text=True).strip()
        if reported != f"elsecall {release_version}":
            raise ValueError("packaged executable reports a different version")
        if str(Path.home()).encode() in files["elsecall"]:
            raise ValueError("packaged executable contains a private build path")
    return {"version": release_version, "archive": Path(archive).name}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    args = parser.parse_args()
    print(check(args.archive, package.version()))
