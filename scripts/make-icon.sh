#!/usr/bin/env bash
# Render the source SVG and pack it as a macOS .icns.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
src="$root/assets/icon.svg"
png="$root/assets/icon-1024.png"
iconset="$root/assets/AppIcon.iconset"
icns="$root/assets/AppIcon.icns"

sips -s format png "$src" --out "$png" >/dev/null

rm -rf "$iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" "$png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done

iconutil -c icns "$iconset" -o "$icns"
rm -rf "$iconset"
echo "built $icns"
