"""Cloning, booting, stopping and deleting the VMs this tool owns.

Ownership is checked against Tart on every call: a VM counts as ours only when
its marker exists and its MAC address still matches the one recorded at clone
time. Prepared images are never booted or deleted from here, so they stay
exactly as `image prepare` left them.
"""

from __future__ import annotations

import fcntl
import sys
import time
from contextlib import contextmanager
from typing import Iterator

from . import recording, registry, tart
from .deadline import poll
from .errors import VmError
from .guest import evidence, tail
from .vnc import endpoint_from_log

BOOT_SECONDS = 240


def require_owned(name: str) -> registry.Owned:
    owned = registry.owned(name)
    if owned is None:
        raise VmError(f"{name} was not created by ecvm; it is left alone")
    vm = tart.find(name)
    if vm is not None and tart.mac(name) != owned.mac:
        raise VmError(f"{name} is not the VM ecvm created under that name; it is left alone")
    return owned


def require_vm(name: str) -> registry.Owned:
    """An owned VM that is not a prepared image."""
    owned = require_owned(name)
    if owned.kind is registry.Kind.IMAGE:
        raise VmError(f"{name} is a prepared image; clone it with `ecvm up NEW --from {name}` instead")
    return owned


@contextmanager
def slot(name: str) -> Iterator[None]:
    """Hold the host's VM slots while checking for a free one and starting `name` in it.

    macOS runs at most two guests; the lock keeps two `ecvm up` calls from
    both taking the last slot. VMs started by other tools still count.
    """
    with (registry.root() / "slots.lock").open("w") as lock:

        def locked() -> bool:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return False
            return True

        if not poll(locked, 300, interval=1):
            raise VmError("another `ecvm up` has held the VM slots for 5 minutes", needs_human=True)
        others = [vm for vm in tart.running() if vm != name]
        if len(others) >= tart.MAX_RUNNING:
            raise VmError(
                f"{len(others)} VMs are already running ({', '.join(others)}) and macOS allows "
                f"{tart.MAX_RUNNING}: ask the operator to stop one, or wait for one to stop",
                needs_human=True,
            )
        yield


def boot(name: str) -> None:
    log = registry.run_log(name)
    started = time.monotonic()
    with slot(name):
        # Checked under the lock: another `up` of this VM may have just started it,
        # and starting it again would overwrite its log and VNC address.
        vm = tart.find(name)
        if vm is not None and vm.running:
            if endpoint_from_log(log.read_text() if log.exists() else "") is not None:
                return
            raise VmError(f"{name} runs without ecvm's VNC server; stop it and run `ecvm up {name}` again")
        process = tart.start(name, log)
        if not poll(lambda: process.poll() is not None or bool(tart.find(name) and tart.find(name).running), 60):
            raise VmError(f"tart run {name} did not start within 60s")

    def desktop_ready() -> bool:
        if process.poll() is not None:
            raise VmError(f"tart run {name} exited {process.returncode}: {tail(log.read_text())}")
        if endpoint_from_log(log.read_text()) is None:
            return False
        return tart.guest_ok(name, ["pgrep", "-x", "Dock"], timeout=10)

    if not poll(desktop_ready, BOOT_SECONDS, interval=2):
        raise VmError(f"{name} did not reach the desktop within {BOOT_SECONDS}s", screenshot=evidence(name))
    # Let login items and the menu bar settle before anyone looks.
    time.sleep(5)
    print(f"{name} is up after {time.monotonic() - started:.0f}s", file=sys.stderr)


def up(name: str, source: str | None, display: str | None = None) -> None:
    registry.check_name(name)
    if tart.find(name) is None:
        if source is None:
            raise VmError(f"{name} does not exist; pass --from IMAGE to clone it")
        if tart.find(source) is None:
            raise VmError(f"image {source} does not exist; see docs/e2e-vm.md to prepare one")
        tart.clone(source, name)
        registry.claim(name, registry.Kind.VM, source, tart.mac(name))
        if display:
            tart.set_display(name, display)
    else:
        require_vm(name)
    boot(name)


def down(name: str) -> None:
    require_owned(name)
    recording.stop(name, quiet=True)
    vm = tart.find(name)
    if vm is not None and vm.running:
        tart.stop(name)
    registry.run_log(name).unlink(missing_ok=True)


def remove(name: str) -> None:
    require_vm(name)
    down(name)
    if tart.find(name) is not None:
        tart.delete(name)
    registry.forget(name)
