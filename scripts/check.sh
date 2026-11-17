#!/bin/sh
set -eu
. scripts/lib/elsecall.sh
[ -n "${ELSECALL_ADMISSION:-}" ] || [ ! -x "$EC" ] || exec "$EC" run --label "elsecall check" -- "$0" "$@"
cargo fmt --all -- --check
cargo clippy --workspace --locked --all-targets -- -D warnings
if [ -d scripts/vm ]; then
  python3 -m unittest discover -s scripts/vm -p 'test_*.py'
fi
if [ -d distribution ]; then
  python3 -m unittest discover -s distribution -p 'test_*.py'
fi
exec python3 scripts/test-environment.py cargo test --workspace --locked
