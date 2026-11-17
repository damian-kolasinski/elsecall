# macOS VMs for end-to-end tests and demos

`scripts/vm/ecvm` drives throwaway macOS guests in [Tart](https://github.com/cirruslabs/tart):
it clones a prepared image, boots it, runs visible Terminal sessions, clicks and
types over VNC, records the screen to MP4 and waits for conditions. Every step
has a deadline. A step that runs out of time saves a screenshot, prints its
path and exits non-zero, so whoever runs it looks at what is in the way instead
of waiting. When that is a dialog only a person can answer, the message ends
with `this needs the operator`: ask them right away.

## Requirements

- Tart 2.41 or later on `PATH` (from its GitHub releases), and `ffmpeg`.
- `python3`. The first run installs the pinned VNC client
  (`scripts/vm/requirements.txt`) into a private venv under
  `~/Library/Caches/elsecall-vm`, which also holds each VM's private state.
- A base image with the Tart guest agent, such as a local clone of
  `ghcr.io/cirruslabs/macos-tahoe-base:latest` (user `admin`, password `admin`,
  automatic login, Homebrew and mise preinstalled). The examples call it `ec-base`.

macOS runs at most two macOS guests at a time. `ecvm up` refuses to boot a third
and names the running ones; ask the operator to stop one, or wait.

## The ready image

```sh
scripts/vm/ecvm image prepare --from ec-base ec-ready   # about 2 minutes
scripts/vm/ecvm image check ec-ready                     # boots a throwaway clone
```

`prepare` clones the base, boots it and settles, once, everything that would
otherwise interrupt a clone:

| Interruption | What the image keeps |
| --- | --- |
| "tart-guest-agent wants access to control Terminal" (also System Events, Finder): Apple Events from `tart exec osascript` hang with error -1712 until someone clicks | Approved: `prepare` sends one Apple Event to each app and presses Return over VNC, which answers Allow |
| "App Background Activity" banners whenever a launch agent appears, including Elsecall's and the guest agent's own | `com.apple.backgroundtaskmanagement.agent` disabled for the user; launch agents still load |
| "See what's new in macOS" and update offers | `com.apple.tipsd` and `com.apple.SoftwareUpdateNotificationManager` disabled; automatic update checks off |
| Terminal reopening the base image's windows and shell history at login | Window restoration off, history removed |
| Small Terminal text in recordings | Default profile at 15 pt, 100×30 |

Do Not Disturb stays off, so Elsecall's own notifications still show in demos;
it does not hold back background-item banners anyway. Screen recording happens
over VNC from the host, so the guest never asks to bypass its private window
picker. `check` boots a clone, confirms the notifiers stay off, no banner waits
in the notification database, Terminal stays closed, widgets stay hidden and an
Apple Event to Terminal returns without any prompt, then saves a screenshot of
the desktop and deletes the clone.

`prepare` never changes the base image. `ecvm` acts only on VMs it created: each
clone's marker records the MAC address Tart chose for it, and every command that
takes a VM name checks that marker against Tart, so images and VMs that other
people or sessions use are left alone. A prepared image is never booted or
deleted by `ecvm`, so it stays as `prepare` left it; to rebuild one, delete it with
`tart delete` and prepare it again.

## A fresh Mac

The image still has Homebrew's mise. For a Mac that never had it, boot with
`--fresh`, or run `ecvm fresh NAME` on a running clone: it uninstalls the
formula, deletes mise's data, configuration and cache, removes the shell lines
that activate it, and fails if any shell can still find `mise`.

```sh
scripts/vm/ecvm up mac-mini --from ec-ready --fresh
```

## Commands

| Command | Does |
| --- | --- |
| `up NAME --from IMAGE [--fresh] [--hostname mac-mini]` | Clones `NAME` if needed and boots it to the desktop (about 30 s); names the Mac as a fleet member |
| `down NAME`, `rm NAME`, `ls` | Shut down; shut down and delete; list the VMs `ecvm` created |
| `exec NAME [--timeout S] -- CMD…` | Runs a command in the guest; past the deadline, fails with a screenshot |
| `push NAME FILE GUEST_PATH`, `pull NAME GUEST_PATH FILE` | Copies a file in or out (`~/` is the guest user's home) |
| `term NAME SCRIPT [--label L]` | Runs a script in a new, visible Terminal window, without Apple Events |
| `wait NAME --session L \| --process PAT \| --file PATH \| --shell CMD [--timeout S]` | Waits for a condition; on timeout, fails with a screenshot |
| `shot NAME [OUT.png]` | Captures the screen |
| `click NAME X Y`, `key NAME KEY…`, `type NAME TEXT` | VNC input in screenshot pixels; keys such as `enter`, `esc`, `super-q` (Cmd-Q) |
| `record start NAME OUT.mp4`, `record stop NAME` | Records the screen over VNC (10 fps, 1280 px wide) plus a contact sheet |

A `term` script holds one complete shell command per line. Each is shown as if
typed and then run; `#` lines appear as narration. Constructs spanning lines
(`\` continuations, heredocs, `if` blocks) do not work: put them in a script,
`push` it, and run that. The session stops at the first failing
command, records its exit status for `wait --session`, and ends at a live
prompt, so `type` can carry on in the same window.

VNC input arrives as hardware events, so it reaches system dialogs that guest
privacy controls keep away from in-guest automation. Coordinates are pixels of
`ecvm shot` captures (2048×1536 for the default 1024×768 display).

The VNC password Tart generates stays in a private file next to the VM's state
and never reaches output; error messages redact it.

## A demo, end to end

```sh
cat > install.session <<'EOF'
# A Mac with neither mise nor Elsecall
command -v mise elsecall || echo 'nothing installed yet'
curl -fsSL https://github.com/damian-kolasinski/elsecall/releases/latest/download/install.sh | sh
EOF
vm=scripts/vm/ecvm
$vm up install-demo --from ec-ready --fresh --hostname mac-mini
$vm record start install-demo install.mp4
$vm term install-demo install.session --label install
$vm wait install-demo --session install --timeout 300
$vm wait install-demo --process 'Elsecall.app/Contents/MacOS' --timeout 60
$vm record stop install-demo   # prints install.mp4 and install-contact.png
$vm rm install-demo
```

`scripts/vm/demo-install.sh` runs this demo.

## When something needs a person

- A dialog `prepare` cannot answer with Return: it stops with a screenshot and
  `this needs the operator`. Ask them to click it in the guest (Screen Sharing to
  the VNC address, or `ecvm click` on the coordinates they give), then run
  `prepare` again on a new name.
- Two VMs already running: ask the operator which one may stop.
- Never type a password over VNC to get past a dialog you did not expect, and
  never run `sfltool resetbtm`: it asks for the password in a dialog.
