# Navpath (Rust Workspace)

This workspace contains the Rust implementation of Navpath:
- `navpath-core`: core library (models, options, DB layer, graph provider, A*)
- `navpath-service`: HTTP service (Axum) exposing `/find_path` and health endpoints

No Python is required to build or run the Rust components.

## Quickstart

- Build all crates:
  ```bash
  cargo build
  ```
- Run tests:
  ```bash
  cargo test
  ```
- Run the service:
  ```bash
  # Required: path to your SQLite DB
  export NAVPATH_DB=/absolute/path/to/worldReachableTiles.db
  
  # Optional: host/port and logging level
  export NAVPATH_HOST=0.0.0.0
  export NAVPATH_PORT=8080
  export RUST_LOG=info
  
  # Optional: SQLite read-only PRAGMA tuning (safe toggles)
  # See performance docs for guidance; values shown are examples.
  export NAVPATH_SQLITE_QUERY_ONLY=1
  export NAVPATH_SQLITE_CACHE_SIZE_KB=200000   # ~200MB page cache
  export NAVPATH_SQLITE_MMAP_SIZE=268435456    # 256MB mmap if supported
  export NAVPATH_SQLITE_TEMP_STORE=MEMORY      # or FILE
  
  cargo run -p navpath-service
  ```

The service binds to `${NAVPATH_HOST}:${NAVPATH_PORT}` (defaults `0.0.0.0:8080`).

## Endpoints

- `GET /healthz` — process liveness
- `GET /version` — service and core crate versions
- `GET /readyz` — warms and reports readiness for `NAVPATH_DB`
- `POST /find_path` — pathfinding request

### Example: Health and version
```bash
curl -s http://127.0.0.1:8080/healthz | jq
curl -s http://127.0.0.1:8080/version | jq
```

### Warmed-ready flow
To reuse warm caches (tile existence sets, chain-head index, touching-node LRU, etc.):
1) Set `NAVPATH_DB` to the default DB path before starting the service.
2) Start the service.
3) Call `GET /readyz` once to warm the default provider.

```bash
curl -s http://127.0.0.1:8080/readyz | jq
```

If `NAVPATH_DB` is not set, `/readyz` returns 503 and an error message.

### Example: Find path
Minimal request with defaults (all action edges enabled by default):
```bash
curl -s -X POST http://127.0.0.1:8080/find_path \
  -H 'Content-Type: application/json' \
  -d '{
    "start": [3200, 3200, 0],
    "goal": [3210, 3211, 0]
  }' | jq
```

Specify options (fields match `rust/navpath-core/src/options.rs` `SearchOptions`):
```bash
curl -s -X POST http://127.0.0.1:8080/find_path \
  -H 'Content-Type: application/json' \
  -d '{
    "start": [3200, 3200, 0],
    "goal": [3210, 3211, 0],
    "options": {
      "use_doors": true,
      "use_lodestones": true,
      "use_objects": true,
      "use_ifslots": true,
      "use_npcs": true,
      "use_items": true,
      "max_expansions": 1000000,
      "timeout_ms": 5000,
      "max_chain_depth": 5000,
      "door_cost_override": 600,
      "lodestone_cost_override": 17000,
      "object_cost_override": 2000,
      "ifslot_cost_override": 1000,
      "npc_cost_override": 1000,
      "item_cost_override": 3000,
      "extras": {}
    }
  }' | jq
```

Notes:
- The service automatically injects `options.extras["start_tile"]` as `[start_x, start_y, start_plane]` to match provider gating logic.
- You may override the default DB on a per-request basis by adding `"db_path": "/path/to/db"` to the request body.

## Configuration

- Binding:
  - `NAVPATH_HOST` (default `0.0.0.0`)
  - `NAVPATH_PORT` (default `8080`)
- Default DB:
  - `NAVPATH_DB=/absolute/path/to/worldReachableTiles.db`
- Logging:
  - `RUST_LOG=info` (e.g., `debug,navpath_service=trace` for verbose)
- SQLite PRAGMAs (read-only safe; applied if supported):
  - `NAVPATH_SQLITE_QUERY_ONLY=1|0`
  - `NAVPATH_SQLITE_CACHE_SIZE_KB=<KB>` (negative applied internally for KB units)
  - `NAVPATH_SQLITE_MMAP_SIZE=<bytes>`
  - `NAVPATH_SQLITE_TEMP_STORE=MEMORY|FILE`

These map to `DbOpenConfig` in `rust/navpath-core/src/db/open.rs` and are applied by `Database::open_read_only()`.

## Benchmark tips

- Use a single default DB and warm caches via `/readyz` before benchmarking.
- Suggested tooling:
  - [`hey`](https://github.com/rakyll/hey):
    ```bash
    cat > req.json <<'JSON'
    {"start":[3200,3200,0],"goal":[3210,3211,0]}
    JSON
    hey -n 200 -c 20 -m POST -T 'application/json' -D req.json http://127.0.0.1:8080/find_path
    ```
  - [`wrk`](https://github.com/wg/wrk):
    ```bash
    wrk -t4 -c64 -d30s -s scripts/find_path.lua http://127.0.0.1:8080
    # Provide a Lua script that posts req.json
    ```
- Tune environment for your host memory/IO. Start with:
  - `NAVPATH_SQLITE_CACHE_SIZE_KB=200000`
  - `NAVPATH_SQLITE_MMAP_SIZE=268435456`
- Monitor logs via `RUST_LOG=info` and consider `debug` to troubleshoot.

## Project layout

```
rust/
├── Cargo.toml
├── navpath-core/
│   ├── src/
│   └── Cargo.toml
└── navpath-service/
    ├── src/
    └── Cargo.toml
```
