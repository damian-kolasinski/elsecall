"""Waiting for guest conditions against a deadline, failing with a screenshot."""

from __future__ import annotations

import time
from dataclasses import dataclass
from pathlib import Path

from . import tart, terminal
from .deadline import poll
from .errors import VmError
from .guest import PATH_ARG, evidence


@dataclass(frozen=True)
class Condition:
    """A guest probe that prints something once the condition holds."""

    what: str
    argv: list[str]


def session_done(label: str) -> Condition:
    status = terminal.status_path(terminal.check_label(label))
    return Condition(f"session {label} to finish", ["sh", "-c", PATH_ARG + 'cat "$p" 2>/dev/null', "sh", status])


def process(pattern: str) -> Condition:
    # pgrep runs without a shell around it, so it cannot match its own parent.
    return Condition(f"a process matching {pattern!r}", ["pgrep", "-f", "--", pattern])


def file(path: str) -> Condition:
    return Condition(f"{path} to exist", ["sh", "-c", PATH_ARG + 'test -e "$p" && echo yes', "sh", path])


def shell(command: str) -> Condition:
    return Condition(f"`{command}` to succeed", ["sh", "-c", 'eval "$1" >/dev/null 2>&1 && echo yes', "sh", command])


def wait(name: str, condition: Condition, timeout: float, interval: float = 1, shot: Path | None = None) -> str:
    """Poll until the probe prints something; returns what it printed."""
    seen = {"out": ""}
    end = time.monotonic() + timeout

    def ready() -> bool:
        try:
            done = tart.guest(name, condition.argv, timeout=max(1.0, min(15.0, end - time.monotonic())))
        except tart.GuestTimeout:
            return False
        seen["out"] = done.stdout.decode(errors="replace").strip() if done.returncode == 0 else ""
        return bool(seen["out"])

    if not poll(ready, timeout, interval=interval):
        raise VmError(f"gave up waiting {timeout:.0f}s for {condition.what}", screenshot=evidence(name, shot))
    return seen["out"]


def wait_session(name: str, label: str, timeout: float, interval: float = 1, shot: Path | None = None) -> None:
    status = wait(name, session_done(label), timeout, interval, shot)
    if status != "0":
        raise VmError(f"session {label} failed with exit status {status}", screenshot=evidence(name, shot))
