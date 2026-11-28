#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Checks that code, comments and docs stand alone: a reader needs nothing outside
# this repository to follow them. Fails on lines that match the pattern below in
# every tracked text file except lockfiles, in each commit message of an optional
# range, and in PR_TITLE / PR_BODY when set. Known false hits go in
# scripts/check-wording.allow.
#
# Usage: scripts/check-wording.sh [<base>..<head>]   (or set WORDING_RANGE)
set -eu

cd "$(git rev-parse --show-toplevel)"

# Phrases match in any case. Short IDs match upper case only: lower case q4
# (model names), p95 (percentiles) and m0 (variables) are ordinary words here.
pattern='(?i:per the design|\bthe spec\b|tab "|spec-deviations|\bmilestone ?[0-9]|\bmeasurement M[0-9])|\b(?:S?Q|M|I)[0-9]{1,2}\b|\bP[0-9]{2}\b'
allow=scripts/check-wording.allow
range="${1:-${WORDING_RANGE:-}}"

hits=$(mktemp)
trap 'rm -f "$hits"' EXIT

# Prints "<label>:<line>:<text>" for each matching line of stdin.
scan() {
    L="$1" P="$pattern" perl -ne 'print "$ENV{L}:$.:$_" if /$ENV{P}/' >>"$hits"
}

# This script and its allow-list hold the patterns themselves.
rc=0
git grep -P -I -n -e "$pattern" -- . \
    ':(exclude)scripts/check-wording.sh' ":(exclude)$allow" \
    ':(exclude,glob)**/*.lock' ':(exclude,glob)**/*.lock.hcl' ':(exclude,glob)**/pnpm-lock.yaml' \
    >"$hits" || rc=$?
[ "$rc" -le 1 ] || exit "$rc"

if [ -n "$range" ]; then
    for commit in $(git rev-list --no-merges "$range"); do
        git log -1 --format=%B "$commit" | scan "commit $(git rev-parse --short "$commit")"
    done
fi
if [ -n "${PR_TITLE+set}" ]; then
    printf '%s\n' "$PR_TITLE" | scan "PR title"
fi
if [ -n "${PR_BODY+set}" ]; then
    printf '%s\n' "$PR_BODY" | scan "PR body"
fi

# An allow-list line "<path>:<text>" drops hits in <path> whose line contains <text>.
found=$(awk '
    NR == FNR {
        if ($0 !~ /^#/ && (i = index($0, ":")) > 0) { p[++n] = substr($0, 1, i); t[n] = substr($0, i + 1) }
        next
    }
    {
        for (k = 1; k <= n; k++)
            if (substr($0, 1, length(p[k])) == p[k] && index(substr($0, length(p[k]) + 1), t[k])) next
        print
    }' "$allow" "$hits")

if [ -n "$found" ]; then
    printf '%s\n' "$found" >&2
    echo "Hits: $(printf '%s\n' "$found" | wc -l | tr -d ' '). Reword each line above so it stands alone;" >&2
    echo "add a genuine false hit to $allow." >&2
    exit 1
fi
echo "No outside citations found${range:+ (tree and commits in $range)}."
