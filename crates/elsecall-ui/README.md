# elsecall-ui

Native window that lists the fleet the local Elsecall service already reports.
It talks only to the same-user socket at `~/Library/Application Support/Elsecall/service.sock`.
It does not listen on the network. Releases include it beside the CLI;
`elsecall install` stores it in the user's support directory and, in a desktop
session, signs and publishes it as `~/Applications/Elsecall.app`.
`elsecall install --open` also opens it and waits for its readiness acknowledgment.

To open the installed window later, open Elsecall from Finder or run `elsecall ui`,
which publishes the app again and opens a fresh instance. The service must be running.

The local service checks for newer stable releases at startup and hourly, with
cached results across restarts. When a release is available, the window shows
its version beside **Upgrade and reopen**. Clicking installs the signed CLI and
UI, waits for current work, restarts the service and opens the new window. Checks
and progress poll separately from the fleet; an offer remains usable while fleet
data is loading. No release installs until you click or run `elsecall update install`.

You can also install a CLI release with mise (or extract both executables from
its archive), then run `elsecall install`. Leave the old window open: its next
refresh uses the same button when the staged window differs.
The button shows **Upgrading…** and disables itself while the app is signed,
replaced and reopened. The new native window acknowledges readiness before the
old window closes.
A startup crash or a 15-second readiness timeout restores the previous app. Signing
or launch failures preserve the previous app, keep the old window open, and show
an error; click the button again to retry.

The app is signed in a separate directory before an atomic bundle swap, so a
running executable is never overwritten. A release ID inside the signed bundle
identifies the original binary, since signing changes executable bytes.
Updates require the same user and a macOS desktop session. Installing just a CLI
without its companion preserves the previously staged window; it offers no new UI.
The Finder app is preserved when the service is uninstalled.

## Run

```sh
cargo run -p elsecall-ui
```

`mise run build` still builds only the `elsecall` service binary. This crate is
outside that default workspace member. To install both from a checkout:

```sh
cargo build --workspace --locked --release
./target/release/elsecall install
elsecall ui
```

The window title is Elsecall. A background thread reads `GET /v1/fleet`, then
waits two seconds after each response before the next read. A request that uses
the full 15 second timeout is about 17 seconds before the next attempt. Fleet
responses accept up to 8 MiB, including member diagnostics the table does not draw. If the
service is not running, the window says so and names `elsecall install` and
`elsecall service restart`.

Headless frames use rune's tooling, the same `--snap` path as rune's hello example:

```sh
cargo run -p elsecall-ui -- --snap /tmp/fleet.png --scene members --theme light --scale 1
```

Scenes: `loading`, `unavailable-first`, `unavailable`, `solo`, `no-tailnet`, `members`,
`members-selected`, `outdated-member`, `upgrade`, `upgrading`, `upgrade-failed`,
`release-available`, `release-loading`,
`release-downloading`, `release-waiting`, `release-installing`, `release-reopening`,
`release-failed`, `release-offline`, `release-future`.
`--theme` is `light` or `dark`. `unavailable-first` is the service down before
any fleet data (banner, no table). `unavailable` keeps the last table, greyed,
under that banner.

## Add Mac: first slice

The fleet header has **Add Mac**. It prepares onboarding asynchronously through
`POST /v1/onboarding/prepare` on the same-user socket, prevents duplicate clicks,
and keeps the fleet visible. This window understands the version 1 installer
availability blocker. Older services show version-repair guidance; an unavailable
service or timeout shows service-recovery guidance. Raw errors, paths and
invitation contents never enter this UI.

**Production onboarding is blocked:** there is no verified signed/notarized
plain-Mac receiver installer yet. Preparation reports that prerequisite and sends
nothing. The window refuses artifact-bearing responses until verified acquisition
and receiver integration land. Installer launch, authenticated claim, connecting,
joined, provisioning handoff and ready status are subsequent dependent work.
There is no redundant claim confirmation in this slice.

The agreed service contract is the normal `{schema_version:1,data:Operation}`
envelope. `Operation` has `version:1`, a 32-character lowercase hex `id`, Unix
`expires_at`, `stage`, nullable `artifact`, and nullable `error`. This slice accepts
only an unexpired `blocked` operation with no artifact and
`error.code = onboarding_installer_unavailable`. The installer workstream owns
status/cancel persistence and future signed installer/invitation packaging.

## Real native-window demo

From a local macOS desktop session:

```sh
scripts/run-add-mac-demo.sh
```

This builds and ad-hoc signs a separate **Elsecall Add Mac Demo.app** under
`target/add-mac-demo`. It runs the actual rune fleet window with the neutral
`mac-mini` fixture, explicitly labeled as a demo. It does not contact the local
service or read the live fleet. Click **Add Mac**: the public AppKit
`NSSharingServiceNameSendViaAirDrop` service opens Apple's recipient picker
directly, without a sharing-method menu, Finder or file selection. Only a harmless
`Elsecall AirDrop Demo.txt` is supplied. This is not an installer or invitation.
Use **Cancel** in Apple's dialog; do not select an unauthorized nearby device.
`--build-only` builds the bundle without opening it.

The real-window check exercised the picker, system Cancel, retry and a missing
demo-file refusal. No file was transferred and no Mac was enrolled. On the tested
macOS version, Cancel invoked `didShareItems` with the original item. Consequently
that callback displays **AirDrop closed / Delivery is unconfirmed**, never a
delivery or enrollment success. Explicit user-cancel errors display **AirDrop
cancelled**; other native errors offer retry. This public API ambiguity must stay
separate from authenticated installer/service progress.

Native screenshots attached to review show the running window, actual picker,
post-Cancel state and refusal before sharing. The picker's account avatar/name
are masked; the fleet is synthetic. They are distinct from rendering goldens.
The MP4 is a sequence of those captured native-window stills, not a continuous
screen recording. A separate capture verifies the actual Add Mac button against
the installer workstream's verified real service: a private temporary child home,
loopback-only synthetic presence, and the expected installer-unavailable blocker.
The live service and fleet were untouched, and the isolated service was stopped
after verification.
To reproduce the missing-file refusal, remove only this demo's temporary text
payload while its window is open, then retry Add Mac. Relaunch the demo to restore
the payload. Do not remove files from a production onboarding operation.

Additional rendering scenes: `demo-airdrop`, `add-mac-preparing`,
`add-mac-picker`, `add-mac-blocked`, `add-mac-cancelled`, `add-mac-failed`,
`add-mac-unconfirmed`. Goldens cover both appearances, including minimum window
size for the blocker, failure and picker progress. Pointer-driven flow tests use
fakes only at preparation and native-transfer boundaries; Unix-socket tests use
the real HTTP client, same-user authorization and private directory checks.

```sh
cargo test -p elsecall-ui --locked
cargo clippy -p elsecall-ui --all-targets --locked -- -D warnings
```

Apple references: [direct AirDrop service](https://developer.apple.com/documentation/appkit/nssharingservice/name/sendviaairdrop),
[perform with items](https://developer.apple.com/documentation/appkit/nssharingservice/perform(withitems:)),
[delegate callbacks and presentation](https://developer.apple.com/documentation/appkit/nssharingservicedelegate).

## Golden tests

`cargo test -p elsecall-ui --bin elsecall-ui fleet_scenes_match_goldens` renders each
scene in light and dark and compares it with `tests/golden/*.png`. A channel may
differ by at most 8, which is the delta rune's own text comparison treats as a
real miss.

Rewrite the goldens after an intentional visual change:

```sh
ELSECALL_UI_BLESS=1 cargo test -p elsecall-ui --bin elsecall-ui fleet_scenes_match_goldens
```

Review the PNG diff before committing it.

GitHub's `macos-latest` job runs these tests and does not set
`ELSECALL_UI_SKIP_GOLDENS`. Rune's own macOS workflow
(`.github/workflows/macos-tests.yml` on a Mac runner) keeps GPU rendering, font
and pipeline tests enabled and only skips a Metal timestamp-query test the
virtual adapter does not expose. This crate does the same: the goldens run on
the runner's GPU, with the channel tolerance above for antialiasing noise.

If this process cannot see a Metal device (`MTLCopyAllDevices` empty, or the
probe failing to load Metal), the test fails. Agent, SSH and background
sessions cannot see the GPU. `ELSECALL_UI_SKIP_GOLDENS=1` skips in that case.
Blessing still requires a device, so run it from Terminal.app.

## Rune pin

`rune` and `rune-kit` are git dependencies on `https://github.com/anomalyco/rune`
at `ea6ddf8f2c3cd476a3797caad7d77cbe04641d9c` (dev, 2026-11-12). The workspace
`[patch.crates-io]` points `winit`, `wgpu-hal`, `parley_engine` and
`accesskit_windows` at that same revision.

To bump rune, change that rev in six places: `rune` and `rune-kit` in
`crates/elsecall-ui/Cargo.toml`, and the four patches in the workspace
`Cargo.toml`. Then refresh `Cargo.lock` and re-bless the goldens.

## Appearance

Colours, type and spacing come from mergegate:

- `appearance/tokens.json`
- `ui/design-system/appearance.css`
- `ui/design-system/tokens.css`

The mapping onto rune's `Kit` is the table in `src/theme.rs`. UI text is DM Sans
(`assets/fonts/DMSans.ttf`, OFL in `OFL.txt`, copied from mergegate
`ios/Resources`). Monospace text uses the system mono face (SF Mono on macOS).

## Icon

The app icon is the fleet as a constellation: the bright node is the master, the
pale ones are members. Its source is `assets/icon/elsecall.svg` at the repository
root, drawn on Apple's macOS icon grid in colours derived from the brand tokens
above. The service embeds `assets/icon/Elsecall.icns` and writes it into the app
as `Contents/Resources/Elsecall.icns`, named by `CFBundleIconFile`, whenever it
places the app (install, release update, or the window's own upgrade), so
Finder, the Dock and Spotlight show it. A window started with `cargo run` has no bundle and
shows the generic icon.

After changing the SVG, regenerate the icon, the README image and the GitHub
social preview, then commit them:

```sh
scripts/render-icon.sh
```

It needs macOS for `iconutil` and `rsvg-convert` (`brew install librsvg`).
