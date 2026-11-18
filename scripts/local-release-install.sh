#!/bin/sh
# Install a local Elsecall build from a local release channel, as the published
# installer installs a signed release. scripts/local-release.sh writes a copy of
# this into the channel with the channel, version and key filled in:
#
#   curl -fsSL CHANNEL/install.sh | sh
#
# The archive is verified with packslip against the channel's public key before
# anything from it runs, then linked into mise as the release version and
# selected in mise's global configuration, so later updates take the same path
# as on a published installation. Use it in a test VM or account only: it
# replaces this account's Elsecall installation.
#
# Everything runs from main, called on the last line, so a download cut short
# runs nothing. Commands read from /dev/null: when piped to sh, standard input
# is the rest of this script.
set -eu

tool="packslip:github.com/damian-kolasinski/elsecall"
packslip="github:jdx/packslip@1.11.1"
channel="@CHANNEL@"
release="@VERSION@"
key="@KEY@"

say() { printf 'elsecall (local channel): %s\n' "$*" >&2; }
fail() {
  say "$*"
  exit 1
}

find_mise() {
  if command -v mise >/dev/null 2>&1; then
    command -v mise
    return 0
  fi
  for candidate in "$HOME/.local/bin/mise" /opt/homebrew/bin/mise /usr/local/bin/mise; do
    if [ -x "$candidate" ]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

main() {
  [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] ||
    fail "Local builds run on Apple silicon Macs only."
  if ! mise=$(find_mise); then
    mise="$HOME/.local/bin/mise"
    say "Installing mise from https://mise.run into ~/.local/bin."
    curl -fsSL https://mise.run | MISE_INSTALL_PATH="$mise" sh
    [ -x "$mise" ] || fail "mise did not install. See https://mise.jdx.dev/getting-started.html."
  fi

  # Never replace a published installation: the link would take that version's
  # place in mise, and its real update would then refuse to install over it.
  if [ -e "$HOME/.local/bin/elsecall" ] && ! grep -q ELSECALL-LOCAL-RELEASE-CHANNEL-BUILD "$HOME/.local/bin/elsecall"; then
    fail "This account runs a published Elsecall. Install local builds in a test VM or account instead."
  fi
  if "$mise" ls --installed "$tool" </dev/null 2>/dev/null | grep -v '(symlink)' | grep -q .; then
    fail "mise holds a published $tool install. Install local builds in a test VM or account instead."
  fi

  archive="elsecall-v$release-aarch64-apple-darwin.tar.gz"
  download=$(mktemp -d)
  trap 'rm -rf "$download"' EXIT
  say "Downloading $release from $channel."
  curl -fsSL "$channel/v$release/$archive" -o "$download/$archive" </dev/null
  curl -fsSL "$channel/v$release/packslip.sigstore.json" -o "$download/packslip.sigstore.json" </dev/null
  say "Verifying it against the channel's key with $packslip."
  report=$(cd "$HOME" && "$mise" exec "$packslip" -- packslip verify "$download/packslip.sigstore.json" \
    --pubkey "$key" --allow-unlogged --json --artifact "$download/$archive" </dev/null)
  printf '%s\n' "$report" | grep -q '"project": "elsecall.localhost"' &&
    printf '%s\n' "$report" | grep -q "\"version\": \"$release\"" ||
    fail "The bundle is signed with the channel's key, but not for elsecall.localhost $release."

  installed="$HOME/.local/share/elsecall/local-releases/$release"
  rm -rf "$installed"
  mkdir -p "$installed"
  tar -xzf "$download/$archive" -C "$installed" elsecall elsecall-ui </dev/null
  [ "$("$installed/elsecall" --version </dev/null)" = "elsecall $release" ] ||
    fail "The verified executable does not report $release."

  cd "$HOME"
  say "Selecting $tool@$release, linked to the local build, in mise's global configuration."
  "$mise" link --force "$tool@$release" "$installed" </dev/null
  "$mise" use --global --pin "$tool@$release" </dev/null

  say "Installing the service and opening the window; this waits until both are ready."
  "$mise" exec -- elsecall install --open </dev/null
  say "Done. Updates now come from $channel: elsecall update check, then elsecall update install --reopen."
}

main "$@"
