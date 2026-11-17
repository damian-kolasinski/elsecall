"""Visible Terminal sessions in the guest, started without Apple Events.

A `.command` file opened with `open` runs in a new Terminal window, so no
automation permission is involved. The session shows each command as if typed,
prints `#` lines as narration, stops at the first failing command, records the
exit status for `wait --session`, and ends at a live prompt.
"""

from __future__ import annotations

import re

_LABEL = re.compile(r"^[a-z0-9][a-z0-9-]{0,40}$")
GUEST_DIR = "~/ecvm"


def check_label(label: str) -> str:
    if not _LABEL.match(label):
        raise ValueError(f"session label {label!r} must be lowercase letters, digits and dashes")
    return label


def command_path(label: str) -> str:
    return f"{GUEST_DIR}/{label}.command"


def status_path(label: str) -> str:
    return f"{GUEST_DIR}/{label}.status"


def _quote(text: str) -> str:
    return "'" + text.replace("'", "'\\''") + "'"


def session_script(label: str, body: str, type_delay_cs: int = 2) -> str:
    """The `.command` file for `body`: one shell command per line, `#` lines are narration."""
    check_label(label)
    lines = [
        "#!/bin/zsh -il",
        "# Written by scripts/vm/ecvm: a visible demo session.",
        "setopt pipefail",
        # A builtin delay, so the window title never shows a `sleep` process.
        "zmodload zsh/zselect",
        f"ecvm_status={status_path(label)}",
        'mkdir -p "${ecvm_status:h}"',
        'rm -f "$ecvm_status"',
        f"printf '\\e]0;%s\\a' {_quote(label)}",
        "clear",
        "ecvm_say() { printf '\\e[2m# %s\\e[0m\\n' \"$1\"; }",
        "ecvm_show() {",
        "  printf '\\e[1;32m%%\\e[0m \\e[1m'",
        "  local i; for (( i = 1; i <= ${#1}; i++ )); do printf '%s' \"${1[i]}\"; "
        f"zselect -t {type_delay_cs}; done",
        "  printf '\\e[0m\\n'",
        "}",
        "ecvm_check() {",
        '  if [ "$1" -ne 0 ]; then',
        "    printf '\\e[1;31mexit status %s\\e[0m\\n' \"$1\"",
        '    print "$1" > "$ecvm_status"',
        "    exec zsh -il",
        "  fi",
        "}",
    ]
    for raw in body.splitlines():
        line = raw.strip()
        if not line:
            lines.append("echo")
        elif line.startswith("#!"):
            continue
        elif line.startswith("#"):
            lines.append(f"ecvm_say {_quote(line.lstrip('#').strip())}")
        else:
            lines += [f"ecvm_show {_quote(line)}", line, "ecvm_check $?"]
    lines += ['print 0 > "$ecvm_status"', "exec zsh -il", ""]
    return "\n".join(lines)
