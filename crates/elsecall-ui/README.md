# elsecall-ui

Native window that lists the fleet the local Elsecall service already reports.
It talks only to the same-user socket at `~/Library/Application Support/Elsecall/service.sock`.
It does not listen on the network. Releases include it beside the CLI;
`elsecall install` stages it in the user's support directory.

To open the installed window, run `elsecall ui`. This signs and publishes
`~/Applications/Elsecall.app`, then opens a fresh instance. The service must be running.

To upgrade, install the new CLI release with mise (or extract both executables
from its archive), then run `elsecall install`. Leave the old window open: its
next refresh offers **Upgrade and reopen** when the staged window differs.
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

Scenes: `loading`, `unavailable-first`, `unavailable`, `solo`, `members`,
`upgrade`, `upgrading`, `upgrade-failed`.
`--theme` is `light` or `dark`. `unavailable-first` is the service down before
any fleet data (banner, no table). `unavailable` keeps the last table, greyed,
under that banner.

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
