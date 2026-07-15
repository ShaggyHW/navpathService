#!/usr/bin/env bash
# Perf-regression gate over the criterion corpus (docs/optimization_roadmap_v2.md §9.5).
#
# Criterion's own baselines live under the disposable target/ dir; this gate keeps
# BLESSED median estimates in docs/perf-baselines/<snapshot-hash16>_<rustc>/ so every
# change lands against a durable, versioned reference.
#
# Usage:
#   scripts/perf-gate.sh check [-- <extra criterion args>]   # bench + compare (default)
#   scripts/perf-gate.sh bless [--reuse] [-- <extra args>]   # bench (unless --reuse) + save as baseline
#   scripts/perf-gate.sh install-hook                        # add a git pre-push hook
#
# Notes:
#   - Baselines are keyed on (snapshot tail hash, rustc version): numbers are only
#     comparable for the same snapshot (landmark count changes the hash) and compiler
#     (pinned via rust-toolchain.toml).
#   - [profile.bench] is panic=unwind, so absolute numbers differ slightly from the
#     panic=abort release binary; the gate compares bench-to-bench, which is sound.
#   - Tolerance: PERF_GATE_TOLERANCE (default 0.10) applies to benches not covered by
#     the per-group table below. The table was MEASURED on 2026-07-14 from two full
#     runs of an identical binary: most benches sit within +-4%, but bidir/seeded
#     medium-range searches swing up to +-34% run-to-run (systematic per-process —
#     plausibly cache-set aliasing between the two ~18 MB bidir context arrays; see
#     the roadmap's context-pool items 2.9/2.10, which should shrink this). Tighten
#     the table after that lands.
#   - 'check -- <filter>' (filtered runs) compare a cold-cache process against warm
#     full-suite baselines and read up to ~20% slow on the ms-scale benches even on
#     identical code — treat them as indicative; gate on full runs.
#   - 'check --reuse' skips the bench and re-compares the last run's results.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SNAP="${NAVPATH_BENCH_SNAPSHOT:-$ROOT/graph.snapshot}"
TOL="${PERF_GATE_TOLERANCE:-0.10}"
CRIT="$ROOT/target/criterion"

cmd="check"
reuse=0
extra=()
if [ $# -gt 0 ]; then
  case "$1" in
    check|bless|install-hook) cmd="$1"; shift ;;
  esac
fi
while [ $# -gt 0 ]; do
  case "$1" in
    --reuse) reuse=1; shift ;;
    --) shift; extra=("$@"); break ;;
    *) extra+=("$1"); shift ;;
  esac
done

if [ "$cmd" = "install-hook" ]; then
  HOOK="$ROOT/.git/hooks/pre-push"
  if [ -e "$HOOK" ]; then
    echo "perf-gate: $HOOK already exists; refusing to overwrite. Add 'scripts/perf-gate.sh check' to it manually."
    exit 1
  fi
  cat > "$HOOK" <<'EOF'
#!/usr/bin/env bash
# Perf gate on push; skip with PERF_GATE_SKIP=1 git push ...
[ "${PERF_GATE_SKIP:-0}" = "1" ] && exit 0
exec "$(git rev-parse --show-toplevel)/scripts/perf-gate.sh" check
EOF
  chmod +x "$HOOK"
  echo "perf-gate: installed pre-push hook (skip with PERF_GATE_SKIP=1)"
  exit 0
fi

if [ ! -f "$SNAP" ]; then
  echo "perf-gate: no snapshot at $SNAP; skipping (exit 0)"
  exit 0
fi

HASH16="$(tail -c 32 "$SNAP" | od -An -v -tx1 | tr -d ' \n' | cut -c1-16)"
RUSTV="$(rustc --version | awk '{print $2}')"
KEY="${HASH16}_rustc-${RUSTV}"
BASE="$ROOT/docs/perf-baselines/$KEY"

if [ "$reuse" -eq 0 ]; then
  echo "perf-gate: running criterion corpus (baseline key: $KEY)"
  NAVPATH_BENCH_SNAPSHOT="$SNAP" cargo bench -p navpath-core --bench astar \
    --manifest-path "$ROOT/Cargo.toml" -- --save-baseline cand --noplot "${extra[@]}"
fi

export CRIT BASE TOL CMD="$cmd"
python3 - <<'PYEOF'
import json, os, pathlib, shutil, sys

crit = pathlib.Path(os.environ["CRIT"])
base = pathlib.Path(os.environ["BASE"])
tol = float(os.environ["TOL"])
cmd = os.environ["CMD"]

def median_of(path):
    with open(path) as f:
        return json.load(f)["median"]["point_estimate"]

# Per-group regression tolerance, measured 2026-07-14 (two full runs, identical
# binary). Longest matching prefix wins; PERF_GATE_TOLERANCE covers the rest.
GROUP_TOL = [
    ("astar_bidir/medium", 0.40),  # observed +34.3% on identical code
    ("astar_bidir/long",   0.35),  # observed +36% wander across identical-code runs
    ("astar_bidir/",       0.12),  # shorts stable (<= +6.1%)
    ("astar_seeded/",      0.30),  # observed +24.0% (bidir longs to +38.7%)
    ("astar_seeded/bidir_long", 0.40),
    ("astar_gated/",       0.30),  # budget-capped floods: layout-sensitive, drifted
                                   # +10-40% across allocation-pattern changes (Phase
                                   # B/E measurements); in-process A/B is the real gate
    ("astar/medium",       0.30),  # observed -13.5% (swings both ways)
    ("astar/long",         0.35),  # bimodal per-process: 1.56-2.80 ms on identical code
    ("astar_incident/",    0.45),  # observed 4.8-6.9 ms across identical-code runs
    ("provider_build/",    0.15),  # load-time, page-cache dependent
    ("heuristic/select_active", 0.50),  # ~250 ns bench: noise-scale + per-query operand build
]

def tol_for(bench_id):
    for prefix, t in sorted(GROUP_TOL, key=lambda x: -len(x[0])):
        if bench_id.startswith(prefix):
            return t
    return tol

# Collect candidate estimates: any .../cand/estimates.json under target/criterion.
cands = {}
if crit.is_dir():
    for p in crit.rglob("cand/estimates.json"):
        bench_id = str(p.parent.parent.relative_to(crit))  # e.g. astar/short/1235-1744
        cands[bench_id] = p
if not cands:
    print("perf-gate: no candidate results under target/criterion; run the bench first")
    sys.exit(1)

if cmd == "bless":
    if base.exists():
        shutil.rmtree(base)
    for bench_id, p in sorted(cands.items()):
        dst = base / (bench_id + ".json")
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(p, dst)
    print(f"perf-gate: blessed {len(cands)} baselines into {base}")
    sys.exit(0)

if not base.is_dir():
    print(f"perf-gate: no blessed baselines at {base}")
    print("perf-gate: run 'scripts/perf-gate.sh bless --reuse' to bless the current run")
    sys.exit(1)

fails, news, better = [], [], []
for bench_id, p in sorted(cands.items()):
    ref = base / (bench_id + ".json")
    if not ref.exists():
        news.append(bench_id)
        continue
    cand_m, ref_m = median_of(p), median_of(ref)
    delta = (cand_m - ref_m) / ref_m
    t = tol_for(bench_id)
    line = f"  {bench_id}: {ref_m/1e6:.3f} ms -> {cand_m/1e6:.3f} ms ({delta:+.1%}, tol {t:.0%})"
    if delta > t:
        fails.append(line)
    elif delta < -t:
        better.append(line)
    print(line)

missing = [b for b in (str(q.relative_to(base))[:-5] for q in base.rglob("*.json")) if b not in cands]
for b in sorted(missing):
    print(f"  WARN baseline {b} has no candidate result (bench filtered out or renamed?)")
for b in news:
    print(f"  NEW  {b} (no baseline yet; bless to start tracking)")
if better:
    print("perf-gate: improvements beyond tolerance (consider blessing):")
    for line in better:
        print(line)
if fails:
    print(f"perf-gate: FAIL — {len(fails)} regression(s) beyond {tol:.0%}:")
    for line in fails:
        print(line)
    sys.exit(1)
print(f"perf-gate: OK ({len(cands)} benches within their per-group tolerances)")
PYEOF
