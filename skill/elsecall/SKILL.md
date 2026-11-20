---
name: elsecall
description: Run builds, tests and other heavy commands through Elsecall, which admits work when this Mac has room and can place it on another Mac in the fleet, and adopt it in a repository's scripts. Use when a repository's scripts call `elsecall` or source an Elsecall guard, when asked to run work on another fleet member, or when integrating a project with Elsecall.
---

# Elsecall

Elsecall admits heavy work onto this Mac only when it has room, and can place it on another Mac in the same fleet. Run builds, tests and other heavy commands through it when a repository's scripts already do, or when the operator asks.

## Rules

- Every command except `run` and `skill show` prints one JSON document on stdout, failures included. Exit 0 is success, 1 a failed operation, 2 an invalid command. Branch on `error.code`, never on message text, and follow `error.hint`.
- `elsecall run [options] -- COMMAND...` passes the command's output, signals and exit code through. Exit 125 is Elsecall's own failure: the stderr line before the JSON envelope says what happened and what to do. Pass that line on unchanged.
- Never retry a `fleet_busy` or `service_unavailable` failure in a loop. Wait as the failure line says, then run the same command once more.
- A script that re-executes itself through `elsecall run` (its entry line) is admitted as a whole. Call it by its path, as `Scripts/build.sh`, not through `elsecall run` and not as `sh Scripts/build.sh`.
- `ELSECALL_ADMISSION` in a command's environment says how it was admitted. Never set it yourself.
- `elsecall run --member NAME -- COMMAND...` runs only on that member, never here in its place. From a git working tree it runs in the member's copy of the tree; from outside one, in the member's home directory with nothing synced, as for `elsecall run --member mac-mini -- sw_vers`.
- `elsecall fleet` lists what needs attention in `data.attention`. Each item's `actions` give an `elsecall` command and the member to run it on (`run_on`); leave an action with `person_only: true` to the operator unless they ask you to start it.
- `elsecall runs` shows what is running and queued here; `elsecall explain RUN_ID` shows why a request waited, ran where it did, or failed.
- `ELSECALL=off` runs everything directly, without admission. Use it only when the operator asks, or to test a script without the service.
- Install, update or uninstall Elsecall only when the operator asks: other projects on this machine depend on its installation.

## Topics

`elsecall skill show NAME` prints a topic (in an exported copy of this skill, `references/NAME.md`):

- `integrate`: adopt Elsecall in a repository's scripts (the guard, entry lines, per-command prefixes, leases), failure lines, environment controls, precedence, run IDs, and outputs a placed run brings back.
