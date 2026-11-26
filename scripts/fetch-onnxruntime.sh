#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Downloads the pinned ONNX Runtime archive for <target>, checks its SHA-256
# against third_party/onnxruntime.toml, and extracts the library, LICENSE and
# ThirdPartyNotices.txt to target/onnxruntime/<target>/. A mismatch fails.
#
# Usage: scripts/fetch-onnxruntime.sh <target>   (a table name in the toml file)
set -eu

if [ $# -ne 1 ]; then
    echo "usage: $0 <target>" >&2
    exit 2
fi
target="$1"
cd "$(dirname "$0")/.."
pins=third_party/onnxruntime.toml

# The value of <key> in table [<target>].
value() {
    awk -v table="[$target]" -v key="$1" '
        /^\[/ { inside = ($0 == table) }
        inside && $1 == key && $2 == "=" { gsub(/"/, "", $3); print $3; exit }
    ' "$pins"
}
url=$(value url)
sha256=$(value sha256)
path=$(value path)
if [ -z "$url" ] || [ -z "$sha256" ] || [ -z "$path" ]; then
    echo "no complete [$target] table in $pins" >&2
    exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
    hash() { sha256sum "$1" | cut -d' ' -f1; }
else
    hash() { shasum -a 256 "$1" | cut -d' ' -f1; }
fi

downloads=target/onnxruntime/downloads
archive="$downloads/$sha256"
mkdir -p "$downloads"
if [ ! -f "$archive" ] || [ "$(hash "$archive")" != "$sha256" ]; then
    curl --fail --location --silent --show-error --retry 3 \
        --connect-timeout 10 -o "$archive.part" "$url"
    actual=$(hash "$archive.part")
    if [ "$actual" != "$sha256" ]; then
        rm -f "$archive.part"
        echo "SHA-256 mismatch for $url: expected $sha256, got $actual" >&2
        exit 1
    fi
    mv "$archive.part" "$archive"
fi

out="target/onnxruntime/$target"
rm -rf "$out"
mkdir -p "$out"
case "$path" in
    *.dylib) lib=libonnxruntime.dylib ;;
    *) lib=libonnxruntime.so ;;
esac
# The NuGet package is a zip archive; a library inside it is a plain file.
unzip -p "$archive" "$path" > "$out/$lib"
unzip -p "$archive" LICENSE > "$out/LICENSE"
unzip -p "$archive" ThirdPartyNotices.txt > "$out/ThirdPartyNotices.txt"
for f in "$lib" LICENSE ThirdPartyNotices.txt; do
    if [ ! -s "$out/$f" ]; then
        echo "$f is missing from the archive" >&2
        exit 1
    fi
done
echo "$out/$lib"
