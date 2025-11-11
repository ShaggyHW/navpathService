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

How to do a request, it will save to a file aswell.

```sh
curl -s http://127.0.0.1:8080/route \
  -H 'content-type: application/json' \
  -d '{
    "start": {"wx": 3225, "wy": 3902, "plane": 0},
    "goal":  {"wx": 2885, "wy": 2951, "plane": 0},
    "profile": {"requirements": [{"key":"coins","value":100}]},
    "options": {"return_geometry": false, "only_actions": true}
  }'
```

You can check the rest of the keys in the worldReachableTiles.db TeleportRequirement Tables. if more keys are passed more connections will become available.


IF YOU WANT TO BUILD YOUR OWN DATABASE, I GOT MINE BY USING THIS REPO:
https://github.com/ShaggyHW/rs3cache_extractor
