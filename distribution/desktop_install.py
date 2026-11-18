"""Install a release on this CI runner, require its window to open, then remove it
and require the window to close and the CLI's mise request to go with it.

This replaces the runner's own Elsecall installation, so it refuses to run outside
GitHub Actions. Pass an archive to install from it, or --script to run the
published one-command installer as a user would, with --expect-version naming
the release it must install.
"""

import argparse
import json
import os
import subprocess
import tempfile
import time
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


def uninstalled(output, mise):
    """`elsecall uninstall`'s result, which must report the mise request as `mise`."""
    document = json.loads(output)
    data = document.get("data", {})
    if data.get("status") != "uninstalled":
        raise RuntimeError(f"uninstall did not finish: {output}")
    if data.get("mise", {}).get("state") != mise:
        raise RuntimeError(f"the mise request is not {mise}: {json.dumps(data.get('mise'))}")
    return data


WINDOW = "/Elsecall.app/Contents/MacOS/Elsecall"


def window_closes(within=20):
    """Waits for every window running from the published app to exit."""
    deadline = time.monotonic() + within
    while subprocess.run(["pgrep", "-f", WINDOW], capture_output=True, check=False).returncode == 0:
        if time.monotonic() > deadline:
            raise RuntimeError(f"the window still runs {within} s after uninstall")
        time.sleep(0.5)


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


def from_script(script):
    environment = {key: value for key, value in os.environ.items() if key != "ELSECALL_VERSION"}
    installed = run(["sh", str(script)], env=environment, cwd=Path.home())
    if installed.returncode != 0:
        raise RuntimeError("the installer failed")
    return installed.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path, nargs="?")
    parser.add_argument("--script", type=Path)
    parser.add_argument("--expect-version")
    args = parser.parse_args()
    require_ci()
    if args.script:
        output = from_script(args.script)
    elif args.archive:
        output = from_archive(args.archive)
    else:
        parser.error("name an archive or --script")
    window = opened_window(output)
    cli = Path.home() / ".local/bin/elsecall"
    if args.expect_version:
        reported = run([str(cli), "--version"]).stdout.strip()
        if reported != f"elsecall {args.expect_version}":
            raise RuntimeError(f"installed {reported!r}, not release {args.expect_version}")
    if run([str(cli), "status"]).returncode != 0:
        raise RuntimeError("the installed service is not ready")
    removed = run([str(cli), "uninstall"])
    if removed.returncode != 0:
        raise RuntimeError("uninstall failed")
    # Only the one-command installer requests the CLI in mise.
    data = uninstalled(removed.stdout, "removed" if args.script else "absent")
    window_closes()
    print(json.dumps({"window": window, "uninstalled": True, "mise": data["mise"]}))


if __name__ == "__main__":
    main()
