ENJOY

IF YOU NOTICE A COORDINATE NOT AVAILABLE OPEN AN ISSUE WITH THIS FORMAT

```
X: 3200
Y: 3200
Plane: 0

Where it is:
Near Lumbridge:

Screenshot of map area:
[Screenshot]

```

```sh
cargo run -p navpath-builder --release --   --sqlite ./worldReachableTiles.db   --out-snapshot ./graph.snapshot   --out-tiles ./tiles.bin  --landmarks 64

cargo run -p navpath-builder --release --   --sqlite /home/query/Dev/rs3cache_extractor/worldReachableTiles.db   --out-snapshot ./graph.snapshot   --out-tiles ./tiles.bin  --landmarks 64

UPDATE THE PATH TO YOUR ONW

export SNAPSHOT_PATH=/home/query/Dev/navpathService/graph.snapshot 
export NAVPATH_HOST=127.0.0.1
export NAVPATH_PORT=8080
export RUST_LOG=info
cargo run -p navpath-service --release
cargo run -p navpath-service --release -- --dump-result result.json --no-seed

```

## Validation & perf tooling

Run these before landing any engine, heuristic, or snapshot-format change
(details in `docs/optimization_roadmap_v2.md` §9):

```sh
# Golden replay comparator: uni + bidir + virtual-start x seeds [none,1,12345]
# against tools/golden_corpus.json (costs, path re-costing, admissibility, pops).
cargo run --release -p navpath-service --example replay
# ... after an INTENDED cost/pops change, re-bless the expectations:
cargo run --release -p navpath-service --example replay -- --regen

# Cross-snapshot invariance: costs must be identical across landmark counts (24/64)
# and NAVPATH_ACTIVE_LANDMARKS (4/8). Builds fresh snapshots from the tile DB.
tools/invariance_check.sh

# Payload invariants: the action list must faithfully describe the returned path
# (ability origins, ability reach, no skipped/replayed tiles). Needs a running service.
python3 tools/verify_actions.py 8080

# Perf gate: criterion corpus vs blessed medians in docs/perf-baselines/
# (keyed on snapshot hash + rustc). Fails on >10% median regression.
scripts/perf-gate.sh check          # bench + compare
scripts/perf-gate.sh bless          # bench + save as the new baseline
scripts/perf-gate.sh bless --reuse  # re-bless the last run without re-benching
scripts/perf-gate.sh install-hook   # optional git pre-push hook (PERF_GATE_SKIP=1 to skip)
```

The DB producer must ship the `tiles_regions` table (run `migrate_tiles_regions.py`
after any tiles change) — the builder falls back to a ~10x slower row-per-tile scan
and warns loudly when it is missing.

## API Endpoints

### Check if a tile exists

Check whether a tile exists (is walkable) at the given coordinates.

```sh
curl -s "http://127.0.0.1:8080/tile/exists?x=2994&y=3280&plane=0"
```

Response if tile exists:
```json
{"exists": true, "node_id": 12345}
```

Response if tile doesn't exist:
```json
{"exists": false}
```

### Check walk-only reachability

True iff the goal is within a 20-tile range (Chebyshev) of the start AND a pure
walk path connects them — no macro edges (doors, stairs, teleports) — without
leaving the endpoints' 20-tile neighbourhood. Answered from the snapshot's
walk-component ids plus a bounded BFS (microseconds; no search permit).

```sh
curl -s "http://127.0.0.1:8080/reachable?sx=3259&sy=3101&splane=0&gx=3262&gy=3105&gplane=0"
```

Response:
```json
{"reachable": true}
```

When false, `reason` says why: `out_of_range` (further than 20 tiles),
`different_plane`, `start_tile_not_found` / `goal_tile_not_found` (coordinate is
not a walkable tile), `not_connected` (no walk-only path exists at all — e.g.
the goal is behind a closed door or fence), or `no_path_in_range` (a walk path
exists but every one detours outside the 20-tile neighbourhood — e.g. the far
bank of a river whose bridge is 50 tiles away).

### Calculate a route

```sh
curl -s http://127.0.0.1:8080/route \
  -H 'content-type: application/json' \
  -d '{
    "start": {"wx": 3259, "wy": 3101, "plane": 0},
    "goal":  {"wx": 3425, "wy": 3017, "plane": 0},
    "profile": {"requirements": [{"key":"coins","value":100},{"key":"hasDungCape","value":1},{"key":"varp_2102","value":15},{"key":"varbit_9928","value":180}]},
    "options": {"return_geometry": false, "only_actions": true},
    "surge": {
    "enabled": true,
    "charges": 2,
    "cooldown_ms": 20400
  },
  "dive": {
    "enabled": true,
    "cooldown_ms": 20400
  }
  }'
```

### Path Randomization

You can add a `seed` parameter to get different paths for the same start/goal. Same seed = same path. Different seeds = different paths (when alternatives exist).

```sh
curl -s http://127.0.0.1:8080/route \
  -H 'content-type: application/json' \
  -d '{
    "start": {"wx": 3296, "wy": 3184, "plane": 0},
    "goal":  {"wx": 3435, "wy": 3082, "plane": 0},
    "seed": 12345
  }'
```

If no seed is provided, the same optimal path is always returned. With a seed, small random jitter is added to edge weights to explore alternative routes.

**A seed costs you the search, and (under the legacy cache policy) the cache.** Seeded
searches expand ~1.5-3x more heap pops than unseeded ones (jitter breaks the exact
f-value ties the engine relies on, and it disables canonical pruning outright), for a
path that differs only in which of several *equal-cost* routes is chosen. Since
2026-08-06 the cache is **seed-blind by default** (`NAVPATH_CACHE_IGNORE_SEED=1`):
repeat traffic with varying seeds is served the cached path, so only the *first*
request for a pair pays the seeded search. Set `NAVPATH_CACHE_IGNORE_SEED=0` to
restore per-seed cache keys (per-seed tie variety on repeats, at ~100x the repeat
latency). If you don't need variety at all, drop the seed — unseeded searches are also
the only ones canonical pruning accelerates.

**Server-side kill switch:** start the service with `--no-seed` (or
`NAVPATH_IGNORE_SEED=1`) to ignore client seeds entirely. Every request is answered
with the deterministic unseeded optimum — no edge jitter, canonical pruning engages,
the seeded retry rungs never run — and responses to requests that did send a seed
carry `degraded: "seed_ignored"`. `/stats` reports `seeding_disabled: true`.

```sh
cargo run -p navpath-service --release -- --no-seed
```

## Route cache

Results are cached per snapshot in an LRU keyed on
`(start, goal, exact eligibility bits, hasQuickTele, seed)`. A hit skips the search
entirely (sub-millisecond responses); the actions/geometry payload is still rebuilt per
request, so one entry serves every `options`/`surge`/`dive` combination. The cache is
dropped whenever the snapshot is swapped (`/admin/reload`).

### Diagnosing a low hit rate

Every `/route` log line ends with a `cache=` field, and `/stats` breaks the misses down:

```sh
curl -s http://127.0.0.1:8080/stats | jq '{cache_hits, cache_miss_seed, cache_miss_cold, route_cache}'
```

| `cache=` | meaning | what to do |
|---|---|---|
| `hit` | served from cache, no search ran | — |
| `miss_seed` | same start/goal/profile is cached, only the **seed** differed (only possible with `NAVPATH_CACHE_IGNORE_SEED=0`) | leave the default seed-blind policy on, or stop sending seeds |
| `miss_cold` | this start/goal/profile pair isn't cached | irreducible — no cache policy helps |
| `off` | `NAVPATH_ROUTE_CACHE=0` | re-enable the cache |

`cache_miss_seed` is exactly how many requests `NAVPATH_CACHE_IGNORE_SEED=1` would turn
into hits. Measured on a repeating pair with random seeds: 11 of 12 requests hit and
latency dropped from ~118 ms to ~0.3-0.9 ms, at the cost of every seed being served the
same path.

## Environment variables

| Variable | Default | Effect |
|---|---|---|
| `SNAPSHOT_PATH` | `./graph.snapshot` | Snapshot to serve. |
| `NAVPATH_HOST` / `NAVPATH_PORT` | `127.0.0.1` / `8080` | Listen address. |
| `NAVPATH_ROUTE_CACHE` | `2048` | Route-cache entries; `0` disables it. |
| `NAVPATH_CACHE_IGNORE_SEED` | `1` | Seed-blind cache keys (default since 2026-08-06): any seed is served the cached path — recovers the hit rate for varying-seed traffic. `0` restores per-seed keys (per-seed tie variety on repeats; the paths only ever differed in equal-cost tie selection). |
| `NAVPATH_IGNORE_SEED` | `0` | `1` (or the `--no-seed` flag) ignores client seeds entirely: all searches run unseeded (no jitter, canonical pruning engages); seeded requests are answered with `degraded: "seed_ignored"`. |
| `NAVPATH_ROUTE_TIMEOUT_MS` | `10000` | Per-request wall-clock deadline (`0` = effectively none); a breach returns 504. |
| `NAVPATH_MAX_CONCURRENT_SEARCHES` | CPU count | Concurrent searches; excess requests get 503 rather than queueing. |
| `NAVPATH_MAX_POPS` | `max(1.5M, nodes/2)` | First-attempt pop budget (`0` = unbounded). |
| `NAVPATH_RETRY_MAX_POPS` | `4x` the above | Budget for the retry rung (`0` disables the retry). |
| `NAVPATH_BIDIR` | `1` | `0` forces the unidirectional engine. |
| `NAVPATH_BIDIR_MIN_HB_RATIO` | `0.5` | Backward-bound strength below which a route is demoted to unidirectional; `0` always runs bidirectional. Measured on the current snapshot: `0` wins on some long/seeded routes (up to 2.6x fewer pops) and loses badly on others (`quick_tele_route` 1.8x worse) — the default is the better compromise. |
| `NAVPATH_TIEBREAK_BUCKET_MS` | `0` (off) | Bucketed f-comparison for seeded searches. **Measured harmful on the current snapshot** (2-20x more pops on both engines); leave off. |
| `NAVPATH_CANONICAL` | `1` | `0` disables canonical successor pruning (unseeded searches only). |
| `NAVPATH_ACTIVE_LANDMARKS` | all | Landmarks evaluated per heuristic call; for A/B runs only. |
| `NAVPATH_DUMP_RESULT` | unset | Path to overwrite with each `/route` response as pretty JSON (also `--dump-result <path>`). |
| `NAVPATH_DEBUG_REQS` | `0` | Per-request requirement-matching diagnostics. |

You can check the rest of the keys in the worldReachableTiles.db TeleportRequirement Tables. if more keys are passed more connections will become available.


IF YOU WANT TO BUILD YOUR OWN DATABASE, I GOT MINE BY USING THIS REPO:
https://github.com/ShaggyHW/rs3cache_extractor
