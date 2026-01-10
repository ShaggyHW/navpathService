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
    "start": {"wx": 3296, "wy": 3184, "plane": 0},
    "goal":  {"wx": 3435, "wy": 3082, "plane": 0},
    "profile": {"requirements": [{"key":"coins","value":100},{"key":"hasDungCape","value":1},{"key":"varp_2102","value":15}]},
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
