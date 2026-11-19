#!/bin/sh
# Test Elsecall's update path against local builds instead of published releases.
#
#   scripts/local-release.sh build [--url URL] [--fail-to-start VERSION] [VERSION...]
#   scripts/local-release.sh serve
#
# build makes Cargo.toml's version and the next patch by default, both pointed at
# the channel URL (by default the host as a Tart guest sees it), then prints how
# to install the first in a test VM and update it to the next. See
# scripts/local_release.py and crates/elsecall-release-update/src/release_update/channel.rs. Building more
# versions later adds them to the channel; --fail-to-start makes one whose service
# refuses to start, so updating to it rehearses the rollback. serve publishes the
# channel over HTTP at the URL the builds were made for.
set -eu
exec python3 "$(dirname "$0")/local_release.py" "$@"
