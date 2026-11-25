#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Sash contributors
# SPDX-License-Identifier: MIT

# Fetch the prebuilt libghostty-vt XCFramework that sash links against.
#
# The upstream "tip" release is rebuilt on every commit to Ghostty's main branch,
# so the checksum is pinned: if upstream moved, the download fails loudly and you
# should either re-pin the checksum (after checking the header/API diff) or build
# libghostty-vt from a tagged release with Zig.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dest="$root/vendor/ghostty-vt"
version="1.1.0-dev+27ffca0"
sha256="d89b0b8e979a8df734a8a57383a6cf9b5bd6ef9c4b16cf7efd0ffab04b39b97a"
url="https://github.com/ghostty-org/ghostty/releases/download/tip/ghostty-vt.xcframework.zip"

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
