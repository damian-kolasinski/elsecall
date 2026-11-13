#!/bin/sh
# Install the privacy hooks for the clone in the current directory and all its
# worktrees. They and the scanner they run are copied under the clone's .git,
# so they never run code from whatever branch is checked out, such as a branch
# under review. Run it again after .githooks or scripts/privacy_scan.py change.
#
# With private terms listed at install time, the hooks refuse to run without
# them later, so a moved or deleted list can't quietly weaken them.
# See AGENTS.md, "Public material".
set -eu
source=$(cd "$(dirname "$0")/.." && pwd)
hooks="$(cd "$(git rev-parse --git-common-dir)" && pwd)/privacy-hooks"
mkdir -p "$hooks"
cp "$source/.githooks/pre-commit" "$source/.githooks/commit-msg" "$source/.githooks/pre-push" \
    "$source/scripts/privacy_scan.py" "$hooks/"
chmod 755 "$hooks/pre-commit" "$hooks/commit-msg" "$hooks/pre-push"
git config core.hooksPath "$hooks"
terms=${PRIVACY_SCAN_TERMS_FILE:-$HOME/.config/elsecall/private-terms}
if [ -s "$terms" ]; then
    git config privacyscan.requireTerms true
    echo "Privacy hooks installed in $hooks; they require the private terms in $terms."
else
    git config --unset privacyscan.requireTerms || true
    echo "Privacy hooks installed in $hooks, with public rules only. List private terms one per line in $terms, then run this again."
fi
