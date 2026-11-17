"""Polling a condition against a deadline, so no wait outlives its budget."""

from __future__ import annotations

import time
from typing import Callable


def poll(condition: Callable[[], bool], timeout: float, interval: float = 1.0,
         clock: Callable[[], float] = time.monotonic, sleep: Callable[[float], None] = time.sleep) -> bool:
    """True as soon as `condition` holds; False once `timeout` seconds pass without it.

    The condition is checked at least once, and never after the deadline.
    """
    end = clock() + timeout
    while True:
        if condition():
            return True
        left = end - clock()
        if left <= 0:
            return False
        sleep(min(interval, left))
