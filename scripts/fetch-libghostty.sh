#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Sash contributors
# SPDX-License-Identifier: MIT

# Fetch the prebuilt libghostty-vt XCFramework that sash links against.
#
# Ghostty rebuilds its "tip" release on every commit, replacing the asset, so the
# exact pinned build is mirrored as a release in this repository. The checksum
# still guards the bytes; update the URL and checksum together when upgrading.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dest="$root/vendor/ghostty-vt"
version="1.1.0-dev+27ffca0"
sha256="d89b0b8e979a8df734a8a57383a6cf9b5bd6ef9c4b16cf7efd0ffab04b39b97a"
url="https://github.com/slashdash/sash/releases/download/ghostty-vt-27ffca0/vt.xcframework.zip"

mkdir -p "$dest"
if [ ! -f "$dest/vt.xcframework.zip" ]; then
    echo "fetching libghostty-vt $version"
    curl -fL --retry 3 -o "$dest/vt.xcframework.zip" "$url"
fi

echo "$sha256  $dest/vt.xcframework.zip" | shasum -a 256 -c -

if [ ! -d "$dest/ghostty-vt.xcframework" ]; then
    unzip -oq "$dest/vt.xcframework.zip" -d "$dest"
fi

echo "libghostty-vt ready at $dest/ghostty-vt.xcframework"
