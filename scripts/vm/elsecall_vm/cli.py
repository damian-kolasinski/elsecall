"""ecvm: macOS guests for Elsecall's end-to-end tests and demos.

Every step has a deadline. A step that runs out of time captures the guest's
screen, prints where the capture is, and exits non-zero, so the caller can look
at what is in the way, and ask a human when it needs one, instead of waiting.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from . import image, lifecycle, preparation, recorder, recording, registry, tart, terminal, waits
from .errors import VmError
from .guest import PATH_ARG, PATH_ARG2, endpoint, evidence, pull, push, run, screen, screenshot


def parser() -> argparse.ArgumentParser:
    top = argparse.ArgumentParser(prog="ecvm", description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = top.add_subparsers(dest="command", required=True, metavar="COMMAND")

    img = sub.add_parser("image", help="prepare or check a quiet image").add_subparsers(dest="action", required=True)
    p = img.add_parser("prepare", help="clone SOURCE into NAME, approve prompts once, quiet banners, shut down")
    p.add_argument("--from", dest="source", default="ec-base")
    p.add_argument("name", nargs="?", default="ec-ready")
    p = img.add_parser("check", help="boot a throwaway clone and confirm it starts without dialogs")
    p.add_argument("name", nargs="?", default="ec-ready")
    p.add_argument("--keep", action="store_true", help="leave the clone running for a look")

    p = sub.add_parser("up", help="clone NAME from an image if needed, then boot it to the desktop")
    p.add_argument("name")
    p.add_argument("--from", dest="source", help="image to clone, such as ec-ready")
    p.add_argument("--fresh", action="store_true", help="remove mise after booting, for a Mac that never had it")
    p.add_argument("--display", help="display size for a new clone, such as 1280x800")
    p.add_argument("--hostname", help="name the Mac, such as mac-mini; prompts and Elsecall show it")
    for verb, text in (("down", "shut a VM down"), ("rm", "shut a VM down and delete it")):
        sub.add_parser(verb, help=text).add_argument("name")
    sub.add_parser("ls", help="list the VMs ecvm created")
    p = sub.add_parser("fresh", help="remove mise from a running VM")
    p.add_argument("name")

    p = sub.add_parser("exec", help="run a command in the guest with a deadline")
    p.add_argument("name")
    p.add_argument("--timeout", type=float, default=120)
    p.add_argument("argv", nargs="+", help="the command, after --")
    p = sub.add_parser("push", help="copy a host file into the guest")
    p.add_argument("name")
    p.add_argument("source", type=Path)
    p.add_argument("target", help="guest path; ~/ means the guest user's home")
    p.add_argument("--mode", default="644")
    p = sub.add_parser("pull", help="copy a guest file to the host")
    p.add_argument("name")
    p.add_argument("source")
    p.add_argument("target", type=Path)

    p = sub.add_parser("term", help="run a script visibly in a new guest Terminal window")
    p.add_argument("name")
    p.add_argument("script", type=Path, help="one command per line; # lines are narration")
    p.add_argument("--label", default="session")

    p = sub.add_parser("shot", help="capture the guest screen to a PNG")
    p.add_argument("name")
    p.add_argument("out", type=Path, nargs="?")
    p = sub.add_parser("click", help="click at framebuffer pixel X Y, as in a screenshot")
    p.add_argument("name")
    p.add_argument("x", type=int)
    p.add_argument("y", type=int)
    p.add_argument("--button", type=int, default=1)
    p = sub.add_parser("key", help="press keys, such as enter, esc or super-q (Cmd-Q)")
    p.add_argument("name")
    p.add_argument("keys", nargs="+")
    p = sub.add_parser("type", help="type text; \\n presses Return")
    p.add_argument("name")
    p.add_argument("text")

    rec = sub.add_parser("record", help="record the guest screen to MP4").add_subparsers(dest="action", required=True)
    p = rec.add_parser("start")
    p.add_argument("name")
    p.add_argument("out", type=Path)
    p.add_argument("--fps", type=int, default=10)
    p.add_argument("--max-seconds", type=float, default=900)
    rec.add_parser("stop").add_argument("name")

    p = sub.add_parser("wait", help="wait for a guest condition; on timeout, fail with a screenshot")
    p.add_argument("name")
    what = p.add_mutually_exclusive_group(required=True)
    what.add_argument("--session", help="a `term` session label: wait for it to finish, fail if it failed")
    what.add_argument("--process", help="pgrep -f pattern")
    what.add_argument("--file", help="guest path")
    what.add_argument("--shell", help="guest shell condition")
    p.add_argument("--timeout", type=float, default=60)
    p.add_argument("--interval", type=float, default=1)
    p.add_argument("--shot", type=Path, help="where to save the failure screenshot")

    p = sub.add_parser("_record")
    for arg in ("name", "out", "fps", "max_seconds"):
        p.add_argument(arg)
    return top


def run_command(args: argparse.Namespace) -> int:
    match args.command:
        case "image" if args.action == "prepare":
            preparation.prepare(args.source, args.name)
        case "image":
            preparation.check(args.name, args.keep)
        case "up":
            if args.hostname:
                image.rename(args.hostname)
            lifecycle.up(args.name, args.source, args.display)
            if args.fresh:
                run(args.name, image.FRESH, 180)
            if args.hostname:
                run(args.name, image.rename(args.hostname), 30)
        case "down":
            lifecycle.down(args.name)
        case "rm":
            lifecycle.remove(args.name)
        case "ls":
            states = {vm.name: vm.running for vm in tart.listed()}
            for directory in sorted((registry.root() / "vms").glob("*")):
                try:
                    owned = registry.owned(directory.name)
                except ValueError:
                    continue
                if owned and directory.name in states:
                    state = "running" if states[directory.name] else "stopped"
                    print(f"{owned.name}\t{owned.kind.value}\t{state}\tfrom {owned.source}")
        case "_record":
            recorder.record(endpoint(args.name), Path(args.out), int(args.fps), float(args.max_seconds))
        case _:
            lifecycle.require_vm(args.name)
            return guest_command(args)
    return 0


def guest_command(args: argparse.Namespace) -> int:
    """Commands on a running VM this tool owns."""
    match args.command:
        case "fresh":
            run(args.name, image.FRESH, 180)
        case "exec":
            try:
                done = tart.guest(args.name, args.argv, timeout=args.timeout, capture=False)
            except tart.GuestTimeout as error:
                raise VmError(
                    f"guest command did not finish within {args.timeout:.0f}s: {error}",
                    screenshot=evidence(args.name),
                ) from None
            return done.returncode
        case "push":
            push(args.name, args.source.read_bytes(), args.target, args.mode)
        case "pull":
            args.target.write_bytes(pull(args.name, args.source))
        case "term":
            label = terminal.check_label(args.label)
            push(args.name, terminal.session_script(label, args.script.read_text()).encode(),
                 terminal.command_path(label), "755")
            # Clear the last run's status in the same step, so a wait cannot read it.
            run(args.name, PATH_ARG + PATH_ARG2 + 'rm -f "$q"; open "$p"', 30,
                args=(terminal.command_path(label), terminal.status_path(label)))
        case "shot":
            print(screenshot(args.name, args.out))
        case "click":
            with screen(args.name) as connection:
                connection.click(args.x, args.y, args.button)
        case "key":
            with screen(args.name) as connection:
                for key in args.keys:
                    connection.key(key)
        case "type":
            with screen(args.name) as connection:
                connection.type(args.text.replace("\\n", "\n"))
        case "record" if args.action == "start":
            recording.start(args.name, args.out, args.fps, args.max_seconds)
        case "record":
            for path in recording.stop(args.name):
                print(path)
        case "wait":
            if args.session:
                waits.wait_session(args.name, args.session, args.timeout, args.interval, args.shot)
            else:
                condition = (
                    waits.process(args.process) if args.process
                    else waits.file(args.file) if args.file
                    else waits.shell(args.shell)
                )
                waits.wait(args.name, condition, args.timeout, args.interval, args.shot)
    return 0


def main() -> int:
    try:
        return run_command(parser().parse_args())
    except ValueError as error:
        print(f"ecvm: {error}", file=sys.stderr)
        return 2
    except OSError as error:
        print(f"ecvm: {error}", file=sys.stderr)
        return 1
    except VmError as error:
        print(f"ecvm: {error}", file=sys.stderr)
        if error.screenshot:
            print(f"ecvm: screenshot: {error.screenshot}", file=sys.stderr)
        if error.needs_human:
            print("ecvm: this needs the operator", file=sys.stderr)
        return 1
