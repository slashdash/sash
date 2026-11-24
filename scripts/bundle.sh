#!/usr/bin/env bash
# Build a menu-bar .app bundle at dist/Sash.app.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

"$root/scripts/fetch-libghostty.sh"
cargo build --release

app="$root/dist/Sash.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

cp "$root/target/release/sash" "$app/Contents/MacOS/sash"

cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Sash</string>
    <key>CFBundleDisplayName</key>
    <string>Sash</string>
    <key>CFBundleIdentifier</key>
    <string>dev.sash.terminal</string>
    <key>CFBundleExecutable</key>
    <string>sash</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleVersion</key>
    <string>0.1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

codesign --force --sign - "$app" >/dev/null 2>&1 || true

echo "built $app"
echo "open $app   # then press the hotkey (default ⌘⇧Space)"
