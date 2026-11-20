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
`elsecall uninstall` removes the Finder app after stopping the service. A window
that then reaches neither the service nor its app closes itself.

## Run

```sh
cargo run -p elsecall-ui
```

Click **Configuration** in the window toolbar or press ⌘, to inspect personal
`workloads.toml`. **Rules** shows global label rules and the service-unavailable
fallback. Search filters rule patterns by text. **TOML** shows the source, including
any directory policies. Use **Reload** after editing the file. The view works
while the service is offline.

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

## Add Mac

**Add Mac** in the header opens a sheet that walks through [Adding a Mac](../../docs/add-mac.md):

1. **On the new Mac**: a looping illustration of System Settings › General › Sharing
   with Remote Login being turned on.
2. **Connect to Wi-Fi**: a looping illustration of joining the same Wi-Fi network
   in System Settings. A wired connection to the same local network works too.
3. **Choose it**: the Macs on this network, looked for again every 3 seconds while
   the page is open, each with its model, host name and address. Each shows whether
   it is ready, needs Remote Login, may not be a Mac, or may already be a member.
   The picker hides this Mac, virtual Macs, and confirmed members. Enter the
   address manually to add a virtual Mac or repair a member. One is picked for you only
   when it is the only one ready, since anything on the network can advertise itself.
   A host name or address can be typed instead. When looking fails, the sheet says
   why and what to do, with a way to the Local Network setting when macOS denies it.
4. **Sign in**: the exact Mac again (name, model, host, address), then an
   administrator account and its password in a secure field. The password leaves the
   sheet's state when the add starts and the child's input right after it is written.
5. **Add**: each step of `fleet add` as it runs. When Tailscale waits for approval,
   the window opens its page in the browser and the sheet shows an illustration of
   clicking Connect, with **Open the page again**. **Stop**, the sheet's close button
   and Escape stop the add and keep the sheet to show how it ended; quitting the window
   stops it too. The end shows the new member with its readiness, serial number, host
   key and Tailscale key expiry, or the failure with **Try again**, which keeps the
   account (and **Open the page** while an approval is pending).

Add Mac shows only on the master, the Mac that adds others.

The sheet runs the installed `elsecall` CLI as this app's child: `fleet nearby`, and
`fleet add --progress json` with the password on standard input, reading one event per
line from its standard error. So macOS asks once whether Elsecall may use the local
network (the app's `NSLocalNetworkUsageDescription` and `NSBonjourServices`), and
the window and agents share one implementation. Illustrations are drawn from the
window's clock at about 30 frames a second while they show, and hold their last frame
under Reduce Motion.

Scenes: `add-mac-prepare`, `add-mac-network`, `add-mac-looking`, `add-mac-choose`, `add-mac-sign-in`,
`add-mac-adding`, `add-mac-approve`, `add-mac-done`, `add-mac-failed`. Goldens also
capture the illustrations later in their loops and the approval
page at the window's minimum size. Pointer-driven tests walk the sheet from the header
to a member, and stop an add, with the CLI faked at its boundary.

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

Colours use the shared brand palette. The table in `src/theme.rs` maps the
light and dark swatches onto Rune Kit's semantic roles. Type and spacing use
Mergegate's `appearance/tokens.json` and `ui/design-system/tokens.css`.

UI text is DM Sans (`assets/fonts/DMSans.ttf`, OFL in `OFL.txt`, copied from
Mergegate's `ios/Resources`). Monospace text uses the system mono face (SF Mono
on macOS).

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
