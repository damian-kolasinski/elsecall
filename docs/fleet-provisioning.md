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

**mise project.** The files mise reads for a directory, in mise's order from
lowest precedence (as `mise config ls` lists them, checked with mise 2026.10):
`.tool-versions`; `.config/mise/conf.d/*.toml`; `.config/mise/config.toml`,
`.config/mise/mise.toml`, `.config/mise.toml`, `.mise/config.toml`,
`mise/config.toml`, `mise.toml`, `.mise.toml`; then the environment variants
`.config/mise/config.<env>.toml`, `.config/mise.<env>.toml`,
`mise/config.<env>.toml`, `mise.<env>.toml`, `.mise/config.<env>.toml` and
`.mise.<env>.toml`, which mise reads only with `MISE_ENV=<env>`. Their pins carry
that environment: a job that sets it needs them, so the planner installs them
too. `*.local.toml` files, in `conf.d` too, are one machine's overrides, not the
repository's, and are not read, though mise reads them. Parent directories belong
to the member, not the repository, and are not read either. Idiomatic version
files (`.nvmrc`, `.python-version`) are off by default in mise and are not read.
The asdf names mise accepts for its own tools are read as mise's names (`nodejs`
is `node`, `golang` is `go`), so they compare with inventory.

**mise global.** Only the master's `~/.config/mise/config.toml`. Its pins are
the defaults every member's shell gets outside a project. mise's other global
files (`~/.config/mise/conf.d/*.toml`, `config.local.toml`, environment
variants) are not read, so a default pinned only there is not seen. The home
folder is not a workspace: its mise config is this global layer. Like mise, they are a
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
leaves out Apple Silicon macOS (`macos`, `darwin`, or either with `/arm64`)
pins nothing, since every member runs Elsecall's arm64 build. Of the other
per-tool options only the [install options](#install-options) are kept; the rest
are never read. A version must be literal:
one containing `{{`, `{%` or `{#` is an error, not something to render.

The reader also does not trust the files themselves. A workspace path is
resolved one component at a time from each link's text, so nothing outside the
workspace is touched, not even to refuse it: `mise.toml` linking to
`~/.ssh/id_ed25519`, or `.config` linking to another folder, is refused by the
link. An absolute link back into the workspace must spell the workspace's
canonical path (`/private/tmp`, not `/tmp`); a relative link is the usual form.
The resolved file is opened without following links and without blocking,
so a FIFO is refused rather than waited on. Only a regular UTF-8 file of at most
1 MiB is read, and at most 64 `conf.d` files and 64 environment files per name
pattern are listed. Errors name the file, the key and the rule, and never a
value or an unknown key.

mise runs only to resolve inexact pins against this machine, as the inventory
already runs it: in the home folder, with `MISE_*` removed, never in the
workspace.

### Install options

Some tools need options next to their version to be installed at all: a GitHub
release whose tags are `cargo-nextest-0.9.150` needs `version_prefix`. Delivery 4
decided which options travel from a pin to the installer: an allowlist of the
options that pick which published artifact a version is and how mise verifies
it, so a member installs the build the pins describe, and nothing else.

- which release and asset: `version_prefix`, `asset_pattern`, `matching`;
- where its executables are: `bin`, `bin_path`, `exe`, `rename_exe`,
  `strip_components`;
- who must have signed it: `slsa_signer_identity`, `slsa_signer_issuer`.

They are read from a tool's table and from the brackets of its key, as mise
reads `"ubi:cli/cli[exe=gh]"`: the tool is `ubi:cli/cli`, with `exe`. One option
set both ways is an error. Every other option is dropped. `postinstall` is a
shell command and `install_env` the environment a build runs in, so neither ever
travels; nor does `api_url`, which would send the download to another server,
or `os`, which the reader applies.

A carried value is a string, number or boolean of 1 to 256 characters, literal
like a version (a template is `pin_source_invalid`). The installer passes
options in mise's bracket syntax (`tool[option=value]@version`), so a value
holds no white space, control character, comma, equals sign, bracket, quote or
backslash, which could end it there; that is why `matching_regex` and
`filter_bins`, whose values need them, are not carried. A path (`bin`,
`bin_path`, `exe`, `rename_exe`) is relative to the tool's install and has no
`..`. A value breaking these rules is `pin_source_invalid`, naming the key.

- One file repeating a version with other options is `pin_source_invalid`; two
  workspace files pinning the same versions with different options conflict
  (`pins_conflict`).
- Across workspaces, and between a workspace and the master's global config,
  pins asking for one version with different options are not planned: the
  version is unresolved with a hint to pin it with the same options everywhere,
  unless the member already has it.
- An exception's versions take the options of the pins it replaces when they
  agree; when they disagree, its versions are unresolved the same way, since an
  exception names versions and no options.

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

`exact` is therefore relative to the machine that read the pins. `node = "22"`
reads as exact where no 22.x is installed, as on a bare Mac, although mise would
install the newest 22.x upstream. Pins are read on the master, where jobs run and
the tools are installed; delivery 2 must still not plan a plain version the
master has not installed as if it were exact, since members would then drift.
When a manager cannot be read here, the report carries a warning and its plain
versions count as exact. The planner (delivery 2) plans an inexact pin as its resolved
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

It fails on a member that lacks the tool. Elsecall then raises a "not pinned, pin
it here" attention item (delivery 5), naming the run, the member, the tool and the
file to pin it in. The user either pins it (recommended: every member gets it) or
installs it on that member by hand (not recommended: the next member fails the
same way). A later agent hand-off offers the pin as a change to the repository.

**Recognising it.** The caller of a placed run sees every byte of its output, since
it passes the member's streams through to its own. As they pass, it watches for:

- exit status 127 from the member failing to start the command at all, which the
  member now reports as `command_not_found` (exit 125, as before);
- a shell's `command not found` (zsh, bash, sh, make), `env` or `make` saying
  `No such file or directory` for a program, as a `#!/usr/bin/env node` script
  does;
- the `xcode-select` and `xcrun` stubs saying a tool requires Xcode or is not a
  developer tool, which names Xcode, pinned in `.xcode-version`;
- mise's `No version is set for shim: X`, `X@1.2 is not installed` and `missing:
  X@1.2`.

A recognised line counts only when the command exits with a non-zero status; the
last one before the exit wins. A program, or a tool mise named, counts only if the
caller's own `PATH` has it: a program the master lacks too is a typo, or missing
everywhere, which no pin on the master would fix, and a name the job's own output
made up (`npm:anything`) is never suggested for installing. Exit 127 without a recognised line names no tool, so
it raises nothing.

**Privacy and speed.** Each stream's current line is held up to 512 bytes; a
longer line is skipped whole, and a line is dropped once examined. The bytes
written to the caller's streams are the ones received, unchanged and in order,
and the check is a scan for newlines plus a few substring tests per line. Nothing
of the output is stored or logged. Only the tool's name is kept, on the run's
record, after validation: letters, digits and `-_.+@:/`, at most 64 characters,
and never the shape of a credential (a token prefix, or a long segment mixing
letters and digits), so a script that runs an unquoted `$TOKEN` as a command never
leaves the token behind. A stored name a later build no longer accepts reads as
none, so it never stops records from being read.

**Where the item lives (decided).** The fact, the tool and its manager, is kept on
the caller's record of the run (`placed.missing_tool`). The item is derived from
the latest records each time the fleet view is read, like every attention item,
one per member and tool from the latest run that hit it. It clears when:

- the tool is pinned: the run's workspace, or the master's global mise config, pins
  it (a mise pin under a backend, such as `aqua:realm/SwiftLint`, counts for the
  program `swiftlint`). The pins are read as `tools pins` reads them, from files
  only, without resolving versions;
- a later run of the same job (label and working tree; unlabelled runs are never
  the same job) on that member exits 0: the tool was installed by hand, or the job
  no longer needs it. Another job's earlier hit of the same tool then still stands;
- the run ages out: it ended more than 7 days ago, or is not among the latest 500
  records.

A run outside any working tree names the master's global mise config. The item
appears in the fleet view of the member that placed the run, which is where the
agent that ran it looks; placed runs start on the master.

**Local runs (decided).** A local run raises nothing. Its command inherits the
caller's terminal directly, which is what keeps `elsecall run` indistinguishable
from running the command; Elsecall never sees that output, and passing it through
a pipe to watch it would change what the command sees (a terminal, colours, line
buffering). A local run's failure is printed in the terminal of whoever ran it,
on the machine where the tools come from; a command missing there fails before
admission, as it always has.

**Actions.** The first pins the tool: the summary names the file, and its command
(`elsecall tools pins <workspace>`, or `elsecall fleet` outside a workspace) shows
the pin once written. The second, for mise tools only and `person_only`, installs
it on that member alone (`elsecall run --member <member> -- mise use --global
<tool>@latest`), which is not recommended.

### Two ports and a planner

- **`PinSource`**: what a workspace needs. Adapters: mise project, mise global,
  `.xcode-version`, `.elsecall-tools.toml`. Lives in `elsecall-tools`
  (`pins`), shipped in delivery 1.
- **`ToolManager`**: how a manager reads what is installed and, from delivery 4,
  installs. Adapters: mise, Xcode, simulator runtimes. Today's
  `tool_inventory::ToolManager` is its read half, with mise, Xcode and
  simulator runtime (`simctl`) adapters; delivery 4 adds installing.
- **The planner** (delivery 2) joins them: pins from the workspaces jobs ran in, minus
  what the member has installed, minus the exceptions. Neither port knows about
  the other.

### `fleet-tools.toml` holds only exceptions

Slice 2b-1 made `fleet-tools.toml` the hand-written desired state. Pins replace
that role. The file now holds only exceptions: tools to leave out, and intended
per-member differences, such as a member kept on an older Xcode on purpose.
Delivery 2 settled the format ([Exceptions](fleet-tools.md#exceptions)): the
existing shape, where an empty entry wants nothing, with versions only in a
member's entry and no manager executables. A file in the older shape is refused
by key, not read with a new meaning:

```toml
[defaults.mise.tools.sentry-cli]          # leave this pin out everywhere

[members.macbook-pro.xcodes.tools.xcode]
default = "26.3"                          # this member intentionally differs
```

An exception changes only what pins ask for: one for a tool nothing pins plans
nothing. `elsecall tools exceptions`, `leave-out`, `set --member` and `unset`
read and write it.

### Which workspaces

**A workspace is the repository root a run was admitted from** (the working
tree's `git rev-parse --show-toplevel`), not the run's working directory: pin
files live at the root, and a run in a subdirectory needs the same tools. Each
git worktree is its own workspace, since it may be on a branch that pins other
versions. A run outside a working tree adds no workspace; on the master its
directory is the home folder, whose mise config is the global layer.

**It stays in the set for 30 days after its last run**, as run records do, and
drops out sooner when its directory is gone (a removed worktree). Admission
records the root of every run made in a working tree, local or placed, on the
member where the run was requested, in `run-workspaces.json` in that member's
data directory: owner-only, the 256 most recent roots, each rewritten at most
hourly. Synthetic rehearsal runs are not recorded. The run records themselves
keep only salted hashes of workspace paths, so they cannot be read back.

**Pins are read where the files are, by that member, as data.** Each member
answers `GET /peer/v1/tools/workspace-pins` with its 64 most recently used
workspaces and what each one's own files pin (the mise project files,
`.xcode-version`, `.elsecall-tools.toml`; never its global config), read by
the same `PinSource` adapters as `tools pins`, without running mise. The pins
travel unjudged: the master judges them against what it has installed. A member
names a workspace home-relative (`~/Developer/acme-ios`), or by its absolute
path outside its home folder, and the plan names it together with the member
that holds it, so members whose home folders differ (`/Users/me`,
`/Users/Me`) name the same layout the same way.

**The plan runs on the master.** It reads the master's global config and
`fleet-tools.toml` there, and judges pins against the master's inventory.
`tools plan` on another member is `not_master` and names the master.

### Provisioning

These are implemented in later deliveries and recorded now.

- **A member becomes ready automatically** once provisioning succeeds. Add Mac
  leaves it awaiting provisioning; nobody runs `elsecall fleet ready` by hand.
- **Xcode is copied from the master's installed bundle**, an
  `/Applications/Xcode-*.app`, streamed over the members' channel. No Apple ID is
  needed. Placing it, selecting it and accepting its license use the narrowly
  scoped sudoers drop-in the Xcode delivery installs, never a general root shell
  (delivery 4: placed in `/Library/Elsecall/Xcodes`, a tree only root writes,
  and linked from `/Applications` by the account).
- **Installs run as admitted work on the member**, through the same admission as
  jobs, so they do not starve running work and are visible like it.
- **Nothing is ever uninstalled automatically.** A version nothing pins is
  reported, not removed.
- **Agents may confirm tool plans; humans confirm file and credential plans.**
  From delivery 6, a tool plan that follows from pins is applied without asking.
- **Secret-looking files are detected, then asked about rather than blocked.**
  The prompt has a checkbox, "Sync changes to these automatically from now on".
  Unchecked, Elsecall asks again on the next change. Delivery 5 implements it; see
  "Consent for secret files" below.
- **Hard blocks stay only for** `~/.ssh/authorized_keys` (with `authorized_keys2`,
  which OpenSSH also reads, and `rc` and `environment`, which act at every SSH
  login), the login keychain,
  `~/Library/LaunchAgents` and Elsecall's own state (`~/.elsecall`,
  `~/Library/Application Support/Elsecall`, and `~/.local/bin`, where its command
  lives). The other paths [configuration sync](config-sync-slice.md) forbade (the
  rest of `~/.ssh`, `~/.gnupg`, other keychains) became asks in delivery 5.
- **Steps only a person can do**, which the UI guides: macOS privacy prompts on
  the member's own screen; an Apple ID, only when no member has the pinned Xcode;
  and exporting a signing identity from the master's keychain, once.

### Consent for secret files

Delivery 5 decided the following.

- **Detection.** A file in a synced or planned path looks secret by name (`.env*`,
  `*.pem`, `*.p12`, `*.pfx`, `*.key`, `id_*` but not `*.pub`, `.netrc`, `hosts.yml`,
  `*.mobileprovision`, keychains, credential files), by place (inside `~/.ssh`,
  `~/.gnupg` or `~/Library/Keychains`), by permissions (owner-only on the master
  while the synced directory is not: sync itself writes every file owner-only on
  members, so a member turned master would otherwise ask about everything), or by
  content (a private key's header, a known token prefix, or an assignment whose name
  is a secret's or whose value looks generated). A file naming the master's
  absolute home folder is asked about too, since members' home folders differ.
- **Asking.** A change adding or changing such a file waits on the master, its whole
  path with it, and a `secret_consent` attention item names the files by name and
  reason. Its two actions are `elsecall sync consent <path> --confirm <id>` and the
  same with `--always`, the checkbox. Both are `person_only`. Without `--always` the
  next change asks again; with it, the files' later changes sync unasked, and a new
  secret-looking file in the path asks again. Enabling a path from a plan syncs the
  secret-looking files the plan listed, since a person confirmed that plan; a plan
  consents only to a path's first content, so enabling a path that is already
  synced holds its secret changes like any other sync.
- **Consent travels with the role.** It is stored in the synced set's manifest,
  which every member holds as a replica, so a new master has it.
- **Agents cannot confirm by accident; they could on purpose.** The action is
  `person_only`, so a hand-off never runs it automatically, and the CLI sends
  `sync consent --confirm`, and `sync enable --confirm` for a plan that lists
  secret-looking files, only from a terminal on standard input and error, which an
  agent's tool calls do not have (`person_required`). Nothing in the CLI can tell a person
  from a program running as the same user, which could attach a terminal or call
  the local service directly; the guard stops mistakes, not a determined process.
- **Values stay out.** Plans, status, attention items, logs and history name files
  and reasons; content is examined in memory only, as configuration sync already
  reads it to fingerprint it.
- **Candidates from `[consistency] configuration` are not proposed yet.** Those
  paths are the ones jobs depend on, so proposing them for sync is attractive, but
  `workloads.toml` belongs to a repository, which is untrusted: letting it put a
  path into the master's sync plan, even as a question, would let a repository
  steer which of the operator's files get asked about and shipped. Declaring stays
  in the master's `config.toml`; the existing consistency warnings already point at
  the paths that differ. Delivery 6's hand-off may offer the declaration as a
  change for a person to review.

## Deliveries

Each is one pull request, verifiable on its own.

| # | Delivery | Depends on |
| --- | --- | --- |
| 1 | This document, and reading pins: the `PinSource` port, its four adapters, `elsecall tools pins` | Inventory (fleet-tools Slice 1) |
| 2 | A plan per member: pins from the workspaces jobs ran in, minus what the member has installed, minus the exceptions; `fleet-tools.toml` narrowed to exceptions; simulator runtime inventory | 1 |
| 3 | Running commands on a member, and "needs attention" items (in progress in parallel) | — |
| 4 | Installing: mise tools, Xcode copied from the master, simulator runtimes, as admitted work; `elsecall tools apply` | 2, 3 |
| 5 | A warning when a run fails on a missing tool ("not pinned, pin it here"), and consent for secret files | 1, 3 |
| 6 | The automatic loop: drift detection, automatic installs, placement by pins, automatic ready; and the UI | 2, 4, 5 |

The Add Mac flow and the Provision button in the fleet window come from the Add
Mac work (elsecall#73, `crates/elsecall-ui`), which already shows members
awaiting provisioning. Delivery 6 drives that button; it does not rebuild the
flow.

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
  inexact pin adds `resolved` (a version, or null) and `hint`; a mise pin with
  [install options](#install-options) adds `options`, an object by option name. Managers and tool
  names are inventory's: Xcode is manager `xcodes`, tool `xcode`; a runtime is
  manager `simulators`, tool `iOS`.
- `warnings`: managers that could not be read, as `{manager, error}`, with the
  inventory's error codes.

Errors exit 1, except a PATH that is not a readable directory, or is the home
folder, which is `invalid_request` and exits 2:

- `pin_source_unreadable`: a file or directory that resolves outside the
  workspace, passes through more than 32 links, is not a regular file, is larger
  than 1 MiB, is not UTF-8, or cannot be opened.
- `pin_source_invalid`: a file that breaks a rule above, or more files than
  the listing bounds. The message names the file, the key (or line) and the rule.
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

## Delivery 2: a plan per member

`elsecall tools plan [--member NAME]` prints each member's plan from
`GET /v1/tools/plan` on the master. It installs nothing. It waits 170 seconds,
as `tools inventory --fleet` does: the master asks every member at once for its
inventory (150 seconds each) and for its workspaces' pins (20 seconds each).
`--member` plans one member; every member's workspaces still count, and only
the master's inventory and that member's are read. An unknown
name is `invalid_request` (exit 2); on a member that is not the master the
command is `not_master` (exit 1). A machine that has not joined a fleet plans
itself.

The planner joins the two ports. Each pin becomes a want, judged on the master:

- **mise.** An exact pin is planned only when the master has that version. A
  plain version it has not installed (`python = "3.10.4"` before anyone
  installed it, or `node = "22"` with no 22.x) is unresolved, with the hint to
  install it on the master or pin a full version, since Elsecall cannot tell a
  partial version from a full one without the network. An inexact pin is
  planned at the version it resolves to on the master and shown as such
  (`exact: false`, with a hint). A selector (`lts`, `ref:`…) is unresolved.
  When the master cannot read mise, every mise pin is unresolved and the plan
  carries a warning.
- **Xcode.** Two or more segments are planned as written; one segment at the
  newest installed match on the master.
- **Simulator runtimes.** Planned as pinned. A workspace that pins Xcode and no
  runtime also wants the default runtime: iOS at the Xcode's major and minor
  version from Xcode 26 on (26.2 → iOS 26.2, 26.0.1 → iOS 26.0). Before Xcode
  26 the versions differ, so that runtime is unresolved, with the hint to pin it
  in `.elsecall-tools.toml`.

Then, per member, its exceptions replace what the pins want for their tools,
and what its inventory has installed is taken away. A version is installed when
an inventory version compares equal to it; a derived runtime also when an
installed one extends it (iOS 26.0 by 26.0.1), since an Xcode's SDK names only
major and minor. An Xcode exception also replaces the derived runtimes on that
member: a member kept on Xcode 26.0 wants iOS 26.0, not the runtime of the
pinned 26.2, and one that leaves Xcode out wants none. Runtimes pinned in
`.elsecall-tools.toml` stay as pinned.

`data` is:

- `master`, and `global`: `{file, state}` for the master's
  `~/.config/mise/config.toml`, `state` `read`, `missing` or `failed` (with
  `error`).
- `workspaces`: every member's reported workspace, as `{member, workspace,
  last_run_at, state}`, with `files` when `read` or `error` when `failed` (a
  `pins_conflict`, say). A failed workspace's pins are not planned. Another
  member's error text is not copied; the message names the workspace and the
  hint says to run `tools pins` there. A workspace whose report does not
  validate on the master (a version outside its file's grammar, a source that
  is not a workspace file, too many files) fails on its own as
  `protocol_error`; the member's other workspaces still count. Only a report
  with an unusable workspace name or more than 64 workspaces is refused whole.
- `unread_members`: members whose workspaces could not be listed, with
  `error`, whose codes are the fleet inventory's (`member_unreachable`,
  `member_timeout`, `version_skew`…). A member on an older build answers
  `not_found` or `version_skew`, and the hint says to upgrade the fleet.
- `warnings`: the master's managers that could not be read, as `{manager,
  error}`.
- `members`, this member first, then roster order. A member whose inventory
  could not be read is `{member, state: "unavailable", error}`, with the fleet
  inventory's codes. Otherwise `state` is `planned` with:
  - `steps`: versions to install, ordered by manager (mise, xcodes,
    simulators), tool and version: `{manager, tool, version, exact, reasons,
    options}`, and `hint` when `exact` is false. `options` are the
    [install options](#install-options) the pins agree on, `{}` for most.
    An entry of `unresolved` for options that differ names the planned version,
    not each pin as written; its `reasons` keep those.
  - `unresolved`: requests shown and not planned, grouped: `{manager, tool,
    version, hint, reasons}`; `version` is the pin as written, or null for a
    default runtime.
  - `exceptions`: each exception that applies, as `tools exceptions` prints
    it, plus `pinned`, the versions the pins asked for that tool (empty when
    nothing pins it, and then nothing is planned).
  - `installed`: wanted versions the member already has, `{manager, tool,
    version}`.
  - `unread_managers`: every manager the member could not read, with `error`.
    Their steps are unknown and left out.

A reason is `{kind: "pin", member, workspace, source, file, version,
environment}` (`workspace` is null for the master's global config, and
`version` is as written), `{kind: "default_runtime", member, workspace,
xcode}`, `{kind: "exception", file: "fleet-tools.toml", key}`, or
`{kind: "exception_runtime", file: "fleet-tools.toml", key, xcode}` for the
runtime of a member's own Xcode.

### Evidence

On the host, with the services under scratch homes so nothing was installed in
the operator's account: a master, `mac-mini`, reading this machine's real mise
installs (through a link) and a copy of its global config, and a bare Mac
enrolled beside it with no mise, no Xcode and no `simctl`. A run started in
each real workspace on the master: `acme-ios` and this repository.

- Both workspaces read: `acme-ios` from `mise.toml` and `mise.signing.toml`,
  this repository from `mise.toml`. Neither has `.xcode-version`, so there is
  no Xcode step and no runtime.
- The master's plan has no steps: its 16 wanted versions are all installed.
- The bare Mac's plan has 16 mise steps for both repositories and the global
  pins: 12 exact, and 4 at the version their inexact pin resolves to on the
  master (`bun` and `sentry-cli` `latest` → 1.3.14 and 3.8.0, `java = "zulu-17"`
  → zulu-17.54.21, `node = "22"` → 22.19.0, the latter also pinned exactly by
  `acme-ios`). Nothing unresolved, no warnings.
- The plan took 0.7 and 1.0 seconds in two runs, mise's `ls` included.

## Delivery 4: installing

`elsecall tools apply [--member NAME] [--dry-run] [--wait]` carries out the plan
on the master. It plans the fleet exactly as `tools plan` does
(`POST /v1/tools/apply` with `{member}`, `not_master` elsewhere,
`invalid_request` for an unknown `--member`) and then sends each planned member
its steps, the members with nothing to install too: their apply has no steps and
finishes at once, so their last apply's failures stop standing. The command
waits 200 seconds, for the plan's 150 and a member's 20. The sends run on their
own in the service, so a caller that gives up never leaves some members asked
and others not. `--dry-run` sends nothing and prints the plan: it
reads `GET /v1/tools/plan`, so its `data` is the `tools plan` document, field
for field. Agents may run it: a tool plan follows
from pins, so it needs no person, and nothing in it is `person_only`.

A member answers as soon as its apply has started, and installs its steps as its
own work (below). Without `--wait` the command returns then, and `data` is
`{master, members}`, one entry per planned member in plan order:

- `{member, state: "started", apply, unresolved}`: `apply` is the member's new
  apply (below), every step `queued`; `unresolved` counts the plan's
  unresolved requests, which are not installed.
- `{member, state: "nothing_to_install", unresolved}`: the plan has no steps;
  the member recorded an empty apply.
- `{member, state: "unavailable", error}`: its inventory could not be read, so
  it has no plan, with the fleet inventory's codes.
- `{member, state: "refused", error}`: it did not start, as `apply_in_progress`
  (an apply already runs there), `plan_too_large` (more than 256 steps or
  60 KiB, beyond what one request to a member carries), `version_skew`,
  `member_unreachable` and the like. Another member's text is not copied.

`elsecall tools applied [--member NAME] [--wait]` reads each member's latest
apply (`GET /v1/tools/apply`), this member first, on any member: `{members:
[{member, state, apply?, error?}]}` with `state` `none` (it never applied a
plan), `running`, `finished` or `unavailable` (with `error`). `--wait` reads
again every two seconds until no member it read is `running`; `tools apply
--wait` does that for the members it started, replacing each of their entries
with its finished read and keeping every other entry as it was. Both exit 0 when a step failed:
that is data, in `apply` and in the fleet's attention.

An `apply` is `{started_at, finished_at, steps}`, `finished_at` null while it
runs. Each step is `{manager, tool, version, options, state}` in plan order,
with `state`:

- `queued`, or `running` with `since`;
- `installed` with `at`;
- `skipped` with `reason` `not_supported`: its manager does not install yet;
  it is skipped when the apply starts, and nothing runs;
- `failed` with `stage`, `code` and `at`.

A failure's `stage` is the part of the step that failed (`admission`,
`install`, `apply`) and its `code` why. They are never what a command printed:
an install's output is discarded, never captured, so a token in a download URL
never reaches a record, a status, an item or a log. The codes so far:

| Stage | Code | When |
| --- | --- | --- |
| `admission` | `install_not_admitted` | The member's scheduler refused the step, as while it stops. |
| `download` | `xcode_missing` | No Xcode installed on the member knows the runtime. |
| `download` | `xcode_unreadable` | The member's Xcodes could not be read. |
| `download` | `runtime_download_failed` | `xcodebuild -downloadPlatform` exited with an error. |
| `download` | `runtime_download_timeout` | It ran for an hour. |
| `privileges` | `xcode_privileges_missing` | The helper or the sudoers rule is not installed, or the helper is another build's. |
| `copy` | `xcode_not_on_master` | The master has no Xcode of that version, or the step runs on the master. |
| `copy` | `xcode_copy_failed` | The copy could not be unpacked. |
| `copy` | `artifact_cut`, `artifact_busy`, `artifact_unreachable`, `artifact_unsupported` | As for mise. |
| `place` | `xcode_not_verified` | The copy did not verify as Apple's Xcode of that version. |
| `copy` | `xcode_unsupported` | The version is not one the helper places. |
| `place` | `xcode_place_failed` | The helper could not place it, as when that version is already in the store. |
| `license` | `xcode_license_failed` | Placing it, its license or first-launch tasks failed; nothing was placed. |
| `select` | `xcode_select_failed` | `xcode-select` failed. |
| `bootstrap` | `artifact_on_master` | The member without mise is the master, which has nothing to copy it from. |
| `bootstrap` | `artifact_unavailable` | The master has no mise to copy. |
| `bootstrap` | `artifact_unreachable` | The master could not be reached, or this member has no roster. |
| `bootstrap` | `artifact_busy` | The master was already sending two copies. |
| `bootstrap` | `artifact_unsupported` | The master runs another build. |
| `bootstrap` | `artifact_cut` | The copy did not arrive whole. |
| `bootstrap` | `mise_copy_failed` | The copy could not be written or moved into place. |
| `bootstrap` | `mise_copy_unusable` | The copied mise did not say its version. |
| `install` | `mise_unusable` | mise could not be started, or `HOME` is not set. |
| `install` | `tool_install_failed` | `mise install` exited with an error. |
| `install` | `tool_install_timeout` | It ran for 30 minutes. |
| `admission` or `apply` | `apply_interrupted` | The service stopped while the step waited or ran. |
| `apply` | `apply_failed` | The apply stopped unexpectedly; its unfinished steps fail with it. |

### Attention: an Xcode no member can copy

A step that failed as `xcode_not_on_master` is not a `provisioning_step_failed`
item, since retrying cannot fix it: every member's fleet view derives one
`xcode_apple_id_required` item per Xcode version some member's latest apply
could not copy, about the master, where a person gets it:

```json
{
  "id": "xcode_apple_id_required:mac-mini:26.4",
  "member": "mac-mini",
  "category": "xcode_apple_id_required",
  "subject": "26.4",
  "code": "xcode_not_on_master",
  "message": "Xcode 26.4 is pinned, and the master, mac-mini, has none to copy: members get Xcode only from the master.",
  "hint": "A person downloads Xcode 26.4 onto mac-mini with an Apple ID (xcodes install 26.4, or developer.apple.com/download), or copies it there from a Mac that has it; then run elsecall tools apply on mac-mini.",
  "since": "2026-11-20T09:30:00.000Z",
  "actions": [{
    "summary": "Once Xcode 26.4 is installed on mac-mini, apply the plan again to copy it to the members that need it.",
    "command": ["elsecall", "tools", "apply"],
    "run_on": "mac-mini",
    "person_only": true
  }]
}
```

`since` is the earliest such failure. The action is `person_only`: only a person
signs in to Apple. The item clears once no member's latest apply failed that
way, as after the next apply finds the Xcode on the master.

### On the member

`POST /peer/v1/tools/apply` takes `{steps}`, 1 to 256 of `{manager, tool,
version, options}`. Only the member's recorded master may send it (`not_master`,
403, from anyone else), and each step is checked against its manager's grammar
before anything runs, as a member's pin report is on the master: a tool or
version that could read as an option, a mise tool holding brackets, a mise
version that is a selector (`latest`, `ref:`, `path:`…), a version outside
Xcode's or a runtime's digits and dots, options on anything but mise, or an
option Elsecall does not carry is `invalid_request`. 0 to 256 steps, at most
the listener's 64 KiB. One apply runs at a time; another is
`apply_in_progress` (409). `GET /peer/v1/tools/apply` is its latest
apply, `{apply}`, null when there is none; any enrolled member may read it. Both
are refused across builds.

The member runs the steps one at a time, in order. Each asks the member's own
scheduler for a deferrable, pinned run of 2 cores labelled
`elsecall tools install`: it starts only while the member has room beyond its
reserve and no interactive work waits, so it never takes room from running work,
and it shows in `elsecall runs` and `explain` like any job, as Elsecall's own
work. After an hour of waiting it starts at its deadline anyway, still only
when it fits. Each command the step runs is started in its own process group,
with no input and its output discarded, and bound to the run as its holder; when
the step times out or is dropped while it runs, the group is killed; a command
that exits by itself leaves its group alone, since the group's ID may then be
reused. The run ends exactly once, with exit 0 when the step installed and 1
otherwise, also when the step is dropped or its task panics: its room is never
held past the step. A panicking apply finishes with its unfinished steps failed
as `apply_failed`, so the member is never stuck `apply_in_progress`. One failed step does not stop the
next: tools do not depend on each other. Nothing is ever uninstalled.

The latest apply is kept in `tools-apply.json` in the member's data directory,
owner-only, after every change. Running work does not survive a restart, so an
apply that was running when the service stopped reads back finished, its
unfinished steps `failed` with `apply_interrupted`, and is kept that way at
once, so their times do not move with later restarts.

**mise tools.** A step is `mise install '<tool>[<options>]@<version>'`: the
version named explicitly, never `mise install` in a workspace, so no
repository's config is read and no hook, task or `postinstall` a repository
wrote runs. The backend mise uses for the tool does run: a tool that builds from
source, or that an asdf or vfox plugin installs, runs that upstream's code, as
it did when the master installed it. The
[install options](#install-options) go in mise's bracket syntax, which their
values cannot end. mise runs as the inventory runs it: the executable the
inventory finds (`ELSECALL_MISE` or the usual places), in the home folder, with
`MISE` and `MISE_*` removed and `MISE_YES=1` set, for up to 30 minutes. A tool a
mise plugin backs needs the plugin on the member, as on the master.

**A member without mise**, as a bare Mac is, first copies the master's: the
executable the master's inventory runs, after following links, over the
members' channel (below), within 10 minutes. The copy is written as `mise` in a
new private folder beside where the member's inventory looks first
(`ELSECALL_MISE` when set, otherwise `~/.local/bin/mise`), since mise behaves as a
shim under any other name (a copy named otherwise refuses `--version`, checked
with mise 2026.10). It is moved into place only once it arrived whole and
`mise --version` runs within 30 seconds, as part of the step's admitted run; the
folder is removed however it ends. This is the first mise step's `bootstrap`
stage, and the steps after it find mise in place; when it fails, the apply's
later mise steps fail with the same stage and code without copying again. Copying, not
downloading, needs no network and gives the member the master's own version.
Both mise builds Elsecall has seen, mise's own and Homebrew's, link only
system libraries, so the master's runs on a member. The copy is held in memory
until it is written, up to 1 GiB; Homebrew's mise 2026.11 is 237 MB. In a macOS
VM, a member without mise copied Homebrew's from the master, checked it and
installed a tool with it within one 133-second step.

**Xcode** is copied from the master's installed bundle, with no Apple ID. A step
runs these stages, all part of its admitted run:

1. `privileges`: Elsecall's helper and this account's sudoers rule are in
   place (`elsecall tools privileges`, below); otherwise nothing is copied,
   since nothing could be placed, and the item asks a person to install them.
2. `copy`: the master streams its installed Xcode of exactly that version over
   the members' channel (`POST /peer/v1/tools/artifact/xcode/<version>`, the
   bundle as `ditto -c` archives it, keeping links, modes and attributes), into
   `ditto -x`, which unpacks it as this account into `~/Library/Application
   Support/Elsecall/xcode-staging/Xcode-<version>.app`. The copy may take two
   hours. A master without that Xcode, and a step on the master itself, fail as
   `xcode_not_on_master`: members get Xcode only from the master, so a person
   gets it there, with an Apple ID (`xcode_apple_id_required`, below).
3. `place`: the helper places the staged bundle in `/Library/Elsecall/Xcodes`
   and accepts its license (below), so a placed Xcode is a ready one: when the
   license fails (stage `license`) nothing is placed, and the next apply tries
   again. The staged copy is removed whatever happens. This account then links
   it as `/Applications/Xcode-<version>.app`, unless that name is taken or it
   may not write there; the inventory finds it in the store either way.
4. `select`: the helper selects it when `xcode-select -p` names no Xcode
   (Command Line Tools only, as on a bare Mac). A member that selects another
   Xcode keeps it; moving a selection is delivery 6's drift work. A failed
   selection is reported once: the Xcode is installed, so the next plan has no
   step for it.

A version the helper would refuse, not two or three numbers or before 16.0,
fails as `xcode_unsupported` before anything is copied, and a step on the master
itself as `xcode_not_on_master` before any privilege is asked for.

`ELSECALL_XCODE_ADMIN`, an absolute path, replaces `sudo -n <helper>` and the
privileges check, as the other tool overrides do in every build; it runs as
this account, so it grants nothing, and the tests use it for a fake helper.

**Simulator runtimes.** A runtime step is `xcodebuild -downloadPlatform
<platform> -buildVersion <version> -architectureVariant arm64`, as Xcode's own
Components settings download one, from Apple and with no Apple ID. The variant
is named because a version alone looks for a universal image, which Apple does
not offer for every version: with Xcode 26.2, `-buildVersion 26.2` alone
answered `iOS 26.2 is not available for download`, and with `arm64` it
downloaded. Every member runs Elsecall's Apple silicon build. It runs the `xcodebuild` of an
Xcode installed on the member, with that Xcode as `DEVELOPER_DIR`, in `/`, for up
to an hour: the Xcode of the runtime's major and minor version (iOS 26.2 through
Xcode 26.2), else the oldest later one, since an earlier Xcode does not know the
runtime; with none, the step fails as `xcode_missing` rather than running a
download bound to fail. A runtime before 26, numbered apart from Xcode, uses the
selected Xcode, else the newest. A plan lists Xcode before runtimes, so the
default runtime of a pinned Xcode finds it installed. A runtime counts as
installed when `simctl` reports its version, whatever build the download chose
(26.2 came as 23C52 where Xcode 26.2 shipped 23C54), and a pinned trailing zero
does not count (`26.2.0` is 26.2). A download cut short at its hour may leave a
partial image, which `xcodebuild` replaces on the next try. Runtimes
are downloaded rather than copied from the master: the master keeps no runtime
image it could send, only what `simctl` mounted.

**Xcode's root steps.** Placing an Xcode bundle, selecting it and accepting
its license need root. Elsecall never gets a general root shell for them: once
per member, a person runs `elsecall tools privileges install`, which asks for
an administrator's password (on the terminal, or piped in on standard input)
and installs two files as root:

- the helper, `/Library/PrivilegedHelperTools/dev.elsecall.xcode-admin`,
  rendered for this account, owned by root and not writable by anyone else. Its
  folder is made root's when missing; one that exists keeps its mode and must be
  root's and writable by no one else, or nothing is installed;
- the drop-in, `/private/etc/sudoers.d/elsecall`, mode 0440, checked by `visudo`
  before it is moved into place, with exactly one rule:

  ```
  devbox ALL = (root) NOPASSWD: /Library/PrivilegedHelperTools/dev.elsecall.xcode-admin ^(place|select|license) [0-9]+[.][0-9]+([.][0-9]+)?$
  ```

  for the account Elsecall runs as. sudo matches the whole argument list
  against that pattern (sudo 1.9.10 and later; macOS 26 ships 1.9.17), so this
  account may run the helper with `place`, `select` or `license` and a version
  of two or three numbers, without a password, and nothing else. The dot is
  `[.]`: sudoers removes backslashes from a word, so `\.` would match any
  character.

The account is usually an administrator, and an administrator may rename
anything in `/Applications` (`root:admin`, mode 0775): a helper that read or
wrote a bundle there could be raced into running the account's own code as
root, or writing where the account points. So placed bundles live in
`/Library/Elsecall/Xcodes`, a tree only root writes, and root writes nowhere
else; the inventory lists Xcodes there as well as in `/Applications`, and the
installer links each one from `/Applications` as the account, for people and
tools that look there. The helper runs again with an empty environment in `/`
before anything else, since sudo on macOS keeps `HOME` and the caller's folder,
and checks its arguments again, including a version of 16.0 or later. Then, as
root:

- `place VERSION` copies (never moves, so the account keeps no handle on what
  root then runs) the account's staged `~/Library/Application
  Support/Elsecall/xcode-staging/Xcode-VERSION.app` into a root-only folder in
  the store, without extended attributes or ACLs and with the file flags an
  account can set (`uchg`, `uappnd`) cleared, makes it root's,
  strips set-ID bits and others' write bits, and moves it to
  `/Library/Elsecall/Xcodes/Xcode-VERSION.app` only once it is Xcode of that
  version (`CFBundleIdentifier`, `CFBundleShortVersionString`) and
  `codesign --verify --deep --strict -R='anchor apple and identifier
  "com.apple.dt.Xcode"'` accepts it and its license is accepted (below), so a
  placed Xcode is a ready one and a failure (`70`) leaves nothing placed for the
  next try to skip. It never replaces an existing bundle, and places one version
  at a time (`75` while another placing of it runs).
- `select VERSION` runs `xcode-select --switch` on a bundle it placed: in the
  store, root's, and writable by no one else.
- `license VERSION` accepts a placed bundle's license again, for a person
  repairing a member.

Accepting a license runs the bundle's `xcodebuild -license accept`, and
`-runFirstLaunch` only when it is the newest Xcode placed: first-launch tasks
install system packages, and an older release's would replace newer ones.

`elsecall tools privileges` prints `{account, helper, drop_in, ready}`, with
`helper` `current` (this build's for this account, root's), `stale` or
`missing` and `drop_in` `present` or `missing` (only root may read it, so its
content is not checked here); `ready` is both. `elsecall tools privileges
remove` removes both files; an Xcode the helper placed stays, since nothing is
ever uninstalled. Both commands are a person's: they ask for an administrator's
password.

**Copying between members.** A member copies an installed tool from another
over `POST /peer/v1/tools/artifact/<artifact>` (now `mise`), upgraded to
`elsecall-artifact/1`. Both members sign the channel for the artifact it carries
(`tools-artifact:mise`), so a channel opened for one cannot carry another. The
source answers 404 `artifact_unavailable` before upgrading when it has none, and
503 `artifact_busy` while it already sends two. After the upgrade it streams the
bytes as they are read, as content frames, then an empty frame, then a trailer
with their count and SHA-256, sent only once the whole file was read; the
receiver writes them as they arrive and keeps them only when both match. A
minute without a frame ends the copy, and the source drops one that has not
ended within 10 minutes.
Any enrolled member may ask: what travels is installed software, not a
secret.

### Attention: a failed step

The member's status carries its latest apply as `provisioning`: `{state
(running or finished), started_at, finished_at, steps, installed, failed}`,
where `failed` lists each failed step as `{manager, tool, version, stage, code,
failed_at}`; `elsecall fleet` shows it under each member's `status`. Every
member's fleet view derives one `provisioning_step_failed` item per failed step
of every member's latest apply:

```json
{
  "id": "provisioning_step_failed:macbook-pro:mise:node@22.19.0",
  "member": "macbook-pro",
  "category": "provisioning_step_failed",
  "subject": "mise:node@22.19.0",
  "code": "tool_install_failed",
  "message": "Installing mise node 22.19.0 on macbook-pro failed at its install stage: mise install exited with an error.",
  "hint": "Install node 22.19.0 with mise on macbook-pro, with the options elsecall tools plan lists, to see why; fix the cause, then run elsecall tools apply --member macbook-pro on the master to retry.",
  "since": 1763600000000,
  "actions": [{
    "summary": "Apply macbook-pro's plan again, which retries this step.",
    "command": ["elsecall", "tools", "apply", "--member", "macbook-pro"],
    "run_on": "mac-mini",
    "person_only": false
  }]
}
```

A step that failed as `xcode_privileges_missing` has a first action for a person
on the member, before the retry:

```json
{
  "summary": "On macbook-pro, let Elsecall place, select and license Xcode: installs its helper and a sudoers rule for it alone, with an administrator's password.",
  "command": ["elsecall", "tools", "privileges", "install"],
  "run_on": "macbook-pro",
  "person_only": true
}
```

The `subject` is `<manager>:<tool>@<version>`, and `since` is when the step
failed. The item stands until the member's next apply, which retries the step,
or no longer needs it and ends it with an empty apply. The text is written by the reading member from the code;
another member's report is read leniently, and a failed step it cannot read is
left out.

### Evidence: mise tools

In a macOS VM (`docs/e2e-vm.md`), a fleet of one named `mac-mini` running this
build, with mise 2026.11.3: a workspace pinning `github:nextest-rs/nextest` with
`version_prefix = "cargo-nextest-"` and a `postinstall`, and an exception giving
the VM 0.9.149 so its plan has a step. `tools plan` listed the step with
`options: {version_prefix: "cargo-nextest-"}`; `tools apply --wait` installed it
in 5 seconds, mise recorded the option in the install's own `.mise.backend.toml`,
the `postinstall` did not run, `elsecall runs` showed one `elsecall tools
install` run that exited 0, and the next `tools apply` had nothing to install.
On the host, `tests/tools_apply.rs` covers two enrolled members with a fake mise:
the dry run equals the plan, steps run in the home folder with `MISE_*` removed,
a failed step becomes a `provisioning_step_failed` item in the master's fleet
view, and a retry that fails again keeps it.

### Evidence: Xcode's root steps

In a macOS VM (macOS 26.6, sudo 1.9.17), with `devbox` as the member account:

- As a standard account with only the drop-in, `sudo -n` ran the helper with
  `place 26.2`, `select 26.0.1` and `license 26.2`, and refused `place 26`,
  `place 26.2.1.4`, `place 26.2 x`, `place 26.2;id`, `place ../x`, `remove 26.2`,
  the helper with no arguments, and `/usr/bin/true`.
- As an administrator whose sudo asks for its password: `elsecall tools
  privileges` read `ready: false`; `elsecall tools privileges install` with the
  password piped in installed both files and read `ready: true`, `helper:
  current`. Without a password, sudo then still refused `place 26x2`, `place 26
  2`, `place 26`, `place 26.2.1.4`, `license 26.2 x`, `remove 26.2`, `--clean
  place 26.2` and `/usr/bin/true`.
- A real Xcode 26.2 staged with one byte appended to its `version.plist` was
  refused (exit 65) and nothing was placed. Staged intact, it was placed in 62
  to 84 seconds, most of them the deep signature check: in
  `/Library/Elsecall/Xcodes`, owned by `root:wheel` with no file set-ID,
  writable by others or not root's. Placing it again was refused (73).
- Clearing every file flag of the copy broke its signature: Xcode's files are
  stored compressed, and `chflags 0` clears that flag too. Dropping extended
  attributes and clearing only `uchg` and `uappnd` keep it valid, checked step
  by step.
- `license 26.2` took 5 to 7 seconds, `select 26.2` pointed `xcode-select -p` at
  the placed bundle, and `xcodebuild -version` printed Xcode 26.2.

On the host, unit tests run the helper rendered with a scratch layout and fakes
for `codesign`, `xcode-select` and `xcodebuild` (including that none of the
caller's environment reaches them), and check the drop-in with `visudo -c`;
nothing is installed in the operator's account.

### Evidence: a bare member provisioned from the master

In a macOS VM, two members on one Mac, each an account running its own `elsecall
serve` on loopback: the master `mac-mini` with Xcode 26.2 and nextest 0.9.150
(installed through its mise with `version_prefix`), and `macbook-pro`, an
administrator account with no mise (`ELSECALL_MISE` named a missing file), no
Xcode and no runtimes. A workspace on the master pinned Xcode 26.2 in
`.xcode-version` and nextest with `version_prefix` in `mise.toml`.

- `macbook-pro`'s inventory read mise, xcodes and simulators all `missing`;
  `elsecall tools privileges install`, with its password piped in, read `ready:
  true`.
- Its plan: nextest 0.9.150 with `options: {version_prefix: "cargo-nextest-"}`,
  Xcode 26.2 and the default runtime iOS 26.2.
- `elsecall tools apply --member macbook-pro --wait` on the master installed all
  three in 876 seconds, as three `elsecall tools install` runs that exited 0:
  - nextest, 133 seconds, copying the master's mise (Homebrew's, 237 MB) first;
  - Xcode, 725 seconds: the copy streamed at about 12 MB/s over the sealed
    channel and both `ditto`s, then placing (with its signature check) took
    about a minute; it ended in `/Library/Elsecall/Xcodes`, linked from
    `/Applications` and selected, since only the Command Line Tools were;
  - iOS 26.2, 16 seconds.
- Afterwards its inventory listed all three, its plan had no steps, and the
  fleet's attention was empty.
- With `.xcode-version` changed to 26.4, which the master lacks, the Xcode step
  failed at `copy` as `xcode_not_on_master` and the runtime at `download` as
  `xcode_missing`; the fleet showed one `xcode_apple_id_required` item for
  `mac-mini`, version 26.4, its action `person_only`.

That run's helper linked the bundle from `/Applications` as root and accepted
the license in a separate step; the final helper, run again as the same
administrator account, placed and licensed Xcode 26.2 in 62 seconds, wrote
nothing in `/Applications`, refused Xcode 15.4 (64), and selected it.

### What delivery 6 builds on

Delivery 4 installs only when asked; the automatic loop, drift detection,
placement by pins, marking a member ready and the UI are delivery 6. It can
build on:

- **One command per member, safe to repeat.** `elsecall tools apply --member
  NAME` installs whatever the plan lists and nothing else, never uninstalls,
  and with nothing to install records an empty apply that clears the member's
  failures. A member already applying answers `apply_in_progress`.
- **A readable outcome.** `elsecall tools applied [--member NAME] [--wait]`,
  and each member's `status.provisioning` in `elsecall fleet`: `running` or
  `finished`, how many steps, how many installed, and each failed step's stage
  and code. A member is fully provisioned for the current pins when its plan has
  no steps and no unresolved requests.
- **Attention items by category.** `provisioning_step_failed` is an agent's to
  retry, except its `person_only` first action for Xcode privileges;
  `xcode_apple_id_required` is a person's. A hand-off can accept the first
  category's retries automatically and must never accept the second's.
- **The person steps, as commands.** `elsecall tools privileges install` on a
  member, with its password typed or piped in, and an Apple ID download on the
  master. Add Mac already takes the new Mac's administrator password for its
  own root steps; offering the first there is delivery 6's to wire.

## Delivery 6: keeping members in step

Delivery 6 lands as several pull requests: the automatic loop and automatic
ready first, then placement by pins, then the window. This section grows with
each.

### The loop

The master's service follows the fleet by itself: it notices what changed,
plans the members it affects, and applies their plans, with no command. It is
the same plan and the same apply as `tools plan` and `tools apply`; nothing in
the loop installs anything those would not. It runs only on the master, so a
`fleet move-master` moves it, and it never applies to the master itself: the
master is what every member is kept in step with, and a pin it cannot satisfy
is a person's to fix there (`tools apply --member` still installs the master's
own plan when asked). A machine that has not joined a fleet has no other member,
so its loop does nothing.

**Noticing is cheap.** Planning reads every member's inventory, which runs mise
and `simctl`. The loop does not plan to find out whether anything changed: every
minute it asks each member for its **drift signal** (`GET
/peer/v1/tools/drift`, enrolled members only, 10 seconds), and computes its own.
A signal is two SHA-256 hashes, read without running any tool:

- **pins**: the pins of the member's recent workspaces, read as data exactly
  as `GET /peer/v1/tools/workspace-pins` reads them, without when each last
  ran; on the master also its global mise config's pins and
  `fleet-tools.toml`. Every member's plan reads them.
- **tools**: where its tools live, by name, size and modification time, one
  level deep, never their content: mise's installs (`installs/` in the data
  folder mise uses as the inventory runs it, with `MISE_*` removed:
  `$XDG_DATA_HOME/mise`, else `~/.local/share/mise`), the mise executables
  the inventory would try, the `Xcode*.app` bundles in `/Applications` (or
  `ELSECALL_XCODE_APPLICATIONS`) and `/Library/Elsecall/Xcodes`, and the
  simulator runtimes' `Images` (stamped) and `Volumes` (by name only, since
  mount points change as they mount) in `/Library/Developer/CoreSimulator`;
  plus its build and whether its Xcode privileges are ready. Dotfiles and
  other applications do not count, so an application updating itself is no
  change. A member's own plan reads its tools, and the master's resolve every
  member's pins.

The signal also carries whether the member's Xcode privileges are ready and the
state of its latest apply (`running` or `finished`, and when it started), read
before the hashes, so an apply that ends meanwhile is seen at the next look. No
path, pin or version leaves the member in it, only the hashes: the plan reads
those when it runs. A member whose signal cannot be read (asleep, unreachable,
on another build) is skipped and asked again next minute; its last pins still
count. With nothing changed, a minute costs each member a signed request and a
few directory reads, and the master nothing else.

While an apply runs on any member, the loop looks every five seconds instead,
so the member whose apply finished is checked, and marked ready, seconds after
its last step. The minute comes back once no apply runs.

**Acting.** A member's plan depends on every member's pins, the master's tools
and its own tools. It is due when:

- **any of those changed** since the loop last planned it: a pin changed in a
  workspace runs started in, on any member, or the master's global config or
  exceptions; a tool appeared on or disappeared from the member, as when
  someone removes one by hand; a tool appeared on the master, which an
  unresolved pin or an Xcode that needed an Apple ID may be waiting for;
  privileges were installed; the member was upgraded;
- **it is new**: enrolled since the loop last looked, as Add Mac enrols a Mac;
- **its latest apply finished**, whoever started it, so the loop checks the
  result (and marks the member ready, below);
- **a person or agent asked**: `elsecall tools provision MEMBER` on the master,
  which the window's Provision button runs;
- **its last plan, apply or ready mark failed**, after a wait that starts at a
  minute and doubles up to ten, so a lasting failure costs a plan every ten
  minutes, not every minute;
- **six hours passed** since it was last planned, which catches anything the
  signal cannot see.

A change waits until the signals have stayed the same for one more minute, and
at most ten minutes in all, so a branch switch or a `mise use` followed by its
install is planned once, at the end. Asking skips the wait, and an ask that
arrives while a plan for the member runs is kept for the next. The loop then
plans the due members at once (one `tools plan` of those members, reading
every member's workspaces but only their inventories and the master's), and
for each:

- **an apply still runs there**: nothing; the loop checks again when it finishes;
- **the plan has steps**: it applies them, as `tools apply --member` would, with
  the exceptions under "Failed steps" below;
- **no steps**, while the member's latest apply still shows failures: an empty
  apply, which ends them, as `tools apply` does;
- **no steps and nothing standing**: nothing.

A step the member's latest apply installed that the plan still lists (its
inventory does not see what was installed) is sent again only when something
changed or someone asks, not after every apply.

Tool plans follow from pins, so the loop applies them without asking anyone.
Installs are admitted work on each member, as from `tools apply`: a member
busy with jobs installs later, never at their expense. Nothing is ever
uninstalled: a version nothing pins any more stays, and only the plan stops
asking for it.

### Failed steps

A failed step stands as its attention item until the member's next apply, as in
delivery 4. The loop does not resend it with every apply, and it never retries
on its own what only a person can fix. It sends a member's plan with each step
that failed in the member's latest apply either **retried** or **held**: a held
step is not run, and the member records it failed again with its own stage,
code and time, so its item stands with the same `since`. The loop sends a
member nothing at all while what it would send matches the latest apply.

- `xcode_privileges_missing` is held until the member's signal says its
  privileges are ready, which `elsecall tools privileges install`, a person's
  step, makes them.
- `xcode_not_on_master` is held until the master's inventory has that Xcode,
  which only a person with an Apple ID gives it. Its item is
  `xcode_apple_id_required`, and the loop leaves it for that person.
- Any other failure, which retrying can fix (a download that broke, a member
  that was stopping), is retried once something changed since the apply that
  failed it finished (either signal), six hours after it failed, or when asked;
  otherwise it is held. So a tool that fails to build is not rebuilt every few
  minutes.

`tools provision`, asked by a person or an agent, retries every failure but the
two held for a person while their cause stands, since retrying those would fail
at once. `tools apply` is unchanged: it sends the whole plan and retries
everything, as a repair a person or agent chose.

The member route carries held steps: `POST /peer/v1/tools/apply` takes
`{steps, held}`, `held` 0 to 256 of `{manager, tool, version, options, stage,
code, failed_at}`, checked like a step, with `stage` and `code` lowercase words
of at most 64 characters, and both lists together at most 256 steps naming no
step twice; the master refuses a plan over that as `plan_too_large` before
sending it. Held steps go first in the record, already `failed`.

### Ready by itself

A member awaiting provisioning becomes ready once the loop plans it and its plan
has no steps, no unresolved requests and no unread managers, and every pin was
read: no member's workspaces went unreported, no workspace failed to read, and
the master's global config was read. Everything the fleet's pins ask of it is
then installed, or an exception says it differs on purpose. The loop marks it
ready as `elsecall fleet ready` does, through a roster change delivered to every
member, and never forces it:

- **refused**, as while a member is unreachable: the member is planned again
  after the failure's wait, and marked only if it is still complete, so a
  member that was asleep and comes back with pins of its own is counted first;
- **recorded, but not delivered to every member**: the same mark is delivered
  again after the wait until every member has it.

Nobody runs `fleet ready` by hand, though it still works, for a member a person
wants to use before it is complete.

A fleet whose workspaces pin nothing makes a new member ready at its first
plan, a minute or two after Add Mac. A pin the master cannot resolve (an
unresolved request) keeps every member awaiting provisioning waiting, since
none of them can be told complete: the plan names it with its hint. Ready is
only ever set, never taken back: a ready member that later lacks a tool gets it
installed, and placement by pins (below) keeps work off it meanwhile.

The `awaiting_provisioning` item says so: its action is now `elsecall tools
provision <member>` on the master, which plans and applies at once, not
`fleet ready`.

### Following the loop

`elsecall tools provisioning [--member NAME]` on the master prints the loop's
view (`GET /v1/tools/provisioning`), from memory, without asking any member:

```json
{
  "master": "mac-mini",
  "following": true,
  "members": [{
    "member": "macbook-pro",
    "state": "applying",
    "readiness": "awaiting_provisioning",
    "planned_at": "2026-11-20T09:31:12.000Z",
    "signal_at": "2026-11-20T09:40:02.000Z",
    "plan": {"steps": 3, "unresolved": 0},
    "held": [],
    "error": null
  }]
}
```

`state` is one of:

- `master`: the master itself, which the loop never applies to;
- `unknown`: not planned yet, as just after the service started;
- `waiting`: a change was seen and the loop waits for it to settle;
- `applying`: an apply runs there;
- `in_step`: its last plan has no steps and nothing unresolved;
- `needs_person`: its last plan holds steps only a person can unblock (`held`
  lists them as `{manager, tool, version, code}`);
- `behind`: its last plan is not complete (steps, unresolved requests, unread
  managers or pins), or the last plan, apply or ready mark failed (`error`
  says why, such as `apply_in_progress` or an unreadable inventory);
- `unreachable`: its signal could not be read (`error`).

`plan` is null before the first plan. On any other member the route is
`not_master`, as `tools plan` is. `elsecall tools provision MEMBER` (`POST
/v1/tools/provisioning` with `{member}`) marks the member due now and returns
at once with its entry; agents may run it, since it applies only a tool plan.
Naming the master is `invalid_request`.

The loop writes one service log line per plan and per action it takes,
`provisioning_planned`, `provisioning_applied` and `provisioning_ready`, naming
members and counts, never a path or version.

### Person steps in Add Mac

Two steps of provisioning need a person.

- **Xcode privileges** on a member need its administrator's password once.
  Add Mac already takes that password, so it offers the step there: "Let
  Elsecall install Xcode on this Mac", selected for a new Mac, installs the
  helper and its sudoers rule at the end of the add, through the same SSH
  connection's sudo, with the script the master's build renders
  ([adding a Mac](add-mac.md), step 11). The loop then sees the member's
  privileges ready in its signal and copies Xcode there with no one present.
  A member added without it gets the `provisioning_step_failed` item for its
  Xcode step, whose first action, `person_only`, is `elsecall tools privileges
  install` on that member.
- **An Apple ID** is needed only when no member, the master included, has the
  pinned Xcode. It is the `xcode_apple_id_required` item, which the loop never
  acts on: a person downloads that Xcode onto the master, and the master's
  changed signal makes the loop copy it to every member that needs it.

In a macOS VM (macOS 26.6), `tests/openssh_mac.rs` ran against a fresh
administrator account over SSH: the master's rendered script through the
session's sudo installed the helper (`root:wheel`, 0755) and the drop-in
(`root:wheel`, 0440), the Mac's own `elsecall tools privileges` read `ready:
true` and `helper: current`, and installing them again changed nothing.

### The window

The fleet window shows provisioning where the operator already looks, and runs
nothing an agent could not: every command it shows is one `elsecall fleet`
lists for agents too.

- **Status.** A member's status cell shows `Installing 2 of 5` with a spinner
  while an apply runs there (from its `status.provisioning`: the step after
  those installed or failed, of all its steps), `Needs you` when one of its
  provisioning items starts with a person's action (Xcode privileges on it, or
  an Apple ID on the master, on the master's row), and the Provision button
  while it awaits provisioning and nothing runs. Otherwise the usual online,
  blocked or offline pill shows.
- **Provision** runs `elsecall tools provision <member>` through the local
  service (`POST /v1/tools/provisioning`), so the master plans and applies it
  at once. It is enabled only in the master's window; another member's window
  says where to press it. The button reads `Starting…` until the fleet shows a
  new apply on the member (running, or already finished), the member ready, or
  200 seconds pass; a refusal shows in its help. The loop provisions a new
  member by itself anyway; the button only skips its wait, and retries failed
  steps but those held for a person, which its help counts apart.
- **Setup**, next to Load details on every reachable member, opens the
  member's setup: **For you**, its provisioning items that start with a
  person, each with its message, hint and the command with the member to run
  it on, and the Apple ID item when its apply needed an Xcode the master lacks;
  **Latest apply**, each step and how it ended, a failure with its stage, code
  and its item's hint; and **To install**, the plan's steps and its unresolved
  requests with their hints, or a note when some pins or tools went unread.
  It reads `GET /v1/tools/apply` and `GET /v1/tools/plan` for that member
  when it opens, each on a thread of its own and shown as it lands, and the
  apply again whenever it moves; one read of each runs at a time. On a member
  that is not the master the plan reads as `not_master`, said in place.
  **Needs you** opens it too.
- **Attention.** Above the members, every item of `elsecall fleet`'s
  `attention`, plainly: its message, its hint and each action's command with
  where to run it, a person's marked `For you`. A secret consent item keeps
  its checkbox, "Sync changes to these automatically from now on", which
  chooses which of its two `person_only` commands to show (`--always` or
  not): the CLI refuses `--confirm` without a terminal, so the window shows
  the command for a person to run rather than running it.

Nothing in the window starts an agent: the items and their actions are the
hooks a hand-off will use.

### Evidence: one click

In a macOS VM (macOS 26.6), two members on one Mac, each an account running its
own `elsecall serve` on loopback: the master `mac-mini` (`admin`, Homebrew's
mise 2026.11.3 with jq 1.7.1 installed, a workspace pinning `jq = "1.7.1"`)
and `macbook-pro` (`devbox`, an administrator account with no mise), enrolled
awaiting provisioning as Add Mac leaves a Mac. The fleet window on the master
listed `macbook-pro` with Provision and the `awaiting_provisioning` item.

- The click at 21:24:02 was planned at 21:24:03 (`tools provisioning`:
  `applying`, one step), and the row read `Installing 1 of 1`.
- The apply ran 21:24:03 to 21:24:22 as one `elsecall tools install` run that
  exited 0: the master's mise (237 MB) was copied to `~/.local/bin/mise`, then
  jq 1.7.1 installed with it.
- At the loop's next look the plan had no steps; `macbook-pro` was ready on
  both members at 21:25:09, its row read `online`, the attention list was
  empty and `tools provisioning` read `in_step`.
- For the 47 seconds between the apply's end and that look the row read
  Provision again, which looks unfinished. So the loop now looks every five
  seconds while an apply runs ("The loop", above); this run predates that.

### Placement by pins

Placeable work goes to a member that has the tools its workspace pins. What a
member lacks is what the loop's last plan for it would install, step by step,
with the workspaces each step's pins come from; the master's global pins and a
member's exceptions count for every workspace, since a job uses the defaults
for whatever its workspace does not pin. Placement asks it without waiting: the
loop holds it in memory, so a request is never slowed by planning. A member the
loop has not planned yet lacks nothing as far as placement knows, so placement
is never worse off than before.

This applies to runs the master places, as agents' placed runs start on the
master, where the loop runs. Another member placing work of its own places it
as before: the loop's view of the fleet stays on the master. Carrying it to
every member is a later change.

- **Preferred.** The members lacking something are left out of the first
  scoring, each named in the reason: `bravo was left out: it lacks bun 1.3.5,
  which the workspace pins; the master is installing them.` The decision's
  `scoring.passed_over` lists them with that reason, apart from `unreachable`,
  which keeps meaning members placement could not ask. The calling member is
  never left out: the work comes from there, with its own tools. The usual
  rules then pick among the rest.
- **Fallback.** Only when no remaining member can start the request within its
  hold limit is it scored again with every member. If the winner lacks
  something, the reason says so, and the run carries a warning in its
  `warnings`, as consistency warnings are carried (`elsecall runs`, `explain`,
  its status file), also when the run then fails to start: `bravo lacks bun
  1.3.5, which this workspace pins, so the run may fail there; the master is
  installing them. Elsecall placed it there because no member that has them
  could start it in time.`
- **Named.** A run that names its member with `--member` runs there as asked,
  and carries the same warning ending `It runs there because it was asked to.`
  when that member lacks something.

At most three tools are named, then how many more. Waiting for a member to be
provisioned is not an option placement takes: a run fails over to a member
lacking a tool only where it would otherwise wait past its hold limit or fail
`fleet_busy`, and the warning names what to expect. `elsecall replay` scores
recorded decisions again with the candidates they recorded, so a what-if under
other parameters does not bring passed-over members back in.

## Open questions for later deliveries

- **Exceptions through config sync.** `fleet-tools.toml` lives on the master
  only: carrying it on the config-sync generation (fleet-tools Slice 2b-2)
  never landed. The plan reads it on the master, so it is always the file in
  effect, but a `fleet move-master` leaves it behind until it is copied. That
  is its own slice in `elsecall-sync`; it also needs a generation in a fleet
  that syncs no paths.
- **mise lockfiles.** A `mise.lock` records exact resolved versions and would make
  `latest` pins exact without asking the master. Not read yet.
