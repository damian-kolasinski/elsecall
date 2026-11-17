"""Tart, the hypervisor CLI: listing, cloning, booting, stopping and guest commands.

Only Tart details live here. Callers decide which VMs they may touch.
"""

from __future__ import annotations

import json
import shutil
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

from .errors import VmError, redact

# macOS lets the host run at most two macOS guests at once.
MAX_RUNNING = 2


@dataclass(frozen=True)
class Listed:
    name: str
    running: bool
    local: bool


def _tart() -> str:
    found = shutil.which("tart")
    if not found:
        raise VmError("tart is not on PATH; install it from https://github.com/cirruslabs/tart/releases")
    return found


def _run(args: list[str], timeout: float = 600) -> str:
    try:
        done = subprocess.run([_tart(), *args], capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired as error:
        raise VmError(f"tart {' '.join(args)} did not finish within {timeout:.0f}s") from error
    if done.returncode != 0:
        raise VmError(f"tart {' '.join(args)} failed: {(done.stderr or done.stdout).strip()}")
    return done.stdout


def listed() -> list[Listed]:
    return [
        Listed(entry["Name"], bool(entry["Running"]), entry["Source"] == "local")
        for entry in json.loads(_run(["list", "--format", "json"], timeout=60))
    ]


def find(name: str) -> Listed | None:
    return next((vm for vm in listed() if vm.name == name and vm.local), None)


def running() -> list[str]:
    return [vm.name for vm in listed() if vm.running]


def mac(name: str) -> str:
    """The VM's MAC address, which Tart chooses at random for every clone."""
    return json.loads(_run(["get", name, "--format", "json"], timeout=60))["MACAddress"]


def clone(source: str, target: str) -> None:
    _run(["clone", source, target])


def set_display(name: str, size: str) -> None:
    _run(["set", name, "--display", size], timeout=60)


def delete(name: str) -> None:
    _run(["delete", name], timeout=120)


def stop(name: str, grace_seconds: int = 30) -> None:
    _run(["stop", name, "--timeout", str(grace_seconds)], timeout=grace_seconds + 60)


def start(name: str, log: Path) -> subprocess.Popen:
    """Boot headless with Virtualization.framework's VNC server; Tart prints its URL to `log`."""
    log.touch(mode=0o600, exist_ok=True)
    log.chmod(0o600)
    with log.open("w") as out:
        return subprocess.Popen(
            [_tart(), "run", "--vnc-experimental", "--no-audio", name],
            stdin=subprocess.DEVNULL,
            stdout=out,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )


def wait_stopped(name: str, timeout: float) -> bool:
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        vm = find(name)
        if vm is None or not vm.running:
            return True
        time.sleep(1)
    return False


class GuestTimeout(Exception):
    pass


def guest(
    name: str,
    argv: list[str],
    *,
    timeout: float,
    stdin: bytes | None = None,
    capture: bool = True,
) -> subprocess.CompletedProcess:
    """Run `argv` in the guest through the Tart guest agent, killing it at the deadline."""
    command = [_tart(), "exec", *(["-i"] if stdin is not None else []), name, *argv]
    try:
        return subprocess.run(
            command,
            input=stdin,
            capture_output=capture,
            timeout=timeout,
            stdin=None if stdin is not None else subprocess.DEVNULL,
        )
    except subprocess.TimeoutExpired as error:
        raise GuestTimeout(redact(" ".join(argv))) from error


def guest_async(name: str, argv: list[str]) -> subprocess.Popen:
    """Start `argv` in the guest without waiting; the caller owns the deadline."""
    return subprocess.Popen(
        [_tart(), "exec", name, *argv],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
    )


def guest_ok(name: str, argv: list[str], timeout: float = 15) -> bool:
    """Whether `argv` succeeds in the guest within `timeout`; a timeout counts as no."""
    try:
        return guest(name, argv, timeout=timeout).returncode == 0
    except GuestTimeout:
        return False
