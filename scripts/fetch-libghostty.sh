#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Sash contributors
# SPDX-License-Identifier: MIT

# Build the libghostty-vt static library that sash links against.
#
# Ghostty's stable VT releases are source-only, so this fetches the pinned
# commit and runs Zig. Zig 0.17 or newer is required; set ZIG to choose a
# specific compiler. The result is cached under vendor/ghostty-vt/<version>.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dest="$root/vendor/ghostty-vt"
version="1.0.2"
commit="573c4a21e02311ae87844af26b6a7a179f82858e"
sha256="cf514fe054ed8abe849a0e136eb82e036fadeb7ed48f24127d85f801341c5642"
archive_name="ghostty-$commit.tar.gz"
url="https://github.com/ghostty-org/ghostty/archive/$commit.tar.gz"
out="$dest/$version"

if [ -f "$out/lib/libghostty-vt.a" ]; then
    echo "libghostty-vt $version ready at $out"
    exit 0
fi

zig="${ZIG:-zig}"
if ! command -v "$zig" >/dev/null 2>&1; then
    echo "building libghostty-vt $version needs Zig 0.17+ (https://ziglang.org/download/)" >&2
    exit 1
fi
zig_version="$("$zig" version)"
major="${zig_version%%.*}"
minor="${zig_version#*.}"
minor="${minor%%.*}"
if [ "$major" -eq 0 ] && [ "$minor" -lt 17 ]; then
    echo "Zig $zig_version is too old; libghostty-vt $version needs 0.17+" >&2
    exit 1
fi

digest() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

mkdir -p "$dest"
archive="$dest/$archive_name"
if [ ! -f "$archive" ]; then
    echo "fetching libghostty-vt $version ($commit)"
    curl -fL --retry 3 -o "$archive" "$url"
fi
if [ "$(digest "$archive")" != "$sha256" ]; then
    echo "libghostty-vt source checksum mismatch; delete $archive and retry" >&2
    exit 1
fi

src="$(mktemp -d "${TMPDIR:-/tmp}/sash-ghostty.XXXXXX")"
trap 'rm -rf "$src"' EXIT
tar -xzf "$archive" -C "$src" --strip-components=1
echo "building libghostty-vt $version with Zig $zig_version"
(cd "$src" && "$zig" build -Demit-lib-vt -Doptimize=ReleaseFast --prefix "$out")
# The prefix also installs shared libraries, and the linker prefers those over
# the archive. Sash links statically, so drop them.
rm -f "$out"/lib/libghostty-vt*.dylib "$out"/lib/libghostty-vt*.so
echo "libghostty-vt $version ready at $out"
