"""Install a release on this CI runner, require its window to open, then remove it.

This replaces the runner's own Elsecall installation, so it refuses to run outside
GitHub Actions. Pass an archive to install from it, or --script to run the
published one-command installer for a release version.
"""

import argparse
import json
import os
import subprocess
import tempfile
from pathlib import Path

import archive_contents


def require_ci(environment=os.environ):
    if environment.get("GITHUB_ACTIONS") != "true":
        raise SystemExit("desktop_install.py replaces this machine's installation; it runs only in CI")


def opened_window(output):
    """The window section of `elsecall install --open`'s result, which must be opened."""
    lines = output.strip().splitlines()
    if not lines:
        raise RuntimeError("elsecall install printed no result")
    document = json.loads(lines[-1])
    window = document.get("data", {}).get("window", {})
    if window.get("state") != "opened":
        raise RuntimeError(f"the window did not open: {json.dumps(window)}")
    return window


def run(arguments, **options):
    result = subprocess.run(arguments, capture_output=True, text=True, timeout=600, check=False, **options)
    print(f"$ {' '.join(map(str, arguments))}\n{result.stdout}{result.stderr}", flush=True)
    return result


def from_archive(archive):
    files = archive_contents.read(archive)
    with tempfile.TemporaryDirectory(prefix="elsecall-desktop-") as directory:
        for name in ("elsecall", "elsecall-ui"):
            path = Path(directory) / name
            path.write_bytes(files[name])
            path.chmod(0o755)
        installed = run([str(Path(directory) / "elsecall"), "install", "--open"])
        return installed.stdout


def from_script(script, version):
    environment = {**os.environ, "ELSECALL_VERSION": version}
    installed = run(["sh", str(script)], env=environment, cwd=Path.home())
    if installed.returncode != 0:
        raise RuntimeError("the installer failed")
    return installed.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path, nargs="?")
    parser.add_argument("--script", type=Path)
    parser.add_argument("--version")
    args = parser.parse_args()
    require_ci()
    if args.script:
        output = from_script(args.script, args.version or "latest")
    elif args.archive:
        output = from_archive(args.archive)
    else:
        parser.error("name an archive or --script")
    window = opened_window(output)
    cli = Path.home() / ".local/bin/elsecall"
    if run([str(cli), "status"]).returncode != 0:
        raise RuntimeError("the installed service is not ready")
    if run([str(cli), "uninstall"]).returncode != 0:
        raise RuntimeError("uninstall failed")
    print(json.dumps({"window": window, "uninstalled": True}))


if __name__ == "__main__":
    main()
