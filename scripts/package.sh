#!/usr/bin/env bash
# Build a DMG you can hand to testers.
#
#   scripts/package.sh              native build (Apple Silicon on an M-series Mac)
#   scripts/package.sh --universal  also runs on Intel Macs
#
# The app is ad-hoc signed, not notarized, so each tester approves it once with
# System Settings -> Privacy & Security -> Open Anyway. Read Me.txt inside the
# image walks them through that and through the menu-bar controls.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

case "${1:-}" in
    "") "$root/scripts/bundle.sh" ;;
    --universal) "$root/scripts/bundle.sh" --universal ;;
    *)
        echo "usage: $0 [--universal]" >&2
        exit 2
        ;;
esac

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
sha="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
dmg="$root/dist/Sash-$version-$(date +%Y%m%d).dmg"
stage="$(mktemp -d "${TMPDIR:-/tmp}/sash-dmg.XXXXXX")"
trap 'rm -rf "$stage"' EXIT

ditto "$root/dist/Sash.app" "$stage/Sash.app"
ln -s /Applications "$stage/Applications"

cat > "$stage/Read Me.txt" <<TEXT
Sash $version ($sha) — pre-release test build
==============================================

Sash is a drop-down terminal that lives in the menu bar. This build is ad-hoc
signed but not notarized by Apple, so macOS asks you to approve it once.

FIRST LAUNCH
1. Drag Sash into Applications, then launch it from there. (Launching from
   this disk image runs a temporary copy and loses the app when it unmounts.)
2. macOS says it cannot verify the app. Open System Settings -> Privacy &
   Security, scroll down, click "Open Anyway" next to Sash, authenticate, and
   click "Open".
   Terminal alternative:
       xattr -dr com.apple.quarantine /Applications/Sash.app
3. Sash opens its panel once, on first launch, so you know it is running.

USING SASH
- There is no Dock icon. The ghost appears in the menu bar.
- Cmd+Shift+Space drops the terminal down; press it again to hide.
- Cmd+, opens Settings. The ghost menu also has Settings and Quit.
- If the shortcut types a special character or switches input sources, change
  it in Settings -> Shortcuts.

IF SOMETHING GOES WRONG
- Log: ~/Library/Logs/Sash/sash.log
- Config: ~/.config/sash/config.json
- Send the log plus your macOS version and the build shown in Settings.

UNINSTALL
Quit Sash from the ghost menu, drag it from Applications to the Trash, and
optionally delete the two paths above.
TEXT

rm -f "$dmg" "$dmg.sha256"
hdiutil create -quiet -volname "Sash $version" -srcfolder "$stage" -ov -format UDZO "$dmg"
shasum -a 256 "$dmg" | tee "$dmg.sha256"

echo "send $dmg"
