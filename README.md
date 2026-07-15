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

UPDATE THE PATH TO YOUR ONW

export SNAPSHOT_PATH=/home/query/Dev/navpathService/graph.snapshot 
export NAVPATH_HOST=127.0.0.1
export NAVPATH_PORT=8080
export RUST_LOG=info
cargo run -p navpath-service --release
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

You can check the rest of the keys in the worldReachableTiles.db TeleportRequirement Tables. if more keys are passed more connections will become available.


IF YOU WANT TO BUILD YOUR OWN DATABASE, I GOT MINE BY USING THIS REPO:
https://github.com/ShaggyHW/rs3cache_extractor
