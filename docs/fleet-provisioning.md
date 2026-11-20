# Fleet provisioning

Provisioning a new Mac takes one click, and members stay in step with what jobs
need without anyone doing anything. This document records the decisions behind
that and splits the work into six deliveries. Each delivery is briefed from this
document, so it stands on its own. A change to a decision here is a product
decision and updates this document first.

[Fleet tool management](fleet-tools.md) covers what already exists: inventory of
mise and Xcode on every member, and `fleet-tools.toml`. [Adding a Mac](add-mac.md)
covers how a Mac joins: it ends enrolled and **awaiting provisioning**. This
document starts there.

## Vocabulary

- **Pin.** A tool version a workspace or the master asks for, in a file that
  already says so: `node = "22.19.0"` in `mise.toml`, `26.2` in `.xcode-version`.
  A pin has a manager, a tool, a version as written, and its source file.
- **Pin source.** One convention for writing pins. There are four (below).
- **Exact pin.** A pin that names one version. Anything else is **inexact**.
- **Plan.** For one member, the pins it lacks, minus the exceptions in
  `fleet-tools.toml`, as install steps.
- **Provisioning.** Applying a member's plan, plus the files and credentials it
  needs, until the member can run the fleet's jobs.
- **Workspace.** A directory jobs run in, usually a repository's root.

## Decisions

### Pins only

Elsecall installs exactly what is pinned. It never copies whatever the master
happens to have installed: a version on the master that nothing pins is not
wanted anywhere. The pin sources are:

| Source | File | Owns |
| --- | --- | --- |
| mise project | the mise config files at the workspace's root | mise tools |
| mise global | `~/.config/mise/config.toml` on the master | mise tools and defaults outside any project |
| Xcode | `.xcode-version` at the workspace's root | the Xcode version |
| Elsecall | `.elsecall-tools.toml` at the workspace's root | what no convention covers |

**mise project.** The files mise reads for a directory, from lowest precedence:
`.tool-versions`, `.config/mise/conf.d/*.toml`, `.config/mise/config.toml`,
`.config/mise.toml`, `mise/config.toml`, `mise.toml`, `.mise/config.toml` and
`.mise.toml`. Each config name has environment variants (`mise.signing.toml`),
which mise reads only with `MISE_ENV=signing`. Their pins carry that environment:
a job that sets it needs them, so the planner installs them too. `*.local.toml`
files are one machine's overrides, not the repository's, and are not read. Parent
directories belong to the member, not the repository, and are not read either.
Idiomatic version files (`.nvmrc`, `.python-version`) are off by default in mise
and are not read.

**mise global.** Only the master's `~/.config/mise/config.toml`. Its pins are
the defaults every member's shell gets outside a project. Like mise, they are a
layer under the project: a project pinning another version of the same tool is
not a conflict, and both versions are wanted.

**`.xcode-version`.** One version, digits and dots, as xcodes and fastlane read it.

**`.elsecall-tools.toml`.** A new repository file, only for what no convention
covers. Simulator runtimes come first:

```toml
[simulators]
runtimes = ["iOS 26.2"]
```

A runtime is a platform (`iOS`, `tvOS`, `watchOS`, `visionOS`, as `simctl` names
them) and an exact version of at least two segments. The schema is strict: an
unknown key is an error, so the file cannot repeat a pin another source owns. It
is a new name on purpose: `.elsecall.toml` is the legacy workload file, which
Elsecall now ignores; that file is not a pin source.

**Conflicts are errors.** Two workspace files pinning the same tool for the same
mise environment fail the read with `pins_conflict`, unless both are mise project
files asking for the same versions, which is one pin. A conflict is never
resolved by precedence, because a reader of either file alone would be misled.

**Default simulator runtime.** With no runtime pin, a workspace that pins Xcode
needs the runtime matching that Xcode's SDK (Xcode 26.2 → iOS 26.2). The planner
derives it; it is not a pin and is not printed by `tools pins`.

### Reading pins runs no repository code

A repository is untrusted. mise config can hold `[hooks]`, `[env]` entries that
run `exec()`, tasks, Tera templates in tool versions, and per-tool `postinstall`
commands, and mise itself refuses to load an untrusted config. Asking mise
(`mise ls --json` in the workspace) would either refuse or, once trusted, run
that code on every member that reads the workspace.

**Decision: Elsecall reads the files directly, as data, with strict validation.**
It parses each file as TOML and looks only at `[tools]`. `[env]`, `[hooks]`,
`[tasks]` and the rest are never evaluated. A tool's value is a version, a list
of versions, a table with `version`, or a list mixing them. A table whose `os`
leaves out macOS pins nothing on a Mac. Other per-tool options are ignored
(see "Open questions" for those the installer needs). A version must be literal:
one containing `{{`, `{%` or `{#` is an error, not something to render.

The reader also does not trust the files themselves. Only a regular UTF-8 file of
at most 1 MiB is read. A workspace file must resolve inside the workspace once
links are followed, so `mise.toml` linking to `~/.ssh/id_ed25519` is refused. A
FIFO is opened non-blocking and refused. Errors name the file, the key and the
rule, and never a value.

mise runs only to resolve inexact pins against this machine, as the inventory
already runs it: in the home folder, with `MISE_*` removed, never in the
workspace.

### Inexact pins

A real consumer pins `bun = "latest"`; the master's global config pins
`node = "22"`. Elsecall does not treat these as exact, and does not refuse them.
`tools pins` reports each with `exact: false`, the version it resolves to on
this machine (`resolved`), and a hint to pin exactly. The resolved version is
what mise picks without the network: the newest installed version that matches.
That was checked against mise 2026.10 with `bun = "latest"` and three bun
versions installed: mise chose the newest by version order, as Elsecall does.

- mise: `latest` resolves to the newest installed version. `lts`, `system`,
  `ref:`, `path:`, `prefix:` and `sub-` are inexact and unresolved. A plain
  version that an installed version extends segment by segment (`22` with
  `22.19.0` installed, `zulu-17` with `zulu-17.54.21`) is inexact and resolves to
  the newest such version. Any other plain version is exact, including one this
  machine has not installed.
- Xcode: two or more segments are exact (`26.2`, `26.0.1`), as Xcode releases are
  named; `26.2` does not mean `26.2.1`. One segment (`26`) is inexact and resolves
  to the newest installed `26.x`.
- Simulator runtimes: always exact; `.elsecall-tools.toml` accepts nothing else.

When a manager cannot be read here, the report carries a warning and its plain
versions count as exact. The planner (PR 2) plans an inexact pin as its resolved
version on the master and shows it as such. An unresolved inexact pin is not
installed; it is shown with its hint.

### Comparing versions

Pinned versions compare with inventory versions by the inventory's rules
([fleet-tools.md](fleet-tools.md), Slice 1): segments split on `.` and `-`,
numeric segments compare as numbers. So `.xcode-version` `26.2` matches the
inventory's `26.2` (the bundle's `CFBundleShortVersionString`), and `22.19`
is extended by `22.19.0` but not by `22.1.0`.

### Home-relative

Members' home folders differ, even in capitalisation (`/Users/me` on one,
`/Users/Me` on another). Everything Elsecall names on more than one member is
home-relative: the global config is `~/.config/mise/config.toml` in output, and
workspace files are relative to the workspace.

### When a job needs something unpinned

It fails on a member that lacks the tool. Elsecall then attaches a "not pinned,
pin it here" attention item to the run (PR 5), naming the tool and the file to
pin it in. The user either pins it (recommended: every member gets it) or
installs it on that member by hand (not recommended: the next member fails the
same way). A later agent hand-off offers the pin as a change to the repository.

### Two ports and a planner

- **`PinSource`**: what a workspace needs. Adapters: mise project, mise global,
  `.xcode-version`, `.elsecall-tools.toml`. Lives in `elsecall-tools`
  (`pins`), shipped in PR 1.
- **`ToolManager`**: how a manager reads what is installed and, from PR 4,
  installs. Adapters: mise, Xcode, simulator runtimes. Today's
  `tool_inventory::ToolManager` is its read half, with mise and Xcode adapters;
  PR 2 adds a simulator runtime reader and PR 4 adds installing.
- **The planner** (PR 2) joins them: pins from the workspaces jobs ran in, minus
  what the member has installed, minus the exceptions. Neither port knows about
  the other.

### `fleet-tools.toml` holds only exceptions

[Slice 2b-1](fleet-tools.md#slice-2b-1-desired-state) made `fleet-tools.toml`
the hand-written desired state. Pins replace that role. The file now holds only
exceptions: tools to leave out, and intended per-member differences, such as a
member kept on an older Xcode on purpose. PR 2 implements this and settles the
format; the intent is the existing shape, where an empty entry wants nothing:

```toml
[defaults.mise.tools.sentry-cli]          # leave this pin out everywhere

[members.macbook-pro.xcodes.tools.xcode]
default = "26.3"                          # this member intentionally differs
```

Until PR 2 lands, the file and its commands work as Slice 2b-1 describes.

### Provisioning

These are implemented in later deliveries and recorded now.

- **A member becomes ready automatically** once provisioning succeeds. Add Mac
  leaves it awaiting provisioning; nobody runs `elsecall fleet ready` by hand.
- **Xcode is copied from the master's installed bundle**, an
  `/Applications/Xcode-*.app`, streamed over the members' channel. No Apple ID is
  needed. Placing it, selecting it and accepting its license use the narrowly
  scoped sudoers drop-in the Xcode delivery installs, never a general root shell.
- **Installs run as admitted work on the member**, through the same admission as
  jobs, so they do not starve running work and are visible like it.
- **Nothing is ever uninstalled automatically.** A version nothing pins is
  reported, not removed.
- **Agents may confirm tool plans; humans confirm file and credential plans.**
  From PR 6, a tool plan that follows from pins is applied without asking.
- **Secret-looking files are detected, then asked about rather than blocked.**
  The prompt has a checkbox, "Sync changes to these automatically from now on".
  Unchecked, Elsecall asks again on the next change.
- **Hard blocks stay only for** `~/.ssh/authorized_keys`, the login keychain,
  `~/Library/LaunchAgents` and Elsecall's own state. The other paths
  [configuration sync](config-sync-slice.md) forbids today (the rest of `~/.ssh`,
  `~/.gnupg`, other keychains) become asks (PR 5).
- **Steps only a person can do**, which the UI guides: macOS privacy prompts on
  the member's own screen; an Apple ID, only when no member has the pinned Xcode;
  and exporting a signing identity from the master's keychain, once.

## Deliveries

Each is one pull request, verifiable on its own.

| # | Delivery | Depends on |
| --- | --- | --- |
| 1 | This document, and reading pins: the `PinSource` port, its four adapters, `elsecall tools pins` | Inventory (fleet-tools Slice 1) |
| 2 | A plan per member: pins from the workspaces jobs ran in, minus what the member has installed, minus the exceptions; `fleet-tools.toml` narrowed to exceptions; simulator runtime inventory | 1 |
| 3 | Running commands on a member, and "needs attention" items (in progress in parallel) | — |
| 4 | Installing: mise tools, Xcode copied from the master, simulator runtimes, as admitted work | 2, 3 |
| 5 | A warning when a run fails on a missing tool ("not pinned, pin it here"), and consent for secret files | 1, 3 |
| 6 | The automatic loop: drift detection, automatic installs, placement by pins, automatic ready; and the UI | 2, 4, 5 |

The Add Mac flow and the Provision button in the fleet window belong to the Add
Mac work (branch `fix-add-mac-onboarding`), which already shows members awaiting
provisioning. Delivery 6 drives that button; it does not rebuild the flow.

## Delivery 1: reading pins

`elsecall tools pins [PATH]` reads a workspace's pins. PATH defaults to the
current directory. It does not call the local service; it reads files, and runs
this machine's mise (in the home folder) and Xcode scan only to resolve inexact
pins: mise when any mise pin exists, Xcode only for a one-segment Xcode pin.

It prints the standard JSON envelope. `data` is:

- `workspace`: the canonical path read.
- `sources`: each file read, in reading order, as `{kind, file}`. `kind` is
  `mise_project`, `mise_global`, `xcode_version` or `elsecall_tools`. `file` is
  workspace-relative, or `~/.config/mise/config.toml`. A file that pins nothing is
  still listed.
- `pins`: ordered by manager (mise, xcodes, simulators), tool, environment, then
  source. Each has `manager`, `tool`, `version` (as written), `exact`,
  `environment` (the mise environment, or null) and `source` (`{kind, file}`). An
  inexact pin adds `resolved` (a version, or null) and `hint`. Managers and tool
  names are inventory's: Xcode is manager `xcodes`, tool `xcode`; a runtime is
  manager `simulators`, tool `iOS`.
- `warnings`: managers that could not be read, as `{manager, error}`, with the
  inventory's error codes.

Errors exit 1, except a PATH that is not a readable directory, which is
`invalid_request` and exits 2:

- `pin_source_unreadable`: a file that is not a regular file, is larger than
  1 MiB, is not UTF-8, resolves outside the workspace, or cannot be opened.
- `pin_source_invalid`: a file that breaks a rule above. The message names the
  file, the key (or line) and the rule.
- `pins_conflict`: two files pin one tool. The message names both files.

No message contains a value from a file.

### Evidence

On the master (`mac-mini`, account `me`), with neutral names:

- `acme-ios`: `mise.toml` and `mise.signing.toml` read, no `.xcode-version`.
  python 3.10.4, node 22.19.0, tuist 4.205.0 and swiftgen 6.6.3 exact. `bun` and
  `sentry-cli` `latest`, inexact, resolved to 1.3.14 and 3.8.0. One signing tool
  exact, with `environment: "signing"`. From the global config: `node = "22"`
  resolved to 22.19.0, `java = "zulu-17"` resolved to zulu-17.54.21, and ruby
  3.2.0 plus three more tools exact. About 0.1 s with mise's cache warm.
- This repository: rust 1.99.0, packslip 1.11.1, nextest 0.9.150 (a table with
  `version_prefix`) and mr-boxington 1.30.0, exact; `[wrappers]` and `[tasks]`
  ignored. About 3 s on the first run, which is `mise ls` itself.
- A scratch workspace with `.xcode-version` `26` resolved to the installed 26.2,
  and a templated version, a link out of the workspace and two files pinning one
  tool each failed with the code above.

The fleet when this started: nothing configured (`fleet-tools.toml` and
`[sync] paths` empty); placed runs already warning "Toolchain xcode differs",
the master selecting Xcode 26.2 and `macbook-pro` (account `devbox`) 26.3; and a
third, new Mac with Command Line Tools only: no mise, no Xcode, no simulator
runtimes, no `~/.config`, and a home folder whose capitalisation differs from
the master's.

## Open questions for later deliveries

- **Which workspaces.** PR 2 decides which directories count as "repos jobs ran
  in": a run's working directory or its repository root, and for how long one
  stays in the set.
- **Install options.** Some tables carry options the installer needs
  (`version_prefix` for a GitHub release, verification identities for a package
  backend). PR 4 decides which to carry, as an allowlist; `postinstall` is never
  one of them, and installs name `tool@version` explicitly instead of running
  `mise install` in a workspace.
- **mise lockfiles.** A `mise.lock` records exact resolved versions and would make
  `latest` pins exact without asking the master. Not read yet.
