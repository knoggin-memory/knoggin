#!/usr/bin/env bash
# Makes tests/fixtures/recalld-data/: a recalld data directory made by recalld
# at the reference commit through its own REST API, and beside its files the
# lines the importer yields from it (expected-export.jsonl) and the counts that
# `knoggin import recalld --json` prints (expected-counts.json). Spec tab
# "Config, CLI, local install", "Import from recalld", "Test fixture".
#
# Usage: scripts/make-recalld-data-fixture.sh <recalld checkout> [work dir]
#
# The recalld checkout must be at the reference commit, built with
# --features bench (the script builds it if the binary is missing). Every
# recalld process runs from a fresh work dir with HOME, data dir, socket and
# logs inside it, and never touches a recalld that is installed or running on
# the machine. The script stops if the importer's counts differ from recalld's
# own namespace statistics.
set -euo pipefail

REFERENCE_COMMIT=a1b8a245117d3972c87bb403d39bf86ab6862102
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"
out="$repo/tests/fixtures/recalld-data"

src="${1:?usage: $0 <recalld checkout> [work dir]}"
src="$(cd "$src" && pwd)"
head="$(git -C "$src" rev-parse HEAD)"
if [[ "$head" != "$REFERENCE_COMMIT" ]]; then
  echo "recalld checkout is at $head, not $REFERENCE_COMMIT" >&2
  exit 1
fi
bin="$src/target/release/recalld"
if [[ ! -x "$bin" ]]; then
  (cd "$src" && cargo build --release --features bench)
fi

if [[ -n "${2:-}" ]]; then
  work="$2"
  mkdir -p "$work"
else
  work="$(mktemp -d "${TMPDIR:-/tmp}/recalld-data-fixture.XXXXXX")"
  trap 'rm -rf "$work"' EXIT
fi
if [[ -n "$(ls -A "$work")" ]]; then
  echo "work dir $work is not empty" >&2
  exit 1
fi
work="$(cd "$work" && pwd)"

python3 -I "$here/make-recalld-data-fixture.py" --recalld "$bin" --work "$work"

rm -rf "$out"
cp -R "$work/data" "$out"
(cd "$repo" && cargo test -p knoggin-import-recalld --test fixture --locked -- \
  --ignored --exact write_expected_files --test-threads 1)

# The importer's counts against recalld's own namespace statistics.
python3 -I - "$work/recalld-stats.json" "$out" <<'EOF' || { rm -rf "$out"; exit 1; }
import json, sys
stats = json.load(open(sys.argv[1]))["namespaces"].values()
counts = json.load(open(f"{sys.argv[2]}/expected-counts.json"))
header = json.loads(open(f"{sys.argv[2]}/expected-export.jsonl").readline())
recalld = {
    "namespaces": len(stats),
    "memories": {p: sum(s["phaseCounts"][p] for s in stats) for p in ("full", "summary", "ghost")},
    "forgottenSkipped": sum(s["tombstoneCount"] for s in stats),
    "edgesRead": sum(s["edgeCount"] for s in stats),
}
importer = {
    "namespaces": counts["namespaces"],
    "memories": counts["memories"],
    "forgottenSkipped": counts["forgottenSkipped"],
    "edgesRead": header["counts"]["edges"] + counts["edgesSkipped"],
}
print(f"recalld:  {json.dumps(recalld)}\nimporter: {json.dumps(importer)}")
if recalld != importer or counts["errors"] != 0:
    sys.exit("make-recalld-data-fixture: the importer's counts differ from recalld's")
EOF
du -sh "$out"
