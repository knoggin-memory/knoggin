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

    # The archive holds the folders bin/ and lib/ and the plain files bin/knoggin, the
    # library, LICENSE and THIRD_PARTY_NOTICES, nothing else: no links of any kind. The
    # first character of a verbose listing line is the entry's type, in GNU and BSD tar.
    tar -tzf "$tmp/$archive" > "$tmp/list" || die "$archive is not a readable archive"
    tar -tvzf "$tmp/$archive" > "$tmp/types" || die "$archive is not a readable archive"
    [ "$(wc -l < "$tmp/list")" -eq "$(wc -l < "$tmp/types")" ] \
        || die "$archive holds a name that tar cannot list"
    while IFS= read -r entry && IFS= read -r line <&3; do
        case "$entry" in
            /* | *..*) die "$archive holds an unsafe path: $entry" ;;
            bin/ | lib/) want=d ;;
            bin/knoggin | lib/libonnxruntime.so | lib/libonnxruntime.dylib | LICENSE | THIRD_PARTY_NOTICES) want=- ;;
            *) die "$archive holds an unexpected path: $entry" ;;
        esac
        [ "${line%"${line#?}"}" = "$want" ] \
            || die "$archive holds $entry as a link or special file, not as a plain file or folder"
    done < "$tmp/list" 3< "$tmp/types"

    # ~/.knoggin also holds the data: only bin, lib and the two text files are replaced.
    # The archive is unpacked inside it, so that each move below is a rename.
    home_dir="$HOME/.knoggin"
    if [ ! -d "$home_dir" ]; then
        mkdir -p "$home_dir"
        chmod 700 "$home_dir"
    fi
    new=$(mktemp -d "$home_dir/.install.XXXXXX")
    trap 'rm -rf "$tmp" "$new"' EXIT
    tar -xzf "$tmp/$archive" -C "$new"
    [ -f "$new/bin/knoggin" ] || die "$archive has no bin/knoggin"
    ls "$new/lib/libonnxruntime".* >/dev/null 2>&1 || die "$archive has no ONNX Runtime library"

    if ! replace_parts; then
        restore_parts
        die "could not move the new bin and lib into $home_dir, which is left as it was"
    fi
    rm -rf "$home_dir/bin.old" "$home_dir/lib.old"
    for file in LICENSE THIRD_PARTY_NOTICES; do
        mv -f "$new/$file" "$home_dir/$file"
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

# Moves the old bin and lib aside, then the new lib in, then the new bin, so that a
# binary is never in place without its library. Records what it did for restore_parts.
replace_parts() {
    moved=
    placed=
    rm -rf "$home_dir/bin.old" "$home_dir/lib.old" || return 1
    for part in bin lib; do
        if [ -e "$home_dir/$part" ] || [ -L "$home_dir/$part" ]; then
            mv "$home_dir/$part" "$home_dir/$part.old" || return 1
            moved="$moved $part"
        fi
    done
    for part in lib bin; do
        mv "$new/$part" "$home_dir/$part" || return 1
        placed="$placed $part"
    done
}

# Undoes what replace_parts did before it failed.
restore_parts() {
    for part in $placed; do
        rm -rf "${home_dir:?}/$part"
    done
    for part in $moved; do
        mv "$home_dir/$part.old" "$home_dir/$part" || warn "could not put $home_dir/$part back from $part.old"
    done
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

# Verifies the build attestation with gh, else cosign, each pinned to the release
# workflow. A check that runs and fails stops the installer. So does a gh or cosign that
# is installed but cannot run the check, with what to do about it; no tool at all stops
# it only under KNOGGIN_REQUIRE_SIGNATURE=1.
check_attestation() {
    if [ -n "${KNOGGIN_INSTALL_BASE_URL:-}" ]; then
        missing_attestation "the archive came from KNOGGIN_INSTALL_BASE_URL, which has no attestation"
        return
    fi
    unusable=
    if command -v gh >/dev/null 2>&1; then
        if ! gh attestation verify --help 2>/dev/null | grep -q -- '--cert-identity'; then
            unusable="gh is too old to verify attestations (it has no 'gh attestation verify --cert-identity')"
        elif ! gh auth status --hostname github.com >/dev/null 2>&1; then
            unusable="gh is not signed in to github.com (run 'gh auth login')"
        else
            gh attestation verify "$tmp/$archive" --repo "$repo" --cert-identity "$workflow" >/dev/null \
                || die "the build attestation of $archive did not verify (gh)"
            say "build attestation checked with gh"
            return
        fi
    fi
    if command -v cosign >/dev/null 2>&1; then
        if ! cosign verify-blob-attestation --help 2>&1 | grep -q -- '--new-bundle-format'; then
            unusable="${unusable:+$unusable; }cosign is too old to read the attestation bundle (it has no --new-bundle-format)"
        else
            check_with_cosign
            return
        fi
    fi
    if [ -n "$unusable" ]; then
        die "could not check the build attestation: $unusable. Install a current gh or cosign, or take the one that cannot run the check off PATH to install with the SHA-256 check alone"
    fi
    missing_attestation "neither gh nor cosign is installed"
}

check_with_cosign() {
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
}

missing_attestation() {
    if [ "${KNOGGIN_REQUIRE_SIGNATURE:-}" = 1 ]; then
        die "no attestation check ran ($1), and KNOGGIN_REQUIRE_SIGNATURE=1"
    fi
    say "build attestation not checked: $1; only the SHA-256 was checked"
}

main "$@"
