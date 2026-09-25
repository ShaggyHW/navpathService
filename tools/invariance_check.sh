#!/usr/bin/env bash
# Cross-snapshot cost-invariance gate (docs/optimization_roadmap_v2.md §9.3).
#
# Route COSTS must be invariant across every heuristic-only axis: landmark count,
# active-landmark count, and search engine (uni vs bidir — the replay comparator runs
# both internally). This script builds 32- and 64-landmark snapshots from the tile DB
# and replays the golden corpus against each, across NAVPATH_ACTIVE_LANDMARKS in
# {4, 8}. Any cost drift beyond final-ulp tie noise (1e-4 relative) — e.g. the
# historical 16 ms-quantum bug (44909 vs the true 41649) — fails the run.
#
# Pop counts legitimately vary across these axes (weaker bounds -> more pops), so the
# corpus pops_max is relaxed via --pops-slack; the strict pops gate is the default
# replay run against the deployed snapshot.
#
# Usage: tools/invariance_check.sh [path/to/worldReachableTiles.db]
#
# Environment:
#   INVARIANCE_POPS_SLACK  pops_max multiplier (default 64)
#   INVARIANCE_JOBS        concurrent replays (default 2; each maps a snapshot and
#                          allocates its search contexts — this host swaps, keep it small)
#   INVARIANCE_REUSE=0     always rebuild both snapshots (default 1: reuse, see below)
#   INVARIANCE_KEEP=1      keep built snapshots in target/invariance/ (~0.2 GB each) so
#                          the next run with the same DB + builder skips the build
#   INVARIANCE_DEPLOYED    deployed snapshot to reuse when identical (default graph.snapshot)
#
# Snapshot reuse (efficiency audit T5.13): a stamp keyed on (sha256 of the DB, sha256
# of the builder binary, landmark count) records the blake3 tail hash of the snapshot
# that build produced. When the deployed graph.snapshot (or a kept build) carries that
# exact hash it IS that build, byte for byte, and is used instead of rebuilding. Any
# change to the DB or the builder (i.e. to navpath-core/-builder sources) changes the
# key, so a stale snapshot can never stand in for a fresh build.
#
# Scratch space is target/tmp, never tmpfs /tmp (the two snapshots are ~0.45 GB).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DB="${1:-$ROOT/worldReachableTiles.db}"
# The corpus pops_max values are blessed on the deployed 64-landmark SCC snapshot; a
# 32-landmark table legitimately needs up to ~40x more pops on the long incident routes
# (still cost-identical, which is what this script checks). 64x keeps the pops gate as a
# plateau/regression alarm without flagging the smaller table.
POPS_SLACK="${INVARIANCE_POPS_SLACK:-64}"
JOBS="${INVARIANCE_JOBS:-2}"
REUSE="${INVARIANCE_REUSE:-1}"
KEEP="${INVARIANCE_KEEP:-0}"
DEPLOYED="${INVARIANCE_DEPLOYED:-$ROOT/graph.snapshot}"
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
STATE="$TARGET/invariance"

[ -f "$DB" ] || { echo "invariance: no DB at $DB"; exit 1; }

export TMPDIR="$TARGET/tmp"
mkdir -p "$TMPDIR" "$STATE"
TMP="$(mktemp -d "$TMPDIR/navpath-invariance.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
T0=$SECONDS

echo "== building binaries =="
cargo build --release -p navpath-builder --manifest-path "$ROOT/Cargo.toml"
cargo build --release -p navpath-service --example replay --manifest-path "$ROOT/Cargo.toml"
BUILDER="$TARGET/release/navpath-builder"
REPLAY="$TARGET/release/examples/replay"

tail_hash() { tail -c 32 "$1" | od -An -v -tx1 | tr -d ' \n'; }
DB_HASH="$(sha256sum "$DB" | cut -c1-16)"
BUILDER_HASH="$(sha256sum "$BUILDER" | cut -c1-16)"
DEPLOYED_HASH=""
[ -f "$DEPLOYED" ] && DEPLOYED_HASH="$(tail_hash "$DEPLOYED")"

# Resolve (reuse or build) the LM-landmark snapshot; sets SNAP_PATH.
resolve_snapshot() {
  local lm="$1"
  local stamp="$STATE/${DB_HASH}_${BUILDER_HASH}_lm${lm}.tail"
  local kept="$STATE/graph_lm${lm}.snapshot"
  if [ "$REUSE" = "1" ] && [ -s "$stamp" ]; then
    local want
    want="$(cat "$stamp")"
    if [ -n "$DEPLOYED_HASH" ] && [ "$DEPLOYED_HASH" = "$want" ]; then
      echo "== $lm landmarks: reusing $DEPLOYED (hash matches this DB + builder) =="
      SNAP_PATH="$DEPLOYED"
      return
    fi
    if [ -f "$kept" ] && [ "$(tail_hash "$kept")" = "$want" ]; then
      echo "== $lm landmarks: reusing kept build $kept =="
      SNAP_PATH="$kept"
      return
    fi
  fi
  echo "== building $lm-landmark snapshot =="
  local t=$SECONDS
  local out="$TMP/graph_$lm.snapshot"
  # --no-walkable: the replays never read walkableTiles.bin (nor tiles.bin, so no
  # --out-tiles either).
  "$BUILDER" --sqlite "$DB" --out-snapshot "$out" --landmarks "$lm" --no-walkable >/dev/null
  local got
  got="$(tail_hash "$out")"
  echo "$got" > "$stamp"
  echo "   built in $((SECONDS - t)) s (tail ${got:0:16})"
  if [ -n "$DEPLOYED_HASH" ] && [ "$DEPLOYED_HASH" = "$got" ]; then
    echo "   identical to $DEPLOYED: later runs with this DB + builder will reuse it"
  fi
  if [ "$KEEP" = "1" ]; then
    mv -f "$out" "$kept"
    out="$kept"
  fi
  SNAP_PATH="$out"
}

# Replays run in the background, at most $JOBS at a time, each logging to its own file;
# logs are printed in a fixed order once everything has finished. Replays of the first
# snapshot overlap the second snapshot's build.
running=0
slot() {
  while [ "$running" -ge "$JOBS" ]; do
    wait -n || true
    running=$((running - 1))
  done
}
ORDER=()
for LM in 32 64; do
  resolve_snapshot "$LM"
  for AL in 4 8; do
    slot
    log="$TMP/replay_${LM}_${AL}.log"
    ORDER+=("$LM:$AL")
    (
      set +e
      SNAPSHOT_PATH="$SNAP_PATH" NAVPATH_ACTIVE_LANDMARKS="$AL" \
        "$REPLAY" --pops-slack="$POPS_SLACK" "$ROOT/tools/golden_corpus.json" > "$log" 2>&1
      echo $? > "$log.rc"
    ) &
    running=$((running + 1))
  done
done
wait

FAILED=0
for item in "${ORDER[@]}"; do
  LM="${item%%:*}"
  AL="${item##*:}"
  log="$TMP/replay_${LM}_${AL}.log"
  echo "== replay: landmarks=$LM active_landmarks=$AL =="
  cat "$log"
  if [ "$(cat "$log.rc" 2>/dev/null || echo 1)" != "0" ]; then
    echo "INVARIANCE FAILURE at landmarks=$LM active_landmarks=$AL"
    FAILED=1
  fi
done

if [ "$FAILED" -ne 0 ]; then
  echo "invariance check FAILED ($((SECONDS - T0)) s)"
  exit 1
fi
echo "invariance check passed: costs identical across landmark/active-landmark axes ($((SECONDS - T0)) s)"
