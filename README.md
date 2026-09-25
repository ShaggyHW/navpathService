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

Snapshot format v9 (2026-09): node ids follow a plane-major Morton (Z-order) coordinate
key instead of raster (plane, y, x) order, and the ALT (landmark distance) table is
stored packed by default (per-16-node-cluster u16 bases + u8 offsets, with an exact
exception list): ~220 MB instead of ~345 MB. The service refuses older snapshots with
an "unsupported version" error, so **rebuild `graph.snapshot` after upgrading** (the
build is deterministic and takes ~10 s). Builder flags:

- `--landmarks N` (rounded up to a multiple of 16, required by the AVX-512 heuristic)
- `--landmark-strategy scc|legacy` (default `scc`: landmarks placed per strongly
  connected component with a symmetric farthest-point score, so every landmark is
  usable in both directions for its region; `legacy` reproduces the pre-2026-09 table)
- `--local-fill true|false` (default `true`: components no landmark reaches get local
  landmarks written into existing columns — no extra bytes)
- `--alt-format packed|u16` (default `packed`: 35% smaller, ~2x faster on a cold page
  cache, within noise warm; `u16` keeps the plain table)

`tiles.bin` holds one walk-flag byte per node in node-id order (Morton order since v9).

Every build also writes `walkableTiles.bin` next to the snapshot (`--out-walkable PATH`
to move it, `--no-walkable` to skip): a ~400 KB coordinate-keyed presence bitmap of the
same tile set `/tile/exists` answers from — `"WTIL"`, version u8, chunk count u32 LE, then
per populated 64x64 region `plane u8, rx u16, ry u16, bitmap[512]` (bit `(y%64)*64 + x%64`,
LSB first). Copy it into Hoor2 (`launcher/payload/resources/walkableTiles.bin`) so
`Area.getRandomWalkableTile()` can answer offline without hitting the service.

export SNAPSHOT_PATH=/home/query/Dev/navpathService/graph.snapshot
export NAVPATH_HOST=127.0.0.1
export NAVPATH_PORT=8080
export RUST_LOG=info

# Recommended for latency (docs/route_latency_improvements_2026-09-17.md):
export NAVPATH_JPS=1             # jump-point expansion for the unidirectional engine (1.4-3x on walk-dominated routes)
export NAVPATH_RACE=1            # hedged JPS-uni/bidir race on the cache misses the gate picks (3-7x on teleport-heavy routes)
# export NAVPATH_MLOCK=1         # also mlock the mapping so memory pressure cannot evict it (needs RLIMIT_MEMLOCK >= snapshot size;
#                                # falls back to locking the ~45 MB per-pop head; a keep-warm loop re-populates the rest)
# export NAVPATH_CTX_PREWARM=64  # raise the startup context pre-warm on a dedicated box (default 16 contexts, ~19 MB each)

cargo run -p navpath-service --release
# --no-seed: ignore client seeds (no jitter, no seeded retry ladder; same optimal path).
# --dump-result: rewrites result.json on every request (debugging only; a sync file write per response).
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

# Cross-snapshot invariance: costs must be identical across landmark counts (32/64)
# and NAVPATH_ACTIVE_LANDMARKS (4/8). Builds fresh snapshots from the tile DB.
tools/invariance_check.sh

# Race tuning: per-pair uni-vs-bidir wall times through the production adapter, the race
# hint per pair, and simulated hedge policies (latency percentiles + CPU). The harnesses
# page the snapshot in first; NAVPATH_HARNESS_COLD=1 measures a cold page cache instead.
NAVPATH_JPS=1 cargo run --release -p navpath-service --example race_sweep -- 400 [--seeded] [--profile=none]

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

Engine regression oracle (in-process): records status / cost bits / path / pops and
best-of-3 wall time for 600 fixed coordinate pairs x 9 engine configurations, then
compares two recordings (strict: bit-identical results; `--loose`: same found/cost —
for heuristic or numbering changes). `--pack-alt` evaluates an in-memory packed table,
`--cold` pages the ALT table out before each timed run.

```sh
cargo run --release -p navpath-core --example engine_oracle -- record base.tsv
cargo run --release -p navpath-core --example engine_oracle -- record new.tsv
cargo run --release -p navpath-core --example engine_oracle -- compare base.tsv new.tsv [--loose]
```

End-to-end HTTP comparison of two builds (startup, cache-off latency per route class,
cache hits, 16-client concurrency, RSS; also checks both return the same costs):

```sh
python3 tools/latency_compare.py --base-bin OLD/navpath-service --base-snapshot old.snapshot \
    --new-bin target/release/navpath-service --new-snapshot graph.snapshot --out docs/latency_comparison.md
```

The DB producer must ship the `tiles_regions` table (run `migrate_tiles_regions.py`
after any tiles change) — the builder falls back to a ~10x slower row-per-tile scan
and warns loudly when it is missing.

## Startup warm-up and readiness

The listener binds immediately, but routes are served only after the warm-up thread
has paged the snapshot in (`NAVPATH_MMAP_POPULATE`) and pre-warmed the search context
pool (`NAVPATH_CTX_PREWARM`), a few seconds on a warm page cache. Until then `/health`
answers **503** with `"ready": false` and `/route` answers 503 `warming up`; `/stats`
carries `ready` too. Orchestrators should gate traffic on `/health` returning 200.

Every `/route` response carries `duration_us` next to the integer `duration_ms`, and
each route log line carries `duration_us`, `search_us`, `payload_us` and `ns_per_pop`
(≈150-230 warm; tens of thousands means the search hit a cold page cache). `/stats`
adds `search_us_log2` and `ns_per_pop_log2` histograms.

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
dropped whenever the snapshot is swapped (`/admin/reload`). A reload whose snapshot file
has the same tail hash as the one being served is a no-op that keeps the warm caches
(`{"reloaded": false, "unchanged": true}`); `POST /admin/reload?force=1` rebuilds anyway.

### Sub-path reuse (re-plans)

Bots re-request the same goal as they walk, and every such request misses the exact
key. A second, per-profile index keeps the newest `NAVPATH_SUBPATH_CACHE` optimal paths
with a node→position map; if both the requested start and goal lie on one of them (in
order), the slice is served with the exact cost `path_g[goal] - path_g[start]`. A
sub-path of a shortest path is a shortest path, and the graph is identical for the
same profile, so this is exact — including origin-only global teleports (the cached
optimum already proved walking on beats teleporting from any node on it). Suffix
(re-plan), prefix (stop early) and interior slices all qualify; virtual starts do not.
The route log reports `cache=subpath`, `/stats` counts `cache_subpath_hits` and
`cache_miss_goal_known` (misses whose goal was on a cached path but whose start was
not — the size of the near-start opportunity).

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
| `subpath` | both endpoints lie on a cached optimal path for the profile; the slice was served without a search | — |
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
| `NAVPATH_SUBPATH_CACHE` | `64` | Paths kept per profile for exact sub-path reuse: a request whose start and goal both lie on a cached optimal path (same eligibility bits and quick-tele flag) is served the slice in microseconds, no search. `0` disables. |
| `NAVPATH_CACHE_IGNORE_SEED` | `1` | Seed-blind cache keys (default since 2026-08-06): any seed is served the cached path — recovers the hit rate for varying-seed traffic. `0` restores per-seed keys (per-seed tie variety on repeats; the paths only ever differed in equal-cost tie selection). |
| `NAVPATH_IGNORE_SEED` | `0` | `1` (or the `--no-seed` flag) ignores client seeds entirely: all searches run unseeded (no jitter, canonical pruning engages); seeded requests are answered with `degraded: "seed_ignored"`. |
| `NAVPATH_ROUTE_TIMEOUT_MS` | `10000` | Per-request wall-clock deadline (`0` = effectively none); a breach returns 504. |
| `NAVPATH_MAX_CONCURRENT_SEARCHES` | CPU count | Concurrent searches; excess requests get 503 rather than queueing. A race hedge counts as a search but only takes a permit while more than a quarter of them stays free. |
| `NAVPATH_WORKER_THREADS` | min(4, cores) | Tokio reactor threads (request parsing, cache hits, response writing); searches run on the blocking pool. |
| `NAVPATH_MAX_BLOCKING_THREADS` | max(2 x permits, 8) | Tokio blocking-pool cap (searches, race arms, large payload builds). |
| `NAVPATH_BLOCKING_KEEP_ALIVE_S` | `300` | Idle blocking threads are kept this long (tokio's 10 s default made low-QPS misses start on a fresh thread). |
| `RUST_LOG` | `info` | Log filter (`tracing` `EnvFilter` syntax, e.g. `warn` drops the per-request lines). Lines are written by a background thread, never on a request thread. |
| `NAVPATH_MMAP_POPULATE` | `1` | Page the whole snapshot in during the startup warm-up (and before every `/admin/reload` swap), with parallel `MADV_POPULATE_READ`. `0` disables it (and the keep-warm loop); a cold page then costs 50-90 µs per pop on first touch. |
| `NAVPATH_MLOCK` | `0` | `1` also `mlock`s the mapping (~220 MB) so the page cache cannot evict it; needs `RLIMIT_MEMLOCK` at least that large (systemd `LimitMEMLOCK=` or `CAP_IPC_LOCK`). If the whole mapping cannot be locked it locks the ~45 MB of per-pop sections (coords, walk CSR, components, metadata) instead, and warns. |
| `NAVPATH_KEEP_WARM_S` | `60` | Interval of the background keep-warm loop, which re-populates whatever is not locked (`MADV_POPULATE_READ`: page-table walks when resident, disk reads for evicted pages — off the request path). Measured on a swapping host: 18 h after startup only 58 MB of the mapping was still resident, and a query whose ALT rows are cold costs ~435 ms instead of ~1 ms. `0` disables. |
| `NAVPATH_CTX_PREWARM` | min(2 x permits, 16) (3 x permits with `NAVPATH_RACE=1`, same cap) | Search **contexts** allocated and paged in at startup (~19 MB each at 1.1M nodes; a bidirectional search uses two, a JPS/unidirectional or virtual-start search one). Counted in context pairs before 2026-09-25 — halve an old value's memory meaning, or double the number to keep it. Removes the 100-400 ms first-touch stall a request otherwise pays for a fresh context; `0` disables. `/stats` `ctx_pool.fresh_allocations` counts the contexts requests had to allocate themselves; raise it if that keeps growing. On a host under memory pressure a large idle pre-warm gets swapped out again and the first requests pay to fault it back in. |
| `NAVPATH_MAX_POPS` | `max(1.5M, nodes/2)` | First-attempt pop budget (`0` = unbounded). |
| `NAVPATH_RETRY_MAX_POPS` | `4x` the above | Budget for the retry rung (`0` disables the retry). The same-seed retry CONTINUES the stopped search instead of re-running it (bit-identical to a fresh run with the larger budget), so a route that needs 1.6M pops no longer pays 3.1M; the response `pops` is the cumulative count. |
| `NAVPATH_BIDIR` | `1` | `0` forces the unidirectional engine. |
| `NAVPATH_JPS` | `0` | `1` enables jump-point expansion (JPS+ with precomputed jump tables, +54 MB, +100 ms at load) in the unidirectional engine for unseeded searches: straight runs on the uniform-cost walk grid are jumped instead of expanded node by node, 5-26x fewer expansions, cost-exact (a sub-path of a shortest path is a shortest path; jumps stop at the goal, at any node with a door/teleport/fairy edge, and at forced turns). Bidirectional searches keep plain expansion, so with the race on the JPS racer is the one that wins walk-dominated routes. Log lines report `engine=jps`. Ties among equal-cost paths resolve diagonal-first, so served paths can differ from plain expansion at identical cost. |
| `NAVPATH_RACE` | `0` | `1` runs the hedged engine race on cache misses: a primary engine starts at once and the other engine (the hedge) runs alongside it when the gate below judges the route worth it; the first stable result (found / genuine not-found) is served and the loser is cancelled. Same exact cost either way. The hedge needs a spare search permit: it is only granted while more than a quarter of `NAVPATH_MAX_CONCURRENT_SEARCHES` stays free, so hedges never push primaries into 503s; otherwise the primary runs alone. `/stats` reports `race_runs` (hedges started), `race_wins_uni`, `race_wins_bidir`, `race_gated`, `race_hedge_skipped`, `race_hedge_denied` and the `race` config; every route log line carries `engine=uni|jps|bidir|cache`. Note: the two engines break equal-cost ties differently, so with the race on the served path among several **equal-cost** alternatives depends on which engine finished first (cost is identical either way; `tools/payload_baseline.json` is captured with the race off). |
| `NAVPATH_RACE_PRIMARY` | `auto` | Engine a race starts first (and runs alone when there is no hedge): `auto` = JPS/uni when jump-point expansion applies (`NAVPATH_JPS=1`, unseeded), else bidir; `uni` / `bidir` force one. Measured with `examples/race_sweep`: JPS beats bidir on ~96% of random pairs; without JPS bidir is the better single engine. |
| `NAVPATH_RACE_GATE` | `1` | Hedge only routes the second engine can plausibly win: with a bidir primary, teleport-dominated routes or ones with `h(start)` >= 20 s; with a JPS primary, heuristic-blind routes (`h = 0`, goal outside landmark coverage). Measured: latency sum within 0-2% of racing every miss, identical p99/max, 15-49% less race CPU; with JPS the hedge runs on ~3% of misses. Over HTTP (gated profile, interleaved A/B): same latency, -48% CPU. `0` hedges every miss. |
| `NAVPATH_RACE_HEDGE_MS` | `0` | Start the hedge only if the primary is still running after this many ms (tokio timer, 1 ms granularity). `0` starts it together with the primary. A delay trades latency for CPU only where the gate lets a route race; measured at 1 ms: -6..-11% CPU for +0..11% latency sum. |
| `NAVPATH_BIDIR_MIN_HB_RATIO` | `0` | Backward-bound strength below which a route is demoted to unidirectional; `0` (default since 2026-09-17) always runs bidirectional. Measured over 300 random pairs: always-bidir is 1.3-3x faster on long walk routes and within 5% of per-pair best overall, but 3-5x slower on teleport-dominated pairs (`lum_to_falador`, virtual starts) — see `docs/route_latency_improvements_2026-09-17.md`. `0.5` restores the old demotion policy. |
| `NAVPATH_TIEBREAK_BUCKET_MS` | `0` (off) | Bucketed f-comparison for seeded searches. **Measured harmful on the current snapshot** (2-20x more pops on both engines); leave off. |
| `NAVPATH_CANONICAL` | `1` | `0` disables canonical successor pruning (unseeded searches only). |
| `NAVPATH_ACTIVE_LANDMARKS` | all | Landmarks evaluated per heuristic call; for A/B runs only. |
| `NAVPATH_ALT_ONE_SIDED` | `1` | Use each landmark as far as the goal's entries allow (forward term, backward term and the two unreachability rules independently) instead of requiring both goal entries exact. Measured: 16-34% fewer pops, 1.35-1.9x faster on 600 random pairs, costs bit-identical. `0` restores the old rule for A/B runs. |
| `NAVPATH_PREFETCH_ROWS` | `1` | Prefetch the landmark rows of the successors a forward expansion keeps (canonical / jump-point branches). Measured +10-17% on unidirectional and virtual-start searches, +3-8% bidirectional. `0` disables. |
| `NAVPATH_REV_ANCHOR_FILTER` | `0` | `1` drops backward-bound anchors (the origin and global-teleport landings) that provably cannot reach the goal before aggregating the backward heuristic. It restores backward landmark columns (with all globals eligible, one-way-pocket landings otherwise leave h_b = 0) and cuts bidirectional pops ~9%, but measured 15-30% slower wall time because a blind backward half never gathers landmark rows; kept as an opt-in. |
| `NAVPATH_H_SIMD` | `1` | `0` disables the explicit AVX-512 heuristic kernels (portable/scalar paths return identical values). |
| `NAVPATH_ALT_HEAP` | — | Retired (ignored with a warning): the file-backed ALT range is already huge-page mapped, so the anonymous copy only doubled its memory. |
| `NAVPATH_DUMP_RESULT` | unset | Path to overwrite with each `/route` response as pretty JSON (also `--dump-result <path>`). |
| `NAVPATH_DEBUG_REQS` | `0` | Per-request requirement-matching diagnostics. |

You can check the rest of the keys in the worldReachableTiles.db TeleportRequirement Tables. if more keys are passed more connections will become available.


IF YOU WANT TO BUILD YOUR OWN DATABASE, I GOT MINE BY USING THIS REPO:
https://github.com/ShaggyHW/rs3cache_extractor
