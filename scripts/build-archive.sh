#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Builds the release archive for <target> on a machine of that target: `cargo auditable
# build --release --locked`, a check that the binary holds the audit section `.dep-v0`,
# the ONNX Runtime library, the notices, and dist/knoggin-<version>-<target>.tar.gz with
# bin/knoggin, lib/libonnxruntime.<ext>, LICENSE and THIRD_PARTY_NOTICES. release.yml and
# scripts/release-dry-run.sh both call it. Build the web UI first (pnpm -C ui build): the
# release binary embeds ui/dist.
#
# Needs cargo-auditable and cargo-about on PATH.
#
# Usage: scripts/build-archive.sh <target>
set -eu

if [ $# -ne 1 ]; then
    echo "usage: $0 <target>" >&2
    exit 2
fi
target="$1"
cd "$(dirname "$0")/.."

host=$(rustc -vV | sed -n 's/^host: //p')
if [ "$host" != "$target" ]; then
    echo "this machine builds $host, not $target; build each target on its own runner" >&2
    exit 1
fi
# rust-embed allows a missing folder, so without this check a build ships no web UI.
if [ ! -f ui/dist/index.html ]; then
    echo "ui/dist/index.html is missing; run pnpm -C ui build first" >&2
    exit 1
fi
version=$(awk -F'"' '/^\[workspace.package\]/ { p = 1 } p && /^version = / { print $2; exit }' Cargo.toml)

cargo auditable build --release --locked -p knoggin
bin=target/release/knoggin
# A build can leave the audit section out without an error.
case "$target" in
    *-linux-*) readelf -S "$bin" | grep -q '\.dep-v0' ;;
    *-apple-darwin) otool -l "$bin" | grep -q '\.dep-v0' ;;
esac || {
    echo "$bin has no .dep-v0 audit section" >&2
    exit 1
}
built=$("$bin" --version)
if [ "$built" != "knoggin $version" ]; then
    echo "the binary says '$built', the manifest says $version" >&2
    exit 1
fi

lib=$(scripts/fetch-onnxruntime.sh "$target")
scripts/third-party-notices.sh "$target"

stage="target/archive/$target"
rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/lib" dist
cp "$bin" "$stage/bin/knoggin"
cp "$lib" "$stage/lib/"
cp LICENSE THIRD_PARTY_NOTICES "$stage/"
chmod 755 "$stage/bin/knoggin" "$stage/lib/"*
chmod 644 "$stage/LICENSE" "$stage/THIRD_PARTY_NOTICES"
archive="dist/knoggin-$version-$target.tar.gz"
# COPYFILE_DISABLE keeps macOS tar from adding ._ files for extended attributes.
COPYFILE_DISABLE=1 tar -czf "$archive" -C "$stage" bin lib LICENSE THIRD_PARTY_NOTICES
echo "$archive"
