#!/usr/bin/env bash
# Build a menu-bar .app bundle at dist/Sash.app.
#
#   scripts/bundle.sh              native build (arm64 on Apple Silicon)
#   scripts/bundle.sh --universal  arm64 + x86_64 in one binary
#
# The version comes from Cargo.toml, the build number from git, and the revision
# is baked in so Settings and the menu-bar menu name the exact build.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

universal=0
case "${1:-}" in
    "") ;;
    --universal) universal=1 ;;
    *)
        echo "usage: $0 [--universal]" >&2
        exit 2
        ;;
esac

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
build="$(git rev-list --count HEAD 2>/dev/null || echo 1)"
sha="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
export SASH_BUILD_SHA="$sha"

"$root/scripts/fetch-libghostty.sh"

if [ "$universal" = 1 ]; then
    rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
    cargo build --release --target aarch64-apple-darwin
    cargo build --release --target x86_64-apple-darwin
else
    cargo build --release
fi

app="$root/dist/Sash.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

if [ "$universal" = 1 ]; then
    lipo -create \
        "$root/target/aarch64-apple-darwin/release/sash" \
        "$root/target/x86_64-apple-darwin/release/sash" \
        -output "$app/Contents/MacOS/sash"
else
    cp "$root/target/release/sash" "$app/Contents/MacOS/sash"
fi

if [ -f "$root/assets/AppIcon.icns" ]; then
    cp "$root/assets/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
fi

cat > "$app/Contents/Info.plist" <<PLIST
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
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleVersion</key>
    <string>$build</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

codesign --force --sign - "$app"

echo "built $app ($version, build $build, $sha)"
echo "open $app   # then press the hotkey (default ⌘⇧Space)"
