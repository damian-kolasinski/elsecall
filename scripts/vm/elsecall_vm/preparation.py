"""Turning a base image into a quiet, ready one, and checking that clones stay quiet."""

from __future__ import annotations

import sys
import time

from . import image, lifecycle, registry, tart
from .deadline import poll
from .errors import VmError
from .guest import evidence, run, screen, screenshot


def apple_event_returns(name: str, script: str, timeout: float) -> bool:
    event = tart.guest_async(name, ["osascript", "-e", script])
    if poll(lambda: event.poll() is not None, timeout, interval=0.25):
        if event.returncode != 0:
            raise VmError(f"Apple Event failed: {event.stderr.read().decode(errors='replace').strip()}")
        return True
    event.kill()
    return False


def approve_automation(name: str, app: str, script: str) -> None:
    """Send one Apple Event; if macOS asks whether the guest agent may, answer Allow with Return."""
    event = tart.guest_async(name, ["osascript", "-e", script])
    if not poll(lambda: event.poll() is not None, 4, interval=0.25):
        # The prompt can take a few seconds to appear. Return presses its
        # default button, Allow; a press that lands before it does nothing harmful.
        with screen(name) as connection:
            for _ in range(4):
                connection.key("enter")
                if poll(lambda: event.poll() is not None, 4, interval=0.25):
                    break
    if event.poll() is None:
        event.kill()
        raise VmError(
            f"the prompt letting the guest agent control {app} did not accept Return; "
            "ask the operator to click Allow in the guest",
            screenshot=evidence(name),
            needs_human=True,
        )
    if event.returncode != 0:
        raise VmError(f"Apple Event to {app} failed: {event.stderr.read().decode(errors='replace').strip()}")


def shutdown(name: str) -> None:
    """Shut down through System Events, which logs out first; fail with a screenshot if anything holds it up."""
    said = ""
    try:
        # The guest agent's connection can drop as the guest goes down, so the
        # exit status says little; whether the VM stops says it all.
        done = tart.guest(name, ["osascript", "-e", image.SHUT_DOWN], timeout=60)
        said = (done.stdout + done.stderr).decode(errors="replace").strip()
    except tart.GuestTimeout:
        pass
    if not tart.wait_stopped(name, 180):
        raise VmError(
            f"{name} did not shut down within 180s {said}".strip(), screenshot=evidence(name), needs_human=True
        )
    registry.run_log(name).unlink(missing_ok=True)


def prepare(source: str, name: str) -> None:
    if tart.find(name) is not None:
        raise VmError(f"{name} already exists; pick another name or delete it first")
    lifecycle.up(name, source)
    say("quieting notifications and window restoration")
    run(name, image.QUIET, 60)
    for app, script in image.AUTOMATED_APPS.items():
        say(f"approving Apple Events to {app}")
        approve_automation(name, app, script)
    say("setting a readable Terminal profile")
    run(name, 'osascript -e "$1"', 30, args=(image.TERMINAL_PROFILE,))
    run(name, 'osascript -e "$1"', 30, args=(image.QUIT_TERMINAL,))
    if not poll(lambda: not tart.guest_ok(name, ["pgrep", "-x", "Terminal"]), 20):
        raise VmError("Terminal did not quit", screenshot=evidence(name), needs_human=True)
    run(name, image.CLEAR_BANNERS, 30)
    say("shutting down")
    shutdown(name)
    registry.mark_image(name)
    say(f"{name} is ready; check it with `ecvm image check {name}`")


def check(image_name: str, keep: bool) -> None:
    """Boot a throwaway clone and confirm it starts quiet; print a screenshot of its desktop."""
    probe = f"{image_name}-check"
    if tart.find(probe) is not None:
        lifecycle.remove(probe)
    try:
        lifecycle.up(probe, image_name)
        time.sleep(15)
        problems = [line for line in run(probe, image.CHECK, 60).splitlines() if line.strip()]
        # Without approving anything: a prompt here means the image lost its approval.
        if not apple_event_returns(probe, image.AUTOMATED_APPS["Terminal"], 10):
            problems.append("Apple Events to Terminal wait for a prompt")
        shot = screenshot(probe, registry.vm_dir(image_name) / "check.png")
        print(f"desktop: {shot}")
        if problems:
            raise VmError(f"{image_name} is not quiet: {'; '.join(problems)}", screenshot=shot)
        print(f"{image_name} boots without dialogs; Apple Events to Terminal work")
    finally:
        if not keep and tart.find(probe) is not None:
            try:
                lifecycle.remove(probe)
            except VmError as error:
                say(f"could not delete {probe}: {error}")


def say(text: str) -> None:
    print(text, file=sys.stderr)
