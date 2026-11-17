#!/bin/sh
# Records Elsecall's one-line install from the latest published release on a
# fresh Mac: a clone of the ready image with mise removed.
#   scripts/vm/demo-install.sh [OUT.mp4] [IMAGE]
# KEEP=1 leaves the clone running afterwards.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
ecvm="$here/ecvm"
out=${1:-$PWD/install-demo.mp4}
image=${2:-ec-ready}
# A name of its own, so it never collides with another session's VM.
name=ecvm-install-$(date +%s)-$$
work=$(mktemp -d)
session="$work/install.session"

finish() {
  status=$?
  "$ecvm" record stop "$name" >/dev/null 2>&1 || true
  [ "${KEEP:-}" = 1 ] || "$ecvm" rm "$name" >/dev/null 2>&1 || true
  rm -rf "$work"
  exit "$status"
}
trap finish EXIT
trap 'exit 130' INT TERM

cat >"$session" <<'EOF'
# A fresh Mac: neither mise nor Elsecall
command -v mise || echo 'mise: not installed'
command -v elsecall || echo 'elsecall: not installed'

# Install the latest release with the one-line installer
curl -fsSL https://github.com/damian-kolasinski/elsecall/releases/latest/download/install.sh | sh
EOF

"$ecvm" up "$name" --from "$image" --fresh --hostname mac-mini
"$ecvm" record start "$name" "$out"
sleep 2
"$ecvm" term "$name" "$session" --label install
"$ecvm" wait "$name" --session install --timeout 420
"$ecvm" wait "$name" --process 'Elsecall.app/Contents/MacOS' --timeout 60
# Let the window show for a moment.
sleep 10
"$ecvm" record stop "$name"
