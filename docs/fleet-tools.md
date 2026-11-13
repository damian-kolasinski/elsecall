# Fleet tool management

Elsecall keeps the development tools on a fleet at the versions the operator asked
for. This document fixes the vocabulary and the rules. Slices implement them one
at a time. A change to anything here is a product decision and updates this
document first.

## Vocabulary

- **Tool manager.** Software Elsecall drives to install tools. The managers are
  `mise` and `xcodes`.
- **Tool.** Something a manager installs, named within that manager. A mise tool
  uses mise's own identifier, exactly as `mise ls` reports it (`node`, `tuist`,
  `packslip:example/acme`). Xcode is the single tool `xcode`.
- **Installed version.** An exact version present on disk.
- **Default.** The version used when no project selects one. For mise, that is
  the version selected by mise's global config. For Xcode, it is the app
  `xcode-select -p` points at.

## Desired state

The master holds `fleet-tools.toml` beside `fleet-config.toml`. It names exact
versions only. A missing file means nothing is desired. `[defaults]` applies to
every member. `[members.<name>]` replaces it for one member: a manager `version`
replaces the default executable, and a tool entry replaces that tool's whole
default (install lists are not merged). An empty tool entry means nothing is
wanted for that tool on that member. The format, the CLI, and the rules are in
[Slice 2b-1](#slice-2b-1-desired-state).

## Plan and apply

Changing what is installed is explicit. A plan shows what would change. Apply
does it. Nothing is installed, selected or removed as a side effect of reading
the fleet.

A version that is installed but not in the desired set is reported. Elsecall
does not uninstall it.

## Xcode

The master downloads each Xcode archive once and streams it to members. Steps
that need root (placing the app, selecting it, accepting the license) go through
a narrowly scoped sudoers drop-in. `elsecall fleet add` installs that drop-in.
It does not grant a general root shell.

## Joining a machine

`elsecall fleet add <ssh-host>` is how a machine joins: Elsecall is installed
there, the machine is enrolled, and the sudoers drop-in is installed.

## Interface

Agents use the CLI. A native rune app talks to the same local socket. Both see
the same inventory and, in later slices, the same plan.

Homebrew, Mint and simulator runtimes are out of scope.

## Slice 1: inventory

This slice only reads the local machine. It does not install, select, uninstall,
read `fleet-tools.toml`, call another member, or open a UI.

`elsecall tools inventory` prints the standard JSON envelope from
`GET /v1/tools/inventory`. The listener's same-user rule authorizes it, as for
the other local routes. Exit 0 when the service answers, including when one
manager failed: that failure is a `state: "failed"` entry with `error.code`,
`error.message` and `error.hint`. Exit 1 only when the service call itself
fails. The command waits 150 seconds. Other local commands still wait 3.

The `data` object is `managers`, mise first, then xcodes. A read entry has
`manager`, `state: "read"`, `executable` and `tools`. `executable.state` is
`found` (`path`, `version`) or `missing`. Each tool has `name`, `version` and
`default`. An Xcode tool also has `build` and `path` (the `.app` bundle). Tools
are ordered by name, then by version with the newest last, then by bundle path.
Version order splits on `.` and `-`, compares a numeric segment as a number and
any other segment as a string, and ranks a numeric segment before a non-numeric
one. A version that matches another and then continues is newer, so `1.2.13`
comes before `1.3.5`, which comes before `1.3.14`, and `26.0` comes before
`26.0.1`. At most one version of a tool is `default`. A failed entry has
`manager`, `state: "failed"` and `error`, and replaces that manager's read
entry.

A path override is honoured in every build, the same way `ELSECALL_MACHINE_LOAD`
is. An empty value is unset. A non-empty value that is not an absolute path
fails that manager with `tool_manager_failed` and the hint
`ELSECALL_<NAME> must be an absolute path` — `ELSECALL_MISE`, `ELSECALL_XCODES`,
`ELSECALL_XCODE_APPLICATIONS` or `ELSECALL_XCODE_SELECT`. Elsecall passes that
absolute path to the command. For `ELSECALL_MISE` and `ELSECALL_XCODES`, a
non-empty absolute path that is not a file hides the built-in locations and the
executable is `missing`. That is not a failure. Installed Xcodes are still
listed when `xcodes` itself is missing.

**mise.** When `ELSECALL_MISE` is unset, the executable is the first existing of
`~/.local/bin/mise`, `/opt/homebrew/bin/mise` and `/usr/local/bin/mise`. `HOME`
is checked before that choice: if it is unset the manager fails, whether or not
mise is installed. Otherwise Elsecall runs the absolute path, in the home
directory, with stdin closed and with `MISE` and `MISE_*` removed from the
child's environment:

- `mise --version` — the version is the first whitespace-separated token
  (`2026.10.11 macos-arm64 (2026-10-15)` → `2026.10.11`).
- `mise ls --json` — an object keyed by tool identifier. A record with
  `installed: true` is an installed version; `version` is the exact version.
  `symlinked_to` still counts (`rust` 1.99.0 on this machine). `installed:
  false` is configured but not present and is left out.
- `mise ls --global --json` — the same shape, limited to the global config. The
  default is the record with `installed: true` and `active: true`. That `version`
  is the resolved install, not `requested_version`.

Each command has 30 seconds. Output is not logged. A non-zero exit is
`tool_manager_failed`, a timeout is `tool_manager_timeout`, and unreadable
output (including two defaults for one tool) is `tool_manager_output_invalid`.

**xcodes.** Installed Xcodes are `.app` bundles directly in `/Applications`
(or the absolute `ELSECALL_XCODE_APPLICATIONS`) whose `Contents/Info.plist`
identifier is `com.apple.dt.Xcode`, including a symlink to such a bundle. The
app and a symlink to it are one install, reported at the path that is not a
symlink. The version is `CFBundleShortVersionString`. The build is
`ProductBuildVersion` in `Contents/version.plist`. A bundle with unreadable
plists, or a plist larger than 1 MiB, is skipped. The default is the bundle
whose `Contents/Developer` equals `/usr/bin/xcode-select -p` (an absolute
`ELSECALL_XCODE_SELECT` replaces that executable). If that command fails, times
out, or points at no bundle — Command Line Tools included — no Xcode is
default. When `ELSECALL_XCODES` is unset, the executable is the first existing
of `/opt/homebrew/bin/xcodes`, `/usr/local/bin/xcodes` and
`~/.local/share/mise/shims/xcodes`. Its version is the first token of
`xcodes version`. Installed Xcodes are listed when that executable is missing.
If the executable is present but `xcodes version` fails or times out, the
report is the failure and the scanned apps are not included: a manager is
either read or failed.

The managers are read off the async worker. One manager failing does not hide
the other.

## Slice 2a: fleet inventory

Any member can read every member's inventory. This slice still does not install,
select, uninstall, read `fleet-tools.toml`, or open a UI.

`GET /peer/v1/tools/inventory` returns this member's inventory: the same `data`
document as `GET /v1/tools/inventory` (`managers`, mise then xcodes, in that
order; an Xcode `path` is absolute). It is a signed peer request. Any enrolled
member may call it, the same rule as the report routes. A member on another
build is refused with `version_skew`, the same way a report route is: a request
that names no build is not refused, and a named build that differs from this
member is HTTP 409 with that code. The read runs off the async worker. Only one
read of this machine is in flight: a caller that disconnects does not start
another, and the result is not kept after the read finishes.

The member listener bounds an ordinary HTTP exchange at 30 seconds. Placement
attach, configuration sync and upgrade attach leave that exchange at 101 and
continue on the upgraded socket, outside the bound. Tool inventory is not an
upgrade. After the request is authenticated, that route may run for the member
budget plus 10 seconds. An unauthenticated connection still has 5 seconds to
send its headers and 5 seconds to send its body.

A document larger than 512 KiB is not sent. The member answers with a signed
`inventory_too_large` and the hint to prune unused versions with mise. The
caller keeps that code. The member channel is 1 MiB, so an oversized body would
otherwise arrive unsigned.

`GET /v1/tools/fleet-inventory` is the local route. It is a separate path, as
`/v1/fleet` and `/v1/fleet/builds` are, because the document is a list of
members rather than this machine's managers. `?member=NAME` limits it to that
member. An unknown name is `invalid_request`.

The `data` object is `members`. A read entry has `member`, `state: "read"` and
`managers` (the slice 1 array). An unavailable entry has `member`,
`state: "unavailable"` and `error` (`code`, `message`, `hint`).

Order is roster order with this member first. `elsecall fleet` lists members in
enrolment order and does not move the caller; this inventory does, so the
machine you asked is the first entry and the others stay in that roster order.
A machine that has not joined a fleet is a fleet of one: just this member,
including when it has no tailnet presence. The name is the listener's presence
when it has one, and otherwise the host name a report uses for this machine
when that name is a member name (a capitalised host label is lowercased, as a
tailnet DNS label is). When the host name cannot be a member name, the entry
is `local`.

Members are asked at the same time. One slow or unreachable member does not
drop the others. Each other member has 150 seconds (`ELSECALL_TOOLS_MEMBER_TIMEOUT_SECS`
replaces that when set to a positive number of seconds; an empty value is
unset). This member is read locally, with no network hop. The local fleet
document and the `--fleet` and `--member` reads accept 40 MiB: 64 members at
512 KiB each are 32 MiB, plus framing.

When a member's inventory cannot be read, the code is:

- `member_unreachable` — the member could not be reached, including when the
  one-second TCP connect budget expires. `elsecall fleet` reports that
  situation as `member_unavailable`; the message and hint here match that
  report.
- `member_timeout` — the member did not answer within the member budget, 150
  seconds unless `ELSECALL_TOOLS_MEMBER_TIMEOUT_SECS` says otherwise. A connect
  timeout is not this code.
- `version_skew` — the member refused because the builds differ.
- `inventory_too_large` — the member's inventory was larger than 512 KiB.
- otherwise the peer's own error code, when it is a code Elsecall writes
  (`not_found`, `protocol_error`, and so on). A code that is not of that form
  is `protocol_error`.

Another member's message and hint are not copied. The text is written here.

`elsecall tools inventory` is unchanged. `elsecall tools inventory --fleet`
prints the fleet document. `elsecall tools inventory --member NAME` prints that
one entry, from the same route (`--fleet` may be passed with it; the member
wins). The command waits 170 seconds, longer than the 150 seconds each other
member is given. Exit 0 when some members are unavailable: that is data. Exit
1 only when the local service call fails. An unknown `--member` is
`invalid_request` and exits 2.

## Slice 2b-1: desired state

This slice reads and edits the master's desired versions. It does not distribute
the file, compare it with inventory, or install anything. Distribution onto the
config-sync generation, so the file survives `fleet move-master`, comes in slice
2b-2.

The file is `~/Library/Application Support/Elsecall/fleet-tools.toml`, in the same
directory as `fleet-config.toml`. A missing file is an empty desired state, not
an error. The document is at most 64 KiB and has at most 64 member sections.
Anything else is `invalid_fleet_tools`. Diagnostics name the key path and the
rule, and never echo values.

```toml
# Manager tables are named after slice-1 managers: `mise`, `xcodes`.
[defaults.mise]
version = "2026.11.6"            # optional: the mise executable every member runs

[defaults.mise.tools.node]
default = "22.19.0"              # the global default; implies installed

[defaults.mise.tools.tuist]
install = ["4.205.0"]            # installed alongside, not default

[defaults.mise.tools."packslip:example/acme"]
default = "0.12.0"

[defaults.xcodes]
version = "1.6.0"                # optional: the xcodes executable

[defaults.xcodes.tools.xcode]    # Xcode is the single tool `xcode` under `xcodes`
default = "26.2"
install = ["26.0.1"]

[members.macbook-pro.xcodes.tools.xcode]
default = "26.3"                 # replaces the whole defaults entry for this tool on this member

[members.macbook-pro.mise.tools.tuist]   # an empty entry: nothing wanted for tuist on this member
```

The top level allows only `defaults` and `members`. A manager allows only
`version` and `tools`. A tool allows only `default` (one exact version) and
`install` (exact versions kept alongside it). Unknown keys are rejected. Under
`xcodes`, the only tool is `xcode`.

Resolved for one member, each manager has an optional executable `version` and a
map of tools. A member's `version` replaces the default executable. A member's
tool entry replaces the whole default for that tool: install lists are not
merged. An entry with neither key wants nothing, so that tool is left out of the
resolved map. `install` never contains `default`. A manager the document does
not mention is still present, with `version` null and no tools.

Versions are exact and compared byte for byte with inventory versions. They
follow the inventory's version rules and also reject `latest`, `lts`, `system`,
and anything starting with `ref:`, `path:`, `prefix:`, or `sub-`. The failure is
`invalid_fleet_tools` (the command exits 1) and says versions must be exact, for
example `22.19.0`. A malformed tool name on the command line, or `--install-only`
/ `--version` used on the manager executable itself, is `invalid_request` (exit
2).

A pin is stored exactly as given. `22` and `zulu-17` are valid text. This slice
does not resolve them through the manager. A later slice resolves them the way
`mise use --pin` does, before any planner compares them with installed versions.

Member names in the file use roster syntax. A hand-edited section for someone
who is not in the roster still parses. `tools desired` lists those names in
`unknown_members`. A `--member` argument has to name someone in the current
roster; otherwise the command is `unknown_member`.

`elsecall tools desired [--member NAME]` prints one resolved member per roster
entry, or just `--member`. A machine that has not joined a fleet is the master
of a fleet of one, so the list is that machine.

`elsecall tools set TOOL VERSION [--member NAME] [--install-only]` writes on the
master. `TOOL` is `mise` or `xcodes` (that manager's executable `version`),
`mise:<name>` (everything after the first colon is the mise tool, such as
`mise:packslip:example/acme`), `xcodes:<name>`, or `xcode` (the
same as `xcodes:xcode`). Without `--install-only` the version becomes `default`
and is dropped from `install`. With it, the version is added to `install`, and
it is left untouched when that version is already the default or already
installed, including one the member inherits. A member `--install-only` of a
version that member does not already have writes a snapshot only when that
member has no entry for the tool: the snapshot copies the inherited default,
copies `install` with the default version removed, and appends the new version.
An entry already in that scope is updated in place and keeps the default it
already set. `--member` writes that member's overlay; omitting it writes `[defaults]`.

`elsecall tools unset TOOL [--version VERSION] [--member NAME]` removes the tool
entry, or the manager `version`, when `--version` is omitted. With `--version`
it removes that version from `default` and `install`. The last version in an
entry removes the entry. An emptied manager or member table is removed.

Writes require this process to be the master. On any other member the failure
is `not_master` and names the current master. Writes are serialized, and the
file is read again inside that lock, so a hand edit made between commands is
kept. Comments and formatting outside the edited key are preserved. The file is
owner-only, mode 0600. It is not cached. An edit that leaves the bytes unchanged
is not written. Slice 2b-2 must refuse `tools set` and `tools unset` while the
config-sync handover hold is active.
