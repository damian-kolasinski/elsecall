#!/bin/sh
# Lint and unit-test the code only builds that follow a local release channel
# compile. The channel and key are placeholders: nothing reads the channel, and
# scripts/local-release.sh makes real ones.
set -eu
export ELSECALL_LOCAL_RELEASE_CHANNEL=file:///nonexistent/elsecall-channel
export ELSECALL_LOCAL_RELEASE_KEY=RWQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
cargo clippy -p elsecall-release-update --locked --all-targets --features local-release-channel -- -D warnings
exec cargo nextest run -p elsecall-release-update --locked --lib --features local-release-channel release_update::
