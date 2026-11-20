#!/bin/sh
# Elsecall's Xcode helper: places, selects and licenses Xcode bundles as root.
# Installed root-owned by `elsecall tools privileges install`; the sudoers
# drop-in beside it lets @ACCOUNT@ run it, with these arguments only:
#
#   place VERSION    copy @ACCOUNT@'s staged Xcode-VERSION.app into
#                    @STORE@, owned by root, once it verifies as Apple's
#                    and its license is accepted: placed means ready
#   select VERSION   xcode-select a bundle this helper placed
#   license VERSION  accept a placed bundle's license again
#
# Accepting a license also runs Xcode's first-launch tasks when it is the
# newest Xcode placed.
#
# VERSION is digits and dots, @MINIMUM@ or later. Bundles live in a tree only
# root writes, so nothing the account can rename is ever followed, and root
# writes nothing anywhere else: a link from /Applications is the account's.
set -eu
# Nothing of the caller's environment reaches a command: sudo on macOS keeps
# HOME and others. The rule never lets the first argument be --clean.
if [ "${1:-}" != --clean ]; then
	exec /usr/bin/env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin HOME=/var/root LANG=C /bin/sh "$0" --clean "$@"
fi
shift
umask 022
cd /

STORE='@STORE@'
OWNER='@OWNER@'
ACCOUNT='@ACCOUNT@'
MINIMUM='@MINIMUM@'

fail() {
	echo "xcode-admin: $1" >&2
	exit "${2:-1}"
}

[ "$#" -eq 2 ] || fail "usage: place|select|license VERSION" 64
action=$1
version=$2
case "$version" in
'' | *[!0-9.]* | .* | *. | *..*) fail "a version is digits and dots" 64 ;;
esac
case "$version" in
*.*) ;;
*) fail "a version has at least two parts" 64 ;;
esac
major=${version%%.*}
rest=${version#*.}
minor=${rest%%.*}
least_major=${MINIMUM%%.*}
least_minor=${MINIMUM#*.}
if [ "$major" -lt "$least_major" ] || { [ "$major" -eq "$least_major" ] && [ "$minor" -lt "$least_minor" ]; }; then
	fail "Elsecall places Xcode $MINIMUM or later" 64
fi
bundle="$STORE/Xcode-$version.app"

# Whether $1 is root's (the owner here) and writable by no one else.
guarded() {
	[ "$(stat -f %Su:%Sg "$1")" = "$OWNER" ] || return 1
	case "$(stat -f %Sp "$1")" in
	?????w???? | ????????w?) return 1 ;;
	esac
}

# The store and the folder it is in, root's alone.
store() {
	[ -d "$STORE" ] || mkdir -p "$STORE"
	for folder in "$(dirname "$STORE")" "$STORE"; do
		[ -d "$folder" ] && [ ! -L "$folder" ] && guarded "$folder" || fail "$folder is not root's alone" 77
	done
}

# The account's staging folder.
staging() {
	user=$ACCOUNT
	@STAGING@
}

# Whether $1 is Xcode of $version, signed by Apple, as a whole.
verified() {
	[ "$(plutil -extract CFBundleIdentifier raw -o - "$1/Contents/Info.plist" 2>/dev/null)" = com.apple.dt.Xcode ] || return 1
	[ "$(plutil -extract CFBundleShortVersionString raw -o - "$1/Contents/Info.plist" 2>/dev/null)" = "$version" ] || return 1
	@CODESIGN@ --verify --deep --strict -R='anchor apple and identifier "com.apple.dt.Xcode"' "$1" >/dev/null 2>&1
}

# Accepts the license of the Xcode at $1, and runs its first-launch tasks,
# which install system packages, only when $version is the newest placed, so an
# older release's never replace newer ones.
accept() {
	"$1/Contents/Developer/usr/bin/xcodebuild" -license accept || return 1
	newest=$( (ls "$STORE"; echo "Xcode-$version.app") | sed -n 's/^Xcode-\([0-9.]*\)\.app$/\1/p' | sort -t. -k1,1n -k2,2n -k3,3n | tail -n 1)
	if [ "$newest" = "$version" ]; then
		"$1/Contents/Developer/usr/bin/xcodebuild" -runFirstLaunch || return 1
	fi
}

# A bundle this helper placed, in the store.
placed() {
	store
	[ -d "$bundle" ] && [ ! -L "$bundle" ] || fail "Xcode $version is not placed" 66
	guarded "$bundle" || fail "$bundle is not root's alone" 77
}

case "$action" in
place)
	store
	# One placing of a version at a time, or two copies could nest.
	lock="$STORE/.placing-$version"
	mkdir "$lock" 2>/dev/null || fail "Xcode $version is already being placed" 75
	work=
	trap 'rmdir "$lock"; [ -z "$work" ] || { chflags -R nouchg,nouappnd "$work" 2>/dev/null; rm -rf "$work"; }' EXIT
	if [ -e "$bundle" ] || [ -L "$bundle" ]; then
		fail "$bundle already exists" 73
	fi
	source="$(staging)/Xcode-$version.app"
	[ -d "$source" ] && [ ! -L "$source" ] || fail "nothing is staged for Xcode $version" 66
	work=$(mktemp -d "$STORE/.work.XXXXXX")
	# A copy, not a move: the account keeps no handle on what root verifies.
	# Extended attributes and ACLs stay behind, and the flags an account can
	# set are cleared; not every flag: Xcode's files are stored compressed.
	ditto --norsrc --noextattr --noqtn --noacl --clone "$source" "$work/Xcode.app"
	chflags -R nouchg,nouappnd "$work/Xcode.app"
	chown -R "$OWNER" "$work/Xcode.app"
	chmod -R go-w,ug-s "$work/Xcode.app"
	verified "$work/Xcode.app" || fail "the staged bundle is not Apple's Xcode $version" 65
	# Accepted before it is placed, so a placed Xcode is a ready one and a
	# failure leaves nothing placed for the next try to skip.
	accept "$work/Xcode.app" || fail "Xcode $version's license or first-launch tasks failed" 70
	mv "$work/Xcode.app" "$bundle"
	;;
select)
	placed
	@XCODE_SELECT@ --switch "$bundle"
	;;
license)
	placed
	accept "$bundle" || fail "Xcode $version's license or first-launch tasks failed" 70
	;;
*)
	fail "usage: place|select|license VERSION" 64
	;;
esac
