#!/usr/bin/env bash
# Perf-regression gate over the criterion corpus (docs/optimization_roadmap_v2.md §9.5).
#
# Criterion's own baselines live under the disposable target/ dir; this gate keeps
# BLESSED median estimates in docs/perf-baselines/<snapshot-hash16>_<rustc>/ so every
# change lands against a durable, versioned reference.
#
# Usage:
#   scripts/perf-gate.sh check [--reuse] [-- <extra criterion args>]  # bench + compare (default)
#   scripts/perf-gate.sh bless [--reuse] [-- <extra args>]            # bench (unless --reuse) + save as baseline
#   scripts/perf-gate.sh prune                                        # drop baselines whose bench no longer exists
#   scripts/perf-gate.sh install-hook                                 # add a git pre-push hook
#
# Environment:
#   PERF_GATE_RUNS      bench processes per gate run (default 3). The dominant noise is
#                       BETWEEN processes (+-34-45% on bidir/seeded classes, per-process
#                       layout), so the gate takes the per-bench median across processes
#                       rather than one long run (efficiency audit T5.8). Keep it odd.
#   PERF_GATE_WARMUP    criterion --warm-up-time seconds per bench (default 1)
#   PERF_GATE_MEASURE   criterion --measurement-time seconds per bench (default 2)
#   PERF_GATE_NRESAMPLES criterion bootstrap resamples (default 10000, criterion's is
#                       100000): only the confidence intervals use them — the gated
#                       median is the plain sample median — so fewer is free time
#   PERF_GATE_FILTER    criterion filter regex (e.g. 'astar_bidir/short|astar_rr');
#                       don't also pass a positional filter after '--'.
#   PERF_GATE_SETARCH=1 run the bench under 'setarch -R' (ASLR off: identical layout in
#                       every process — less spread, but a layout-unlucky change then
#                       reads consistently slow or fast; off by default)
#   PERF_GATE_TOLERANCE default tolerance for benches outside the per-group table
#
# Notes:
#   - Baselines are keyed on (snapshot tail hash, rustc version): numbers are only
#     comparable for the same snapshot (landmark count changes the hash) and compiler
#     (pinned via rust-toolchain.toml).
#   - [profile.bench] is panic=unwind, so absolute numbers differ slightly from the
#     panic=abort release binary; the gate compares bench-to-bench, which is sound.
#   - Only THIS run's results are compared (T5.7): every process starts by deleting
#     target/criterion/**/cand, and collection also requires mtime >= the process start.
#     Per-process results are kept in target/perf-gate/runs/<i>/ for --reuse.
#   - Stale baselines (bench ids that no longer exist, per the bench binary's --list)
#     are reported by 'check' and deleted by 'bless' and 'prune'. 'bless' of a filtered
#     run updates only the benches that ran and keeps the rest.
#   - Tolerance: PERF_GATE_TOLERANCE (default 0.10) applies to benches not covered by
#     the per-group table below. The table was MEASURED on 2026-07-14 from two full
#     runs of an identical binary: most benches sit within +-4%, but bidir/seeded
#     medium-range searches swing up to +-34% run-to-run (systematic per-process —
#     plausibly cache-set aliasing between the two ~18 MB bidir context arrays; see
#     the roadmap's context-pool items 2.9/2.10, which should shrink this). Tighten
#     the table once cross-process medians have been measured against it.
#   - Filtered runs ('-- <filter>' or PERF_GATE_FILTER) compare a cold-cache process
#     against warm full-suite baselines and read up to ~20% slow on the ms-scale
#     benches even on identical code — treat them as indicative; gate on full runs.
#   - '--reuse' skips the bench and re-evaluates the last run's per-process results.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SNAP="${NAVPATH_BENCH_SNAPSHOT:-$ROOT/graph.snapshot}"
TOL="${PERF_GATE_TOLERANCE:-0.10}"
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
CRIT="${CRITERION_HOME:-$TARGET/criterion}"
GATE_DIR="$TARGET/perf-gate"
RUNS="${PERF_GATE_RUNS:-3}"
WARMUP="${PERF_GATE_WARMUP:-1}"
MEASURE="${PERF_GATE_MEASURE:-2}"
NRESAMPLES="${PERF_GATE_NRESAMPLES:-10000}"
FILTER="${PERF_GATE_FILTER:-}"

cmd="check"
reuse=0
extra=()
if [ $# -gt 0 ]; then
  case "$1" in
    check|bless|prune|install-hook) cmd="$1"; shift ;;
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

# The python half: collect one process's candidates, and gate/bless/prune.
pyhelper() {
  CRIT="$CRIT" BASE="$BASE" TOL="$TOL" GATE_DIR="$GATE_DIR" python3 - "$@" <<'PYEOF'
import json, os, pathlib, shutil, statistics, sys

crit = pathlib.Path(os.environ["CRIT"])
base = pathlib.Path(os.environ["BASE"])
gate = pathlib.Path(os.environ["GATE_DIR"])
tol = float(os.environ["TOL"])
action = sys.argv[1]

def median_of(path):
    with open(path) as f:
        return json.load(f)["median"]["point_estimate"]

def rel_id(path, root):
    return str(path.relative_to(root))[: -len(".json")]

def current_ids():
    p = gate / "ids.txt"
    if not p.is_file():
        return None
    return {l.strip() for l in p.read_text().splitlines() if l.strip()}

def collect_cands(root, since):
    """bench_id -> estimates.json of candidates written at/after `since` (epoch s).

    The mtime guard is belt-and-braces on top of deleting every cand/ dir before the
    process starts: a stale cand from an older run (a renamed/removed bench, or a
    bench the filter skipped) must never be compared as if it were current."""
    out, stale = {}, 0
    if root.is_dir():
        for p in root.rglob("cand/estimates.json"):
            if p.stat().st_mtime + 1e-3 < since:
                stale += 1
                continue
            out[str(p.parent.parent.relative_to(root))] = p  # e.g. astar/short/1235-1744
    return out, stale

if action == "collect":
    # collect <run_index> <since_epoch>: snapshot this process's candidates.
    run, since = sys.argv[2], float(sys.argv[3])
    cands, stale = collect_cands(crit, since)
    dst_root = gate / "runs" / run
    for bench_id, p in cands.items():
        dst = dst_root / (bench_id + ".json")
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(p, dst)
    msg = f"perf-gate: run {run}: collected {len(cands)} candidate(s)"
    if stale:
        msg += f", ignored {stale} stale cand(s) older than this run"
    print(msg)
    sys.exit(0 if cands else 1)

def prune(ids):
    """Delete blessed baselines whose bench id is not in the bench's current --list."""
    if ids is None or not base.is_dir():
        return []
    gone = []
    for q in sorted(base.rglob("*.json")):
        b = rel_id(q, base)
        if b not in ids:
            q.unlink()
            gone.append(b)
    for d in sorted((d for d in base.rglob("*") if d.is_dir()), key=lambda d: -len(d.parts)):
        if not any(d.iterdir()):
            d.rmdir()
    return gone

if action == "prune":
    gone = prune(current_ids())
    for b in gone:
        print(f"  pruned {b}")
    print(f"perf-gate: pruned {len(gone)} stale baseline(s) from {base}")
    sys.exit(0)

# --- gate: check | bless over the per-process runs ---
runs_dir = gate / "runs"
runs = sorted((d for d in runs_dir.iterdir() if d.is_dir()), key=lambda d: d.name) if runs_dir.is_dir() else []
per_bench = {}  # bench_id -> [(median_ns, path)]
for d in runs:
    for p in d.rglob("*.json"):
        per_bench.setdefault(rel_id(p, d), []).append((median_of(p), p))
if not per_bench:
    print("perf-gate: no results from a perf-gate run under", runs_dir)
    print("perf-gate: run the gate without --reuse first")
    sys.exit(1)

def pick(samples):
    """Cross-process median (median_low, so it is one real process's estimates)."""
    m = statistics.median_low([s[0] for s in samples])
    return m, next(p for v, p in samples if v == m)

cands = {b: pick(s) for b, s in per_bench.items()}
n_runs = len(runs)

# Per-group regression tolerance, measured 2026-07-14 (two full runs, identical
# binary). Longest matching prefix wins; PERF_GATE_TOLERANCE covers the rest.
GROUP_TOL = [
    ("astar_bidir/medium", 0.40),  # observed +34.3% on identical code
    ("astar_bidir/long",   0.35),  # observed +36% wander across identical-code runs
    ("astar_bidir/",       0.12),  # shorts stable (<= +6.1%)
    ("astar_seeded/",      0.30),  # observed +24.0% (bidir longs to +38.7%)
    ("astar_seeded/bidir_long", 0.40),
    ("astar_rr/",          0.30),  # round-robin over all corpus pairs (added 2026-09-25,
                                   # T5.11): not yet measured on identical code; set to
                                   # the bound of the uni/bidir/seeded classes it mixes
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

ids = current_ids()

if action == "bless":
    base.mkdir(parents=True, exist_ok=True)
    for bench_id, (m, p) in sorted(cands.items()):
        est = json.loads(p.read_text())
        est["perf_gate"] = {
            "runs": n_runs,
            "process_medians_ns": sorted(v for v, _ in per_bench[bench_id]),
        }
        dst = base / (bench_id + ".json")
        dst.parent.mkdir(parents=True, exist_ok=True)
        dst.write_text(json.dumps(est))
    gone = prune(ids)
    print(f"perf-gate: blessed {len(cands)} baselines (median of {n_runs} process(es)) into {base}")
    if gone:
        print(f"perf-gate: pruned {len(gone)} stale baseline(s) with no current bench")
    sys.exit(0)

if not base.is_dir():
    print(f"perf-gate: no blessed baselines at {base}")
    print("perf-gate: run 'scripts/perf-gate.sh bless --reuse' to bless the current run")
    sys.exit(1)

fails, news, better = [], [], []
for bench_id, (cand_m, _) in sorted(cands.items()):
    ref = base / (bench_id + ".json")
    if not ref.exists():
        news.append(bench_id)
        continue
    ref_m = median_of(ref)
    delta = (cand_m - ref_m) / ref_m
    t = tol_for(bench_id)
    spread = [v for v, _ in per_bench[bench_id]]
    sp = f", procs {min(spread)/1e6:.3f}-{max(spread)/1e6:.3f}" if len(spread) > 1 else ""
    line = f"  {bench_id}: {ref_m/1e6:.3f} ms -> {cand_m/1e6:.3f} ms ({delta:+.1%}, tol {t:.0%}{sp})"
    if delta > t:
        fails.append(line)
    elif delta < -t:
        better.append(line)
    print(line)

blessed = [rel_id(q, base) for q in base.rglob("*.json")]
not_run = [b for b in blessed if b not in cands]
stale = sorted(b for b in not_run if ids is not None and b not in ids)
skipped = [b for b in not_run if b not in stale]
for b in stale:
    print(f"  STALE baseline {b}: no such bench any more ('scripts/perf-gate.sh prune' deletes it)")
if skipped:
    print(f"  note: {len(skipped)} blessed bench(es) not in this run (filtered out)")
for b in news:
    print(f"  NEW  {b} (no baseline yet; bless to start tracking)")
if better:
    print("perf-gate: improvements beyond tolerance (consider blessing):")
    for line in better:
        print(line)
if fails:
    print(f"perf-gate: FAIL — {len(fails)} regression(s) beyond tolerance (median of {n_runs} process(es)):")
    for line in fails:
        print(line)
    sys.exit(1)
print(f"perf-gate: OK ({len(cands)} benches within their per-group tolerances, median of {n_runs} process(es))")
PYEOF
}

# Build the bench binary once and resolve its path, so every process runs the same
# executable directly (and optionally under setarch).
bench_exe() {
  cargo bench -p navpath-core --bench astar --no-run --message-format=json-render-diagnostics \
    --manifest-path "$ROOT/Cargo.toml" \
    | python3 -c '
import json, sys
exe = None
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("reason") == "compiler-artifact" and m.get("target", {}).get("name") == "astar" and m.get("executable"):
        exe = m["executable"]
if not exe:
    sys.exit("perf-gate: could not resolve the astar bench executable")
print(exe)'
}

# Record the bench binary's full (unfiltered) id list: the reference for stale baselines.
record_ids() {
  local exe="$1"
  mkdir -p "$GATE_DIR"
  NAVPATH_BENCH_SNAPSHOT="$SNAP" CRITERION_HOME="$CRIT" "$exe" --bench --list 2>/dev/null \
    | sed -n 's/: benchmark$//p' > "$GATE_DIR/ids.txt"
  [ -s "$GATE_DIR/ids.txt" ] || { echo "perf-gate: '$exe --list' returned no benches"; exit 1; }
}

if [ "$cmd" = "prune" ]; then
  EXE="$(bench_exe)"
  record_ids "$EXE"
  pyhelper prune
  exit 0
fi

if [ "$reuse" -eq 0 ]; then
  EXE="$(bench_exe)"
  record_ids "$EXE"
  echo "perf-gate: $(wc -l < "$GATE_DIR/ids.txt") benches; $RUNS process(es), warm-up ${WARMUP}s, measurement ${MEASURE}s (baseline key: $KEY)"
  prefix=()
  if [ "${PERF_GATE_SETARCH:-0}" = "1" ]; then
    prefix=(setarch "$(uname -m)" -R)
  fi
  args=(--bench --save-baseline cand --noplot --warm-up-time "$WARMUP" --measurement-time "$MEASURE"
        --nresamples "$NRESAMPLES" "${extra[@]}")
  if [ -n "$FILTER" ]; then
    args+=("$FILTER")
  fi
  rm -rf "$GATE_DIR/runs"
  for i in $(seq 1 "$RUNS"); do
    # T5.7: no candidate from an earlier process or run may survive into this one.
    if [ -d "$CRIT" ]; then
      find "$CRIT" -type d -name cand -prune -exec rm -rf {} +
    fi
    since="$(date +%s.%N)"
    t0=$SECONDS
    echo "perf-gate: process $i/$RUNS"
    NAVPATH_BENCH_SNAPSHOT="$SNAP" CRITERION_HOME="$CRIT" "${prefix[@]}" "$EXE" "${args[@]}"
    pyhelper collect "$i" "$since"
    echo "perf-gate: process $i/$RUNS took $((SECONDS - t0)) s"
  done
fi

pyhelper "$cmd"
