#!/bin/sh
set -eu
. scripts/lib/elsecall.sh
[ -n "${ELSECALL_ADMISSION:-}" ] || [ ! -x "$EC" ] || exec "$EC" run --label "elsecall check" -- "$0" "$@"
# Keep competing benchmarks out of this run. Functional coverage also runs in check.
EC_TEST_PERF=1 exec python3 scripts/test-environment.py cargo test --locked --test workspace_copy --test placement --test pre_run_sync --test remote_run --test rehearse perf_ -- --test-threads=1 --nocapture
