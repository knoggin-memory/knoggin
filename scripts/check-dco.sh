#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Checks that every commit in <base>..<head> carries a Signed-off-by line
# that matches its author exactly (Developer Certificate of Origin).
# Merge commits are skipped: main takes squash merges only. So are commits by
# Dependabot, which cannot sign off: a bot certifies nothing, and the person who
# merges its pull request is the one who takes the change.
#
# Usage: scripts/check-dco.sh <base> [<head>]   (head defaults to HEAD)
set -eu

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
    echo "usage: $0 <base> [<head>]" >&2
    exit 2
fi
range="$1..${2:-HEAD}"

checked=0
failed=0
for commit in $(git rev-list --no-merges "$range"); do
    checked=$((checked + 1))
    author=$(git log -1 --format='%an <%ae>' "$commit")
    if [ "$author" = "dependabot[bot] <49699333+dependabot[bot]@users.noreply.github.com>" ]; then
        continue
    fi
    if git log -1 --format='%(trailers:key=Signed-off-by,valueonly,unfold)' "$commit" | grep -qxF -- "$author"; then
        continue
    fi
    failed=$((failed + 1))
    git log -1 --format='%h %s' "$commit" >&2
    echo "  missing: Signed-off-by: $author" >&2
done

if [ "$failed" -gt 0 ]; then
    echo "$failed of $checked commits lack a matching Signed-off-by line." >&2
    echo "Sign off with 'git commit -s'; fix older commits with 'git rebase --signoff $1'." >&2
    exit 1
fi
echo "All $checked commits in $range are signed off by their authors."
