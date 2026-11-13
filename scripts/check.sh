#!/bin/sh
set -eu
. scripts/lib/elsecall.sh
[ -n "${ELSECALL_ADMISSION:-}" ] || [ ! -x "$EC" ] || exec "$EC" run --label "elsecall check" -- "$0" "$@"
cargo fmt --all -- --check
cargo clippy --workspace --locked --all-targets -- -D warnings
exec python3 scripts/test-environment.py cargo test --workspace --locked
