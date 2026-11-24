# Fleet tool management

Elsecall keeps the development tools on a fleet at the versions the workspaces it
runs pin. This document fixes the vocabulary and the rules of inventory and
`fleet-tools.toml`. [Fleet provisioning](fleet-provisioning.md) records how pins
decide what each member needs and how members get it. Slices implement them one
at a time. A change to anything here is a product decision and updates this
document first.

## Vocabulary

- **Tool manager.** Software Elsecall drives to install tools. The managers are
  `mise`, `xcodes` and `simulators` (simulator runtimes, through `simctl`).
- **Tool.** Something a manager installs, named within that manager. A mise tool
  uses mise's own identifier, exactly as `mise ls` reports it (`node`, `tuist`,
  `packslip:example/acme`). Xcode is the single tool `xcode`. A simulator
  runtime's tool is its platform as `simctl` names it (`iOS`, `tvOS`,
  `watchOS`, `visionOS`).
- **Installed version.** An exact version present on disk.
- **Default.** The version used when no project selects one. For mise, that is
  the version selected by mise's global config. For Xcode, it is the app
  `xcode-select -p` points at.

## Desired state

What a member needs comes from **pins**: the versions the workspaces jobs run in
already declare (mise config, `.xcode-version`, `.elsecall-tools.toml`) and the
master's global mise config. Elsecall installs exactly what is pinned and never
copies what the master happens to have installed. `elsecall tools pins` reads
them; [Fleet provisioning](fleet-provisioning.md) has the sources and rules.

The master holds `fleet-tools.toml` beside `fleet-config.toml`. It records
[exceptions](#exceptions) and which signed tools Elsecall keeps selected globally.

## Automatically repair a signed Packslip tool

Add the tool to the top level of the master’s `fleet-tools.toml`, before any
table headers. This opt-in is needed once per tool.

```toml
managed_packslip = ["packslip:github.com/acme/tool"]
```

Install the approved release on the master and pin it in
`~/.config/mise/config.toml`. Use the repository’s exact workflow and release tag.

```toml
[tools."packslip:github.com/acme/tool"]
version = "2.0.0"
identity = "https://github.com/acme/tool/.github/workflows/release.yml@refs/tags/v2.0.0"
issuer = "https://token.actions.githubusercontent.com"
```

The provisioning loop repairs a missing release, an inactive global version,
or a different signer on a member. It asks mise to reinstall through Packslip
with the approved identity and issuer and select the release globally. The
next plan checks the member’s declaration and installed default. Failed
installs appear in Setup and follow the usual provisioning retry schedule.
A failed verification leaves the signing declaration in place. During a
forced reinstall mise can remove the cached copy of that same version, so
the tool may be unavailable until verification succeeds.

If a member already has extra settings for the tool, the plan reports them and
waits for the operator to keep them with a member exception or remove them.
Only the authenticated master can request this global configuration change.

Update the version and corresponding signing tag on the master for the next
release. Managed entries support exactly the three fields shown above, a
three-part numeric version, and a tag of the form `v<version>`. A missing or
invalid declaration on the master stops planning and identifies the key to fix. Each managed
tool follows the master’s global pin across the fleet. A fleet or member
exception takes precedence. Update members to a supporting Elsecall version
so they can report their signing declarations.

## Plan and apply

A plan shows what would change on a member: `elsecall tools plan`, from
[provisioning delivery 2](fleet-provisioning.md#delivery-2-a-plan-per-member).
Apply does it. Nothing is installed,
selected or removed as a side effect of reading the fleet. An agent may confirm a
tool plan; a person confirms file and credential plans. Once the automatic loop
lands (provisioning delivery 6), tool plans that follow from pins are applied
without asking, as admitted work on the member.

A version that is installed but not pinned is reported. Elsecall never
uninstalls anything automatically.

## Xcode

Members get Xcode by copying the master's installed bundle
(`/Applications/Xcode-*.app`) over the members' channel, so no Apple ID is
needed. Only when no member has the pinned Xcode does a person sign in to
download it. Steps that need root (placing the app, selecting it, accepting the
license) go through a narrowly scoped sudoers drop-in.
It does not grant a general root shell.

## Joining a machine

`elsecall fleet add` is the usual way a machine joins: [Adding a Mac](add-mac.md). The machine
ends up enrolled and awaiting provisioning. Installing the sudoers drop-in belongs
to the Xcode slice, which also says what it allows.

## Interface

Agents use the CLI. A native rune app talks to the same local socket. Both see
the same inventory and, in later slices, the same plan.

Homebrew and Mint are out of scope. Simulator runtimes are in scope through
provisioning: they are pinned in `.elsecall-tools.toml`.

## Slice 1: inventory

This slice only reads the local machine. It does not install, select, uninstall,
read `fleet-tools.toml`, call another member, or open a UI.

`elsecall tools inventory` prints the standard JSON envelope from
`GET /v1/tools/inventory`. The listener's same-user rule authorizes it, as for
the other local routes. Exit 0 when the service answers, including when one
manager failed: that failure is a `state: "failed"` entry with `error.code`,
`error.message` and `error.hint`. Exit 1 only when the service call itself
fails. The command waits 150 seconds. Other local commands still wait 3.

The `data` object is `managers`: mise, then xcodes, then simulators. A read entry has
`manager`, `state: "read"`, `executable` and `tools`. `executable.state` is
`found` (`path`, `version`) or `missing`. Each tool has `name`, `version` and
`default`. An Xcode tool also has `build` and `path` (the `.app` bundle); a
simulator runtime also has `build`. Tools
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
`ELSECALL_XCODE_APPLICATIONS`, `ELSECALL_XCODE_SELECT` or `ELSECALL_SIMCTL`.
Elsecall passes that absolute path to the command. For `ELSECALL_MISE`,
`ELSECALL_XCODES` and `ELSECALL_SIMCTL`, a
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

**simulators.** Simulator runtimes, read through `simctl`, which ships inside
Xcode. When `ELSECALL_SIMCTL` is unset, the executable is what
`/usr/bin/xcrun --find simctl` prints (10 seconds). When that fails, as on a Mac
with only the Command Line Tools, the executable is `missing` and no runtimes
are listed; that is not a failure, and a timeout is `tool_manager_timeout`.
Otherwise Elsecall runs the absolute path in `/`, with stdin closed:

- `simctl --version` — the version is the CoreSimulator project version
  (`@(#)PROGRAM:simctl  PROJECT:CoreSimulator-1051.17.7` → `1051.17.7`).
- `simctl runtime list -j` — an object keyed by runtime identifier. A runtime
  whose `state` is `Ready` is installed; an entry in any other state is left
  out, even one missing fields, as is a
  `platformIdentifier` that is not the iOS, tvOS, watchOS or visionOS
  simulator. The tool is the platform (`iOS`), the version is the runtime's
  `version` (`26.2`) and `build` is its build (`23C54`). No runtime is a
  default. Two runtimes of one version with different builds (betas) are both
  listed.

Each command has 30 seconds, with the same failure codes as mise. On a Mac with
Xcode 26.2 and nineteen runtimes the read takes about 0.1 seconds.

The managers are read off the async worker. One manager failing does not hide
the others.

## Slice 2a: fleet inventory

Any member can read every member's inventory. This slice still does not install,
select, uninstall, read `fleet-tools.toml`, or open a UI.

`GET /peer/v1/tools/inventory` returns this member's inventory: the same `data`
document as `GET /v1/tools/inventory` (`managers`, mise, xcodes then simulators,
in that order; an Xcode `path` is absolute, and a runtime is one of the four
platforms, has a `build` and no `path`, and is never a default). It is a signed peer request. Any enrolled
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

## Exceptions

The `defaults` and `members` tables hold exceptions to the pins. Slice 2b-1 made it the
hand-written desired state; provisioning delivery 2 narrowed it, and a file in
that older shape is refused, never read with a new meaning.

The file is `~/Library/Application Support/Elsecall/fleet-tools.toml` on the
master, in the same directory as `fleet-config.toml`. A missing file is no
exceptions, not an error. The document is at most 64 KiB and has at most 64
member sections. Anything else is `invalid_fleet_tools`. Diagnostics name the
key path and the rule, and never echo values.

```toml
# Manager tables are named after inventory's managers: `mise`, `xcodes`, `simulators`.
[defaults.mise.tools.sentry-cli]          # leave this pin out on every member

[members.macbook-pro.xcodes.tools.xcode]
default = "26.3"                          # this member intentionally differs

[members.macbook-pro.mise.tools.sentry-cli]
install = ["3.8.0"]                       # and keeps a tool the others leave out

[members.mac-mini.simulators.tools.iOS]   # no iOS runtimes on this member
```

- The top level allows `managed_packslip`, `defaults`, and `members`. A manager table allows
  only `tools`. Under `xcodes` the only tool is `xcode`; under `simulators` the
  tools are the platforms `iOS`, `tvOS`, `visionOS` and `watchOS`.
- A tool entry under `[defaults]` must be empty: it leaves that tool out on
  every member, whatever the pins ask for it. Even an empty `install = []`
  is refused there.
- A tool entry under `[members.<name>]` is empty, leaving the tool out on that
  member, or names `default` (one exact version) and `install` (exact versions
  kept beside it), at least one version. Those versions are planned on that member instead of the
  pinned ones; `default` is the one to select, as the global mise default or
  the selected Xcode. `install` never contains `default`.
- A member's entry replaces the defaults' entry for that tool.
- An exception only changes what pins ask for: one for a tool nothing pins
  plans nothing, and the plan lists it so it can be removed.

Versions are exact. They follow the inventory's version rules and also reject
`latest`, `lts`, `system`, and anything starting with `ref:`, `path:`,
`prefix:`, or `sub-`; the failure says versions must be exact, for example
`22.19.0`. The older shape is refused with the key that holds it: a version
under `[defaults]` ("a [defaults] entry only leaves a tool out…"), and a
manager's executable `version` ("fleet-tools.toml no longer sets a manager's executable"). The hint names this section.

Member names in the file use roster syntax. A hand-edited section for someone
who is not in the roster still parses; `tools exceptions` lists those names in
`unknown_members`. A `--member` argument has to name someone in the current
roster; otherwise the command is `unknown_member`.

The commands talk to the local service (`/v1/tools/exceptions`). `TOOL` is
`mise:<name>` (everything after the first colon is the mise tool, such as
`mise:packslip:example/acme`), `xcode` (or `xcodes:xcode`), or
`simulators:<platform>`. `mise` and `xcodes` alone, the executables slice 2b-1
could set, and a malformed tool name are `invalid_request` (exit 2).

- `elsecall tools exceptions [--member NAME]` prints, for every roster member
  or one, the exceptions that apply to it: its own entries and the defaults'
  for tools it has none for, ordered by manager and tool. Each has `manager`,
  `tool`, `key` (the entry's key in the file) and `kind`: `leave_out`, or
  `versions` with `default` (or null) and `install`. A machine that has not
  joined a fleet is the master of a fleet of one.
- `elsecall tools leave-out TOOL [--member NAME]` writes an empty entry, for
  every member or for one. An entry with versions in that scope loses them.
- `elsecall tools set TOOL VERSION --member NAME [--install-only]` gives one
  member its own versions. `--member` is required: for every member, change the
  pin. Without `--install-only` the version becomes `default` and is dropped
  from `install`; with it, the version is added to `install` unless the entry
  already holds it.
- `elsecall tools unset TOOL [--version VERSION] [--member NAME]` removes the
  entry, or with `--version` that version from `default` and `install`. The last
  version in an entry removes the entry; it does not leave the tool out. An
  emptied manager or member table is removed.

Writes require this process to be the master. On any other member the failure
is `not_master` and names the current master. Writes are serialized, and the
file is read again inside that lock, so a hand edit made between commands is
kept. Comments and formatting outside the edited key are preserved. The file is
owner-only, mode 0600. It is not cached. An edit that leaves the bytes unchanged
is not written.

The file lives on the master only. Carrying it on the config-sync generation, so
it survives `fleet move-master`, is still to come (slice 2b-2); until then, copy
`fleet-tools.toml` to the new master after a move. When it lands, `tools
leave-out`, `set` and `unset` are refused while the config-sync handover hold is
active.
