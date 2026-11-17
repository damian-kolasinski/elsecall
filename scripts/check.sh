#!/bin/sh
set -eu
. scripts/lib/elsecall.sh
[ -n "${ELSECALL_ADMISSION:-}" ] || [ ! -x "$EC" ] || exec "$EC" run --label "elsecall check" -- "$0" "$@"
cargo nextest --version >/dev/null 2>&1 || {
  echo "check: cargo-nextest is missing; run 'mise install'" >&2
  exit 1
}
cargo fmt --all -- --check
cargo clippy --workspace --locked --all-targets -- -D warnings
if [ -d scripts/vm ]; then
  python3 -m unittest discover -s scripts/vm -p 'test_*.py'
fi
if [ -d distribution ]; then
  python3 -m unittest discover -s distribution -p 'test_*.py'
fi
if [ -f scripts/test_check_selection.py ]; then
  python3 -m unittest discover -s scripts -p 'test_*.py'
fi
# nextest does not run doctests. Both run even if the first fails.
status=0
python3 scripts/test-environment.py cargo nextest run --workspace --locked || status=$?
python3 scripts/test-environment.py cargo test --doc --workspace --locked || status=$?
exit "$status"
