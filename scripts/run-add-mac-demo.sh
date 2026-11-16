#!/bin/sh
# Build a separate local app with a neutral fleet and a harmless text payload.
# No service calls, invitations, installer, installation or enrollment.
set -eu
cd "$(dirname "$0")/.."
cargo build -p elsecall-ui --locked
app="$PWD/target/add-mac-demo/Elsecall Add Mac Demo.app"
mkdir -p "$app/Contents/MacOS"
cp target/debug/elsecall-ui "$app/Contents/MacOS/elsecall-ui"
cat > "$app/Contents/MacOS/demo" <<'SH'
#!/bin/sh
exec "$(dirname "$0")/elsecall-ui" --scene demo-airdrop "$@"
SH
chmod 755 "$app/Contents/MacOS/demo"
cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.elsecall.add-mac-demo</string>
<key>CFBundleName</key><string>Elsecall Add Mac Demo</string>
<key>CFBundleExecutable</key><string>demo</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
/usr/bin/codesign --force --sign - "$app"
if [ "${1:-}" != "--build-only" ]; then
    /usr/bin/open -n "$app"
fi
