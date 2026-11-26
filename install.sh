#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Installs Knoggin: downloads the release archive for this machine, checks its SHA-256
# (always) and its build attestation (when `gh` or `cosign` is installed), unpacks it to
# ~/.knoggin/ and links ~/.local/bin/knoggin.
#
#   curl -fsSL https://github.com/knoggin-memory/knoggin/releases/download/v<version>/install.sh | sh
#
# In the repository this is a template: the release fills in the version and one hash
# per target, the @-placeholders in main (scripts/release-assets.sh). The release's copy
# installs exactly its own version.
#
# KNOGGIN_REQUIRE_SIGNATURE=1   stop when no attestation check can run
# KNOGGIN_INSTALL_BASE_URL=URL  download from URL instead of the release; such a file
#                               has no attestation, so only the SHA-256 is checked
#
# Everything is inside main, called on the last line, so a partial download runs nothing.

set -eu

say() { printf 'knoggin install: %s\n' "$*"; }
warn() { printf 'knoggin install: warning: %s\n' "$*" >&2; }
die() {
    printf 'knoggin install: error: %s\n' "$*" >&2
    exit 1
}

main() {
    version='@VERSION@'
    repo='knoggin-memory/knoggin'
    workflow="https://github.com/$repo/.github/workflows/release.yml@refs/heads/main"
    case "$version" in
        @*) die "this is the unstamped template from the repository; use the install.sh of a release" ;;
    esac

    target=$(detect_target) || exit 1
    case "$target" in
        x86_64-unknown-linux-gnu) expected='@SHA256_X86_64_UNKNOWN_LINUX_GNU@' ;;
        aarch64-unknown-linux-gnu) expected='@SHA256_AARCH64_UNKNOWN_LINUX_GNU@' ;;
        aarch64-apple-darwin) expected='@SHA256_AARCH64_APPLE_DARWIN@' ;;
    esac
    case "$expected" in
        @*) die "this installer has no hash for $target" ;;
    esac

    command -v curl >/dev/null 2>&1 || die "curl is needed"
    command -v tar >/dev/null 2>&1 || die "tar is needed"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256() { sha256sum "$1" | cut -d' ' -f1; }
    elif command -v shasum >/dev/null 2>&1; then
        sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
    else
        die "sha256sum or shasum is needed to check the download"
    fi

    archive="knoggin-$version-$target.tar.gz"
    if [ -n "${KNOGGIN_INSTALL_BASE_URL:-}" ]; then
        base="${KNOGGIN_INSTALL_BASE_URL%/}"
        protocols='=https,http,file'
    else
        base="https://github.com/$repo/releases/download/v$version"
        protocols='=https'
    fi

    tmp=$(mktemp -d "${TMPDIR:-/tmp}/knoggin-install.XXXXXX")
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 130' INT TERM

    say "downloading $archive"
    curl --fail --location --silent --show-error --proto "$protocols" --tlsv1.2 \
        --retry 3 --connect-timeout 10 -o "$tmp/$archive" "$base/$archive" \
        || die "could not download $base/$archive"

    actual=$(sha256 "$tmp/$archive")
    [ "$actual" = "$expected" ] \
        || die "SHA-256 mismatch for $archive: expected $expected, got $actual"
    say "SHA-256 checked"

    check_attestation

    # The archive holds bin/, lib/, LICENSE and THIRD_PARTY_NOTICES, nothing else.
    tar -tzf "$tmp/$archive" > "$tmp/list" || die "$archive is not a readable archive"
    while IFS= read -r entry; do
        case "$entry" in
            /* | *..*) die "$archive holds an unsafe path: $entry" ;;
            bin/ | bin/knoggin | lib/ | lib/libonnxruntime.so | lib/libonnxruntime.dylib | LICENSE | THIRD_PARTY_NOTICES) ;;
            *) die "$archive holds an unexpected path: $entry" ;;
        esac
    done < "$tmp/list"
    mkdir "$tmp/new"
    tar -xzf "$tmp/$archive" -C "$tmp/new"
    [ -f "$tmp/new/bin/knoggin" ] || die "$archive has no bin/knoggin"
    ls "$tmp/new/lib/libonnxruntime".* >/dev/null 2>&1 || die "$archive has no ONNX Runtime library"

    # ~/.knoggin also holds the data: only bin, lib and the two text files are replaced.
    home_dir="$HOME/.knoggin"
    if [ ! -d "$home_dir" ]; then
        mkdir -p "$home_dir"
        chmod 700 "$home_dir"
    fi
    for part in bin lib; do
        rm -rf "$home_dir/$part.old"
        if [ -e "$home_dir/$part" ]; then
            mv "$home_dir/$part" "$home_dir/$part.old"
        fi
        mv "$tmp/new/$part" "$home_dir/$part"
        rm -rf "$home_dir/$part.old"
    done
    for file in LICENSE THIRD_PARTY_NOTICES; do
        mv -f "$tmp/new/$file" "$home_dir/$file"
    done
    say "installed knoggin $version to $home_dir"

    link_dir="$HOME/.local/bin"
    link="$link_dir/knoggin"
    mkdir -p "$link_dir"
    if [ -L "$link" ] && [ "$(readlink "$link")" = "$home_dir/bin/knoggin" ]; then
        :
    elif [ -e "$link" ] || [ -L "$link" ]; then
        warn "$link already exists and was left alone; the binary is $home_dir/bin/knoggin"
    else
        ln -s "$home_dir/bin/knoggin" "$link"
        say "linked $link"
    fi
    case ":$PATH:" in
        *":$link_dir:"*) ;;
        *) warn "$link_dir is not on PATH; add it, or run $home_dir/bin/knoggin" ;;
    esac
}

# The target triple of this machine, or a stop with the reason.
detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Darwin)
            # A shell under Rosetta reports x86_64 on Apple silicon.
            if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
                arch=arm64
            fi
            case "$arch" in
                arm64 | aarch64) echo aarch64-apple-darwin ;;
                *) die "Knoggin runs on Apple silicon only; ONNX Runtime has no Intel Mac library" ;;
            esac
            ;;
        Linux)
            # The official ONNX Runtime library and the release build need glibc 2.35.
            glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
            case "$glibc" in
                "glibc "*) ;;
                *) die "Knoggin needs Linux with glibc 2.35 or newer (musl is not supported)" ;;
            esac
            major=$(echo "${glibc#glibc }" | cut -d. -f1)
            minor=$(echo "${glibc#glibc }" | cut -d. -f2)
            if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 35 ]; }; then
                die "Knoggin needs glibc 2.35 or newer; this machine has ${glibc#glibc }"
            fi
            case "$arch" in
                x86_64 | amd64) echo x86_64-unknown-linux-gnu ;;
                aarch64 | arm64) echo aarch64-unknown-linux-gnu ;;
                *) die "no Knoggin build for Linux on $arch" ;;
            esac
            ;;
        *) die "no Knoggin build for $os; on other systems run the container image" ;;
    esac
}

# Verifies the build attestation with gh, else cosign. A check that runs and fails stops
# the installer; no check at all stops it only under KNOGGIN_REQUIRE_SIGNATURE=1.
check_attestation() {
    if [ -n "${KNOGGIN_INSTALL_BASE_URL:-}" ]; then
        missing_attestation "the archive came from KNOGGIN_INSTALL_BASE_URL, which has no attestation"
    elif command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
        gh attestation verify "$tmp/$archive" --repo "$repo" >/dev/null \
            || die "the build attestation of $archive did not verify (gh)"
        say "build attestation checked with gh"
    elif command -v cosign >/dev/null 2>&1; then
        bundle="knoggin-$version.sigstore.json"
        curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 \
            --retry 3 --connect-timeout 10 -o "$tmp/$bundle" "$base/$bundle" \
            || die "could not download the attestation bundle $bundle"
        cosign verify-blob-attestation --bundle "$tmp/$bundle" --new-bundle-format \
            --type slsaprovenance1 \
            --certificate-identity "$workflow" \
            --certificate-oidc-issuer https://token.actions.githubusercontent.com \
            "$tmp/$archive" >/dev/null 2>&1 \
            || die "the build attestation of $archive did not verify (cosign)"
        say "build attestation checked with cosign"
    else
        missing_attestation "neither gh (signed in) nor cosign is installed"
    fi
}

missing_attestation() {
    if [ "${KNOGGIN_REQUIRE_SIGNATURE:-}" = 1 ]; then
        die "no attestation check ran ($1), and KNOGGIN_REQUIRE_SIGNATURE=1"
    fi
    say "build attestation not checked: $1; only the SHA-256 was checked"
}

main "$@"
