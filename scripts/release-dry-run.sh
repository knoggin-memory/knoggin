#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# A release on this machine, with nothing published (tab "Testing, CI, releases",
# "Without the GitHub organization"): builds the web UI and the archive for this
# machine's platform with the release's own scripts, writes the SBOM, the stamped
# install.sh and SHA256SUMS, serves them on a loopback port, and runs install.sh against
# that into a temporary home. Then `knoggin --version` runs from the installed copy.
# It also proves that the installer refuses a wrong hash and, under
# KNOGGIN_REQUIRE_SIGNATURE=1, a file it cannot check the attestation of.
#
# Needs pnpm, cargo-auditable, cargo-about, syft and python3. Nothing is signed, uploaded
# or installed outside the temporary home; the archive stays in dist/.
#
# Usage: scripts/release-dry-run.sh
set -eu

cd "$(dirname "$0")/.."
for tool in pnpm cargo-auditable cargo-about syft python3 curl; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "$tool is needed" >&2
        exit 1
    }
done
target=$(rustc -vV | sed -n 's/^host: //p')
version=$(awk -F'"' '/^\[workspace.package\]/ { p = 1 } p && /^version = / { print $2; exit }' Cargo.toml)

work=$(mktemp -d)
server=
cleanup() {
    if [ -n "$server" ]; then
        kill "$server" 2>/dev/null || true
        wait "$server" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

echo "== web UI"
pnpm -C ui install --frozen-lockfile
pnpm -C ui build

echo "== archive for $target"
scripts/build-archive.sh "$target"

echo "== release files"
serve="$work/serve"
mkdir -p "$serve"
cp "dist/knoggin-$version-$target.tar.gz" "$serve/"
scripts/release-assets.sh "$serve"
(cd "$serve" && if command -v sha256sum >/dev/null 2>&1; then sha256sum -c SHA256SUMS; else shasum -a 256 -c SHA256SUMS; fi)

echo "== serving $serve on loopback"
python3 -I -u -m http.server --bind 127.0.0.1 0 --directory "$serve" > "$work/server.log" 2>&1 &
server=$!
port=
for _ in $(seq 50); do
    port=$(sed -n 's/.* port \([0-9][0-9]*\).*/\1/p' "$work/server.log" | head -n 1)
    [ -n "$port" ] && curl -fsS -o /dev/null "http://127.0.0.1:$port/SHA256SUMS" && break
    port=
    sleep 0.1
done
[ -n "$port" ] || {
    echo "the local server did not start" >&2
    cat "$work/server.log" >&2
    exit 1
}
base="http://127.0.0.1:$port"

home="$work/home"
mkdir -p "$home" "$work/tmp"
# A clean environment: only the temporary home, the base tools and the local address.
run_installer() {
    env -i HOME="$home" PATH=/usr/bin:/bin TMPDIR="$work/tmp" \
        KNOGGIN_INSTALL_BASE_URL="$base" "$@"
}

echo "== install.sh refuses a wrong hash"
sed "s/[0-9a-f]\{64\}/$(printf '0%.0s' $(seq 64))/" "$serve/install.sh" > "$work/wrong-hash.sh"
if run_installer sh "$work/wrong-hash.sh" > "$work/refused.log" 2>&1; then
    echo "install.sh accepted a wrong hash" >&2
    exit 1
fi
grep 'SHA-256 mismatch' "$work/refused.log"

echo "== install.sh refuses an unchecked attestation under KNOGGIN_REQUIRE_SIGNATURE=1"
if run_installer KNOGGIN_REQUIRE_SIGNATURE=1 sh "$serve/install.sh" > "$work/refused.log" 2>&1; then
    echo "install.sh installed without an attestation check" >&2
    exit 1
fi
grep 'KNOGGIN_REQUIRE_SIGNATURE=1' "$work/refused.log"
[ ! -e "$home/.knoggin" ] || {
    echo "a refused install left files behind" >&2
    exit 1
}

echo "== install.sh into $home"
run_installer sh "$serve/install.sh"

echo "== knoggin --version from the install"
ls -l "$home/.local/bin/knoggin"
find "$home/.knoggin" -type f | sed "s|$home/||" | sort
"$home/.local/bin/knoggin" --version
