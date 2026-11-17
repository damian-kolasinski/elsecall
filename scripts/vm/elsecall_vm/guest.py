"""Reaching a running guest: commands with deadlines, files, and its screen.

Each failure here becomes a `VmError`, and a failure that leaves the guest in
an unknown state carries a screenshot of what was on screen.
"""

from __future__ import annotations

import time
from contextlib import contextmanager
from pathlib import Path
from typing import Iterator

from . import registry, tart
from .errors import VmError, redact
from .vnc import Endpoint, Screen, endpoint_from_log

# Expands a leading ~/ in $1, a guest path, to the guest user's home, as `p`;
# PATH_ARG2 does the same for $2, as `q`.
PATH_ARG = 'p=$1; case $p in "~/"*) p="$HOME/${p#??}";; esac; '
PATH_ARG2 = 'q=$2; case $q in "~/"*) q="$HOME/${q#??}";; esac; '


def tail(text: str, size: int = 2000) -> str:
    """The end of a log, redacted before it is cut so no partial URL escapes."""
    return redact(text)[-size:]


def endpoint(name: str) -> Endpoint:
    log = registry.run_log(name)
    found = endpoint_from_log(log.read_text()) if log.exists() else None
    if found is None:
        raise VmError(f"{name} has no VNC server; boot it with `ecvm up {name}`")
    return found


@contextmanager
def screen(name: str) -> Iterator[Screen]:
    """A VNC connection whose failures, including timeouts, become `VmError`s."""
    try:
        connection = Screen(endpoint(name))
    except VmError:
        raise
    except Exception as error:
        raise VmError(f"cannot reach {name}'s screen over VNC: {error}") from None
    try:
        yield connection
    except VmError:
        raise
    except Exception as error:
        raise VmError(f"VNC to {name} failed: {error}") from None
    finally:
        connection.close()


def screenshot(name: str, path: Path | None = None) -> Path:
    if path is None:
        shots = registry.vm_dir(name) / "shots"
        shots.mkdir(exist_ok=True)
        path = shots / time.strftime("%Y%m%d-%H%M%S.png")
    with screen(name) as connection:
        return connection.save(path)


def evidence(name: str, path: Path | None = None) -> Path | None:
    """A screenshot for a failure report; None when the screen cannot be reached either."""
    try:
        return screenshot(name, path)
    except VmError:
        return None


def run(name: str, script: str, timeout: float, stdin: bytes | None = None, args: tuple[str, ...] = ()) -> str:
    """Run a shell script in the guest with `args` as $1...; past the deadline, fail with a screenshot."""
    argv = ["sh", "-c", script, "sh", *args]
    try:
        done = tart.guest(name, argv, timeout=timeout, stdin=stdin)
    except tart.GuestTimeout as error:
        raise VmError(
            f"guest command did not finish within {timeout:.0f}s: {error}", screenshot=evidence(name)
        ) from None
    if done.returncode != 0:
        detail = (done.stderr or done.stdout or b"").decode(errors="replace").strip()
        raise VmError(f"guest command exited {done.returncode}: {detail}")
    return done.stdout.decode(errors="replace")


def push(name: str, data: bytes, guest_path: str, mode: str = "644") -> None:
    run(name, PATH_ARG + 'mkdir -p "$(dirname "$p")" && cat > "$p" && chmod "$2" "$p"', 120,
        stdin=data, args=(guest_path, mode))


def pull(name: str, guest_path: str) -> bytes:
    try:
        done = tart.guest(name, ["sh", "-c", PATH_ARG + 'cat "$p"', "sh", guest_path], timeout=300)
    except tart.GuestTimeout:
        raise VmError(f"reading {guest_path} took over 300s", screenshot=evidence(name)) from None
    if done.returncode != 0:
        raise VmError(f"cannot read {guest_path}: {done.stderr.decode(errors='replace').strip()}")
    return done.stdout
