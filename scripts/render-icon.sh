#!/bin/sh
# Renders the app icon and the repository images from their SVG sources in
# assets/icon: Elsecall.icns for the app bundle, elsecall.png for the README
# and social-preview.png for GitHub. Requires macOS (iconutil) and rsvg-convert
# (brew install librsvg). Run it after changing either SVG and commit the output.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
icon="$root/assets/icon"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Let rsvg-convert find DM Sans in the repository. Pango on macOS asks Core
# Text, which never sees it, unless told to use fontconfig.
export PANGOCAIRO_BACKEND=fc
mkdir -p "$work/cache"
cat >"$work/fonts.conf" <<EOF
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig>
  <dir>$root/crates/elsecall-ui/assets/fonts</dir>
  <cachedir>$work/cache</cachedir>
</fontconfig>
EOF
export FONTCONFIG_FILE="$work/fonts.conf"

render() {
  rsvg-convert --width "$2" --height "$2" --output "$3" "$1"
}

set_dir="$work/Elsecall.iconset"
mkdir "$set_dir"
for size in 16 32 128 256 512; do
  render "$icon/elsecall.svg" "$size" "$set_dir/icon_${size}x${size}.png"
  render "$icon/elsecall.svg" "$((size * 2))" "$set_dir/icon_${size}x${size}@2x.png"
done
iconutil --convert icns --output "$icon/Elsecall.icns" "$set_dir"

render "$icon/elsecall.svg" 256 "$icon/elsecall.png"
rsvg-convert --width 1280 --height 640 --output "$icon/social-preview.png" "$icon/social-preview.svg"
