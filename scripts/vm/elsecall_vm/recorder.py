"""Recording the guest's screen over VNC into an MP4, in real time.

Frames stream straight into ffmpeg at a fixed rate. When a capture takes
longer than one frame interval, the last frame repeats, so the video keeps
wall-clock timing however fast VNC delivers.
"""

from __future__ import annotations

import math
import shutil
import signal
import subprocess
import time
from pathlib import Path

from .vnc import Endpoint, Screen

WIDTH = 1280


def frames_due(elapsed: float, fps: int, written: int) -> int:
    """How many frames to write now so that `written` covers `elapsed` seconds."""
    return max(0, math.floor(elapsed * fps) + 1 - written)


def encoder(out: Path, size: tuple[int, int], fps: int) -> list[str]:
    return [
        _ffmpeg(), "-loglevel", "error", "-y",
        "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", f"{size[0]}x{size[1]}", "-framerate", str(fps), "-i", "-",
        "-vf", f"scale={WIDTH}:-2", "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p",
        "-crf", "23", "-movflags", "+faststart", str(out),
    ]


def _ffmpeg() -> str:
    found = shutil.which("ffmpeg")
    if not found:
        raise RuntimeError("ffmpeg is not on PATH")
    return found


def record(endpoint: Endpoint, out: Path, fps: int, max_seconds: float) -> int:
    """Record until SIGTERM/SIGINT or `max_seconds`; returns the number of frames written."""
    stopping = False

    def stop(*_):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    screen = Screen(endpoint)
    first = screen.frame().convert("RGB")
    size = first.size
    ffmpeg = subprocess.Popen(encoder(out, size, fps), stdin=subprocess.PIPE)
    written = 0
    captured = 1
    start = time.monotonic()
    frame = first
    try:
        while not stopping:
            elapsed = time.monotonic() - start
            if elapsed >= max_seconds:
                break
            data = frame.tobytes()
            for _ in range(frames_due(elapsed, fps, written)):
                ffmpeg.stdin.write(data)
                written += 1
            image = screen.frame().convert("RGB")
            captured += 1
            frame = image if image.size == size else image.resize(size)
    finally:
        ffmpeg.stdin.close()
        try:
            ffmpeg.wait(timeout=150)
        except subprocess.TimeoutExpired:
            ffmpeg.kill()
            raise
        screen.close()
    seconds = time.monotonic() - start
    print(f"captured {captured} frames in {seconds:.1f}s ({captured / max(seconds, 0.1):.1f}/s); wrote {written}")
    contact_sheet(out, written / fps)
    return written


def contact_sheet(video: Path, seconds: float) -> Path:
    """Nine evenly spaced frames in one PNG next to the video, for a quick review."""
    sheet = video.with_name(video.stem + "-contact.png")
    every = max(seconds / 9, 0.5)
    subprocess.run(
        [_ffmpeg(), "-loglevel", "error", "-y", "-i", str(video),
         "-vf", f"fps=1/{every:.2f},scale=640:-2,tile=3x3", "-frames:v", "1", str(sheet)],
        check=True,
        timeout=60,
    )
    return sheet
