"""The guest's screen over VNC: capture frames, click, press keys and type.

VNC input arrives as hardware events, so it reaches system dialogs that the
guest's privacy controls (TCC) keep away from in-guest automation, and frames
need no screen-recording permission in the guest. The password stays in this
module's memory and never reaches output.
"""

from __future__ import annotations

import atexit
import re
import warnings
from dataclasses import dataclass, field
from pathlib import Path

_URL = re.compile(r"vnc://:(?P<password>[^@\s]*)@(?P<host>[\w.\-]+):(?P<port>\d+)")


@dataclass(frozen=True)
class Endpoint:
    host: str
    port: int
    password: str = field(repr=False)

    def __repr__(self) -> str:
        return f"Endpoint({self.host}:{self.port})"


def endpoint_from_log(text: str) -> Endpoint | None:
    """Tart prints `Opening vnc://:PASSWORD@HOST:PORT...` once the VNC server listens."""
    match = _URL.search(text)
    if not match:
        return None
    return Endpoint(match["host"], int(match["port"]), match["password"])


# Characters a US keyboard types with Shift, by the key that carries them.
_SHIFTED = dict(zip('~!@#$%^&*()_+{}|:"<>?', "`1234567890-=[]\\;',./", strict=True))


def keystrokes(text: str) -> list[tuple[bool, str]]:
    """Split text into (shift, key) presses for a US layout; newlines press Return."""
    strokes: list[tuple[bool, str]] = []
    for char in text:
        if char == "\n":
            strokes.append((False, "enter"))
        elif char == "\t":
            strokes.append((False, "tab"))
        elif char == " ":
            strokes.append((False, "space"))
        elif char.isupper():
            strokes.append((True, char.lower()))
        elif char in _SHIFTED:
            strokes.append((True, _SHIFTED[char]))
        else:
            strokes.append((False, char))
    return strokes


class Screen:
    """One VNC connection. Every call fails after `timeout` seconds instead of hanging."""

    def __init__(self, endpoint: Endpoint, timeout: float = 15):
        warnings.filterwarnings("ignore", module="vncdotool")
        from vncdotool import api

        self._api = api
        self._client = api.connect(f"{endpoint.host}::{endpoint.port}", password=endpoint.password, timeout=timeout)

    def frame(self):
        """The whole current screen as a PIL image."""
        self._client.refreshScreen(False)
        return self._client.screen

    def save(self, path: Path) -> Path:
        self.frame().convert("RGB").save(path)
        return path

    def move(self, x: int, y: int) -> None:
        self._client.mouseMove(x, y)

    def click(self, x: int, y: int, button: int = 1) -> None:
        self._client.mouseMove(x, y)
        self._client.mousePress(button)

    def key(self, combo: str) -> None:
        """A key or chord in vncdotool names, such as `enter`, `esc` or `super-q` (Cmd-Q)."""
        self._client.keyPress(combo)

    def type(self, text: str) -> None:
        for shift, key in keystrokes(text):
            if shift:
                self._client.keyDown("shift")
                self._client.keyPress(key)
                self._client.keyUp("shift")
            else:
                self._client.keyPress(key)

    def close(self) -> None:
        # vncdotool runs one Twisted reactor per process, which cannot restart,
        # so it stops at exit rather than with each connection.
        self._client.disconnect()


def _shutdown_reactor() -> None:
    import sys

    api = sys.modules.get("vncdotool.api")
    if api is not None:
        api.shutdown()


atexit.register(_shutdown_reactor)
