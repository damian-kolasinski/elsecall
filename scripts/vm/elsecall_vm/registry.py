"""Private per-VM state on the host, and which VMs this tool owns.

Every VM this tool clones gets a directory under the state root holding an
ownership marker, the `tart run` log (which carries the VNC password, so the
directory is private to the user) and recorder bookkeeping. The marker also
records the VM's MAC address, which Tart picks at random for each clone, so a
marker left behind by a deleted VM never vouches for a newer one of that name.
"""

from __future__ import annotations

import json
import os
import re
import time
from dataclasses import dataclass
from enum import Enum
from pathlib import Path


_NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,62}$")


def check_name(name: str) -> str:
    if not _NAME.match(name) or ".." in name:
        raise ValueError(f"VM name {name!r} must be letters, digits, dots, dashes and underscores")
    return name


class Kind(Enum):
    VM = "vm"
    IMAGE = "image"


@dataclass(frozen=True)
class Owned:
    name: str
    kind: Kind
    source: str
    created: float
    mac: str


def root() -> Path:
    base = os.environ.get("ECVM_HOME") or str(Path.home() / "Library/Caches/elsecall-vm")
    path = Path(base)
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    return path


def vm_dir(name: str) -> Path:
    path = root() / "vms" / check_name(name)
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(path, 0o700)
    return path


def run_log(name: str) -> Path:
    return vm_dir(name) / "tart-run.log"


def claim(name: str, kind: Kind, source: str, mac: str) -> Owned:
    owned = Owned(name, kind, source, time.time(), mac)
    (vm_dir(name) / "owned.json").write_text(
        json.dumps({"kind": kind.value, "source": source, "created": owned.created, "mac": mac})
    )
    return owned


def owned(name: str) -> Owned | None:
    marker = root() / "vms" / check_name(name) / "owned.json"
    if not marker.exists():
        return None
    data = json.loads(marker.read_text())
    return Owned(name, Kind(data["kind"]), data["source"], data["created"], data.get("mac", ""))


def mark_image(name: str) -> None:
    current = owned(name)
    assert current is not None
    claim(name, Kind.IMAGE, current.source, current.mac)


def forget(name: str) -> None:
    directory = root() / "vms" / check_name(name)
    if directory.exists():
        for child in sorted(directory.rglob("*"), reverse=True):
            child.rmdir() if child.is_dir() else child.unlink()
        directory.rmdir()
