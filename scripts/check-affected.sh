#!/bin/sh
set -eu
. scripts/lib/elsecall.sh
[ -n "${ELSECALL_ADMISSION:-}" ] || [ ! -x "$EC" ] || exec "$EC" run --label "elsecall check affected" -- "$0" "$@"
exec python3 scripts/check_affected.py "$@"
