#!/usr/bin/env python3
"""Run test processes without the enclosing consumer's Elsecall controls."""
import os
import sys

UI_GOLDENS = {"ELSECALL_UI_SKIP_GOLDENS", "ELSECALL_UI_BLESS"}

os.execvpe(
    sys.argv[1],
    sys.argv[1:],
    {
        k: v
        for k, v in os.environ.items()
        if k in UI_GOLDENS or (k != "ELSECALL" and not k.startswith("ELSECALL_"))
    },
)
