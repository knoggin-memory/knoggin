#!/usr/bin/env bash
# Makes tests/fixtures/ranking/fixture.json from recalld at the reference commit.
# See spec tab "Testing, CI and releases", "Making the ranking fixture", and
# tests/fixtures/ranking/README.md for the output format.
#
# Usage: scripts/make-ranking-fixture.sh <recalld checkout> [work dir]
#
# The recalld checkout must be at the reference commit. The script builds it
# with --features bench, runs every recalld process from a fresh work dir with
# HOME, data dir, socket and logs inside it, and never touches a recalld that
# is installed or running on the machine.
set -euo pipefail

REFERENCE_COMMIT=a1b8a245117d3972c87bb403d39bf86ab6862102
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"

src="${1:?usage: $0 <recalld checkout> [work dir]}"
src="$(cd "$src" && pwd)"
head="$(git -C "$src" rev-parse HEAD)"
if [[ "$head" != "$REFERENCE_COMMIT" ]]; then
  echo "recalld checkout is at $head, not $REFERENCE_COMMIT" >&2
  exit 1
fi

(cd "$src" && cargo build --release --features bench)
bin="$src/target/release/recalld"

work="${2:-$(mktemp -d "${TMPDIR:-/tmp}/ranking-fixture.XXXXXX")}"
mkdir -p "$work"
if [[ -n "$(ls -A "$work")" ]]; then
  echo "work dir $work is not empty" >&2
  exit 1
fi
work="$(cd "$work" && pwd)"

exec python3 -I "$here/make-ranking-fixture.py" \
  --recalld "$bin" \
  --commit "$REFERENCE_COMMIT" \
  --work "$work" \
  --out "$repo/tests/fixtures/ranking/fixture.json"
