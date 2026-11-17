#!/bin/sh
# Install Elsecall on this Mac and open its window.
#
#   curl -fsSL https://github.com/damian-kolasinski/elsecall/releases/latest/download/install.sh | sh
#
# Selects the signed release in mise's global configuration (installing mise first
# if needed; mise's packslip backend checks the release's signature against its
# GitHub repository before anything from it runs), then runs that release's
# `elsecall install --open`. The copy attached to a release installs that release;
# ELSECALL_VERSION chooses another.
#
# Everything runs from main, called on the last line, so a download cut short runs
# nothing. Commands read from /dev/null: when piped to sh, standard input is the
# rest of this script.
set -eu

tool="packslip:github.com/damian-kolasinski/elsecall"
# The release workflow replaces this with the version the installer is attached to.
# mise's own "latest" can lag a new release, or skip it under a minimum release age.
release=latest

say() { printf 'elsecall: %s\n' "$*" >&2; }
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
  version="${ELSECALL_VERSION:-$release}"
  [ "$(uname -s)" = Darwin ] || fail "Elsecall runs on macOS only."
  [ "$(uname -m)" = arm64 ] || fail "Elsecall releases are built for Apple silicon Macs only."

  if ! mise=$(find_mise); then
    mise="$HOME/.local/bin/mise"
    say "Installing mise from https://mise.run into ~/.local/bin."
    curl -fsSL https://mise.run | MISE_INSTALL_PATH="$mise" sh
    [ -x "$mise" ] || fail "mise did not install. See https://mise.jdx.dev/getting-started.html."
  fi
  "$mise" backends ls </dev/null 2>/dev/null | grep -qx packslip ||
    fail "$("$mise" --version </dev/null | head -n 1) has no packslip backend. Update mise (mise self-update, or brew upgrade mise) and run this again."

  # Project configuration in the current directory must not choose the release.
  cd "$HOME"
  say "Selecting $tool@$version in mise's global configuration."
  "$mise" use --global --pin "$tool@$version" </dev/null

  say "Installing the service and opening the window; this waits until both are ready."
  status=0
  result=$("$mise" exec -- elsecall install --open </dev/null) || status=$?
  printf '%s\n' "$result"
  case "$result" in
  *'"state":"background_session"'*)
    say "This account has no desktop session, so the service runs in the background."
    say "Log in to the desktop and run elsecall ui to see the window."
    [ "$status" -ne 1 ] || status=0
    ;;
  *'"window":{'*)
    [ "$status" -eq 0 ] ||
      fail "The service is installed, but the window did not open; the result above says why. Try elsecall ui."
    ;;
  esac
  [ "$status" -eq 0 ] || fail "Installation did not finish; the result above says why."

  case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *)
    say "The CLI is ~/.local/bin/elsecall. Add this to your shell configuration to run it as elsecall:"
    # shellcheck disable=SC2016 # printed for the user to paste, unexpanded
    say '  export PATH="$HOME/.local/bin:$PATH"'
    ;;
  esac
  say "Done. Run elsecall status to check the service; updates are offered in the window and by elsecall update check."
}

main "$@"
