#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Writes THIRD_PARTY_NOTICES at the repository root (tab "Testing, CI, releases"): the
# licences of the Rust crates (cargo-about with about.hbs and about.toml), ONNX
# Runtime's LICENSE and ThirdPartyNotices.txt from target/onnxruntime/<target>/ (run
# scripts/fetch-onnxruntime.sh <target> first), and the model's licence.
#
# Usage: scripts/third-party-notices.sh <target>
set -eu

if [ $# -ne 1 ]; then
    echo "usage: $0 <target>" >&2
    exit 2
fi
cd "$(dirname "$0")/.."
ort="target/onnxruntime/$1"
for f in "$ort/LICENSE" "$ort/ThirdPartyNotices.txt" third_party/model-licence.txt; do
    if [ ! -s "$f" ]; then
        echo "$f is missing; run scripts/fetch-onnxruntime.sh $1 first" >&2
        exit 1
    fi
done

rule() {
    printf '\n================================================================================\n%s\n\n' "$1"
}

{
    cargo about generate --locked about.hbs
    rule "ONNX Runtime 1.31.1: LICENSE"
    cat "$ort/LICENSE"
    rule "ONNX Runtime 1.31.1: ThirdPartyNotices.txt"
    cat "$ort/ThirdPartyNotices.txt"
    rule "The embedding model"
    cat third_party/model-licence.txt
} > THIRD_PARTY_NOTICES.part
mv THIRD_PARTY_NOTICES.part THIRD_PARTY_NOTICES
