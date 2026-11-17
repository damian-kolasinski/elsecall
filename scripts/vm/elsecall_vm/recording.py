"""Starting and stopping the background recorder for a VM.

The recorder runs as its own process. Its PID is only trusted while that
process's command line still names this VM's recording, so a reused PID never
receives a signal meant for a finished recorder.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
import os
import signal

from . import registry
from .deadline import poll
from .errors import VmError
from .guest import endpoint, tail


def _state(name: str) -> Path:
    return registry.vm_dir(name) / "recording.json"


def _marker(name: str) -> str:
    return f"elsecall_vm _record {name} "


def _ours(pid: int, name: str) -> bool:
    done = subprocess.run(["ps", "-p", str(pid), "-o", "command="], capture_output=True, text=True)
    return done.returncode == 0 and _marker(name) in done.stdout + " "


def start(name: str, out: Path, fps: int, max_seconds: float) -> None:
    endpoint(name)
    state = _state(name)
    if state.exists() and _ours(json.loads(state.read_text())["pid"], name):
        raise VmError(f"{name} is already being recorded; run `ecvm record stop {name}` first")
    out = out.resolve()
    out.parent.mkdir(parents=True, exist_ok=True)
    log = registry.vm_dir(name) / "recorder.log"
    with log.open("w") as sink:
        process = subprocess.Popen(
            [sys.executable, "-m", "elsecall_vm", "_record", name, str(out), str(fps), str(max_seconds)],
            stdin=subprocess.DEVNULL, stdout=sink, stderr=subprocess.STDOUT, start_new_session=True,
        )
    state.write_text(json.dumps({"pid": process.pid, "out": str(out)}))
    if poll(lambda: process.poll() is not None, 3, interval=0.5):
        raise VmError(f"the recorder exited at once: {tail(log.read_text())}")
    print(f"recording {name} to {out}", file=sys.stderr)


def stop(name: str, quiet: bool = False) -> tuple[Path, Path] | None:
    """Stop the recorder and wait for it to finish encoding; returns the video and its contact sheet."""
    state = _state(name)
    if not state.exists():
        if quiet:
            return None
        raise VmError(f"{name} is not being recorded")
    data = json.loads(state.read_text())
    pid = data["pid"]
    if _ours(pid, name):
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass  # It finished on its own in the meantime.
        if not poll(lambda: not _ours(pid, name), 180, interval=0.5):
            raise VmError(f"the recorder for {name} did not finish encoding within 180s")
    state.unlink()
    out = Path(data["out"])
    if not out.exists():
        if quiet:
            return None
        log = registry.vm_dir(name) / "recorder.log"
        raise VmError(f"no video was written: {tail(log.read_text())}")
    return out, out.with_name(out.stem + "-contact.png")
