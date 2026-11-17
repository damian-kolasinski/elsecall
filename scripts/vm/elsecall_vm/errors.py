"""Failures the operator or agent must act on, each with a plain next step."""

from __future__ import annotations

import re
from pathlib import Path

_VNC_SECRET = re.compile(r"(vnc://[^:@/\s]*:)[^@\s]*@")


def redact(text: str) -> str:
    """Remove VNC passwords from text that came from Tart."""
    return _VNC_SECRET.sub(r"\1***@", text)


class VmError(Exception):
    """An expected failure. `screenshot` names a capture taken at the moment it failed."""

    def __init__(self, message: str, *, screenshot: Path | None = None, needs_human: bool = False):
        super().__init__(redact(message))
        self.screenshot = screenshot
        self.needs_human = needs_human
