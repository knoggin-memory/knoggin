#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Prints the newest of <version> and the versions of the published releases (no drafts,
# no pre-releases, tags vX.Y.Z), or of those in the line <X.Y> when it is given.
# release.yml uses it so that "Latest", X.Y and latest only ever move forward, whatever
# order two releases finish in.
#
# Needs gh, GH_TOKEN and GITHUB_REPOSITORY. KNOGGIN_RELEASES replaces the list of
# release tags, one per line, for a test.
#
# Usage: scripts/newest-release.sh <version> [<X.Y>]
set -eu

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
    echo "usage: $0 <version> [<X.Y>]" >&2
    exit 2
fi
if [ -n "${KNOGGIN_RELEASES+set}" ]; then
    tags=$KNOGGIN_RELEASES
else
    tags=$(gh release list --repo "$GITHUB_REPOSITORY" --exclude-drafts --exclude-pre-releases \
        --limit 1000 --json tagName --jq '.[].tagName')
fi
printf '%s\n' "$tags" \
    | sed -n 's/^v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)$/\1/p' \
    | { cat; echo "$1"; } \
    | awk -v line="${2:+$2.}" 'line == "" || index($0, line) == 1' \
    | sort -t. -k1,1n -k2,2n -k3,3n \
    | tail -n 1
