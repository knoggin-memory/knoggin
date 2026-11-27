#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Turns the archives in <dir> into a release's files (tab "Testing, CI, releases", the
# publish job; release.yml runs it in the job assets): an SPDX SBOM per archive made by
# syft from what the archive holds, install.sh stamped with the version and each
# archive's SHA-256, and SHA256SUMS over all of them. release.yml and
# scripts/release-dry-run.sh both call it.
#
# Needs syft on PATH.
#
# Usage: scripts/release-assets.sh <dir>   (holding knoggin-<version>-<target>.tar.gz)
set -eu

if [ $# -ne 1 ]; then
    echo "usage: $0 <dir>" >&2
    exit 2
fi
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$1"
version=$(awk -F'"' '/^\[workspace.package\]/ { p = 1 } p && /^version = / { print $2; exit }' "$root/Cargo.toml")

if command -v sha256sum >/dev/null 2>&1; then
    sums() { sha256sum "$@"; }
else
    sums() { shasum -a 256 "$@"; }
fi

set -- knoggin-"$version"-*.tar.gz
[ -f "$1" ] || {
    echo "no knoggin-$version-*.tar.gz in $(pwd)" >&2
    exit 1
}

stamp="s/@VERSION@/$version/g"
unpacked=
trap 'rm -rf "$unpacked"' EXIT
trap 'exit 130' INT TERM
for archive in "$@"; do
    target=${archive#knoggin-"$version"-}
    target=${target%.tar.gz}
    unpacked=$(mktemp -d)
    tar -xzf "$archive" -C "$unpacked"
    # syft reads the crate list that cargo-auditable put into the binary.
    syft scan "dir:$unpacked" --quiet --source-name knoggin --source-version "$version" \
        -o "spdx-json=knoggin-$version-$target.spdx.json"
    rm -rf "$unpacked"
    hash=$(sums "$archive" | cut -d' ' -f1)
    placeholder=$(echo "$target" | tr 'a-z-' 'A-Z_')
    stamp="$stamp;s/@SHA256_${placeholder}@/$hash/g"
done
sed "$stamp" "$root/install.sh" > install.sh
chmod 755 install.sh

rm -f SHA256SUMS
sums knoggin-"$version"-* install.sh > SHA256SUMS.part
mv SHA256SUMS.part SHA256SUMS
cat SHA256SUMS
