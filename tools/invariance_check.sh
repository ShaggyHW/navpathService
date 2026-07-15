#!/usr/bin/env bash
# Cross-snapshot cost-invariance gate (docs/optimization_roadmap_v2.md §9.3).
#
# Route COSTS must be invariant across every heuristic-only axis: landmark count,
# active-landmark count, and search engine (uni vs bidir — the replay comparator runs
# both internally). This script builds fresh 24- and 64-landmark snapshots from the
# tile DB and replays the golden corpus against each, across NAVPATH_ACTIVE_LANDMARKS
# in {4, 8}. Any cost drift beyond final-ulp tie noise (1e-4 relative) — e.g. the
# historical 16 ms-quantum bug (44909 vs the true 41649) — fails the run.
#
# Pop counts legitimately vary across these axes (weaker bounds -> more pops), so the
# corpus pops_max is relaxed via --pops-slack; the strict pops gate is the default
# replay run against the deployed snapshot.
#
# Usage: tools/invariance_check.sh [path/to/worldReachableTiles.db]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DB="${1:-$ROOT/worldReachableTiles.db}"
POPS_SLACK="${INVARIANCE_POPS_SLACK:-16}"

[ -f "$DB" ] || { echo "invariance: no DB at $DB"; exit 1; }

TMP="$(mktemp -d "${TMPDIR:-/tmp}/navpath-invariance.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

echo "== building binaries =="
cargo build --release -p navpath-builder --manifest-path "$ROOT/Cargo.toml"
cargo build --release -p navpath-service --example replay --manifest-path "$ROOT/Cargo.toml"

for LM in 24 64; do
  echo "== building $LM-landmark snapshot =="
  "$ROOT/target/release/navpath-builder" \
    --sqlite "$DB" \
    --out-snapshot "$TMP/graph_$LM.snapshot" \
    --out-tiles "$TMP/tiles_$LM.bin" \
    --landmarks "$LM" >/dev/null
done

FAILED=0
for LM in 24 64; do
  for AL in 4 8; do
    echo "== replay: landmarks=$LM active_landmarks=$AL =="
    if ! SNAPSHOT_PATH="$TMP/graph_$LM.snapshot" NAVPATH_ACTIVE_LANDMARKS="$AL" \
        "$ROOT/target/release/examples/replay" --pops-slack="$POPS_SLACK" "$ROOT/tools/golden_corpus.json"; then
      echo "INVARIANCE FAILURE at landmarks=$LM active_landmarks=$AL"
      FAILED=1
    fi
  done
done

if [ "$FAILED" -ne 0 ]; then
  echo "invariance check FAILED"
  exit 1
fi
echo "invariance check passed: costs identical across landmark/active-landmark axes"
