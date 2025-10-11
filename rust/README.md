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

## API schemas

- **FindPathRequest** (`rust/navpath-service/src/routes.rs`):
  ```json
  {
    "start": [x, y, plane],
    "goal": [x, y, plane],
    "options": { /* optional; see SearchOptions */ },
    "db_path": "/optional/override.db"
  }
  ```
- **SearchOptions** (`rust/navpath-core/src/options.rs`) defaults:
  - `use_doors=true`
  - `use_lodestones=true`
  - `use_objects=true`
  - `use_ifslots=true`
  - `use_npcs=true`
  - `use_items=true`
  - `max_expansions=1000000`
  - `timeout_ms=5000`
  - `max_chain_depth=5000`
  - Cost overrides (if present): `door=600`, `lodestone=17000`, `object=2000`, `ifslot=1000`, `npc=1000`, `item=3000`
  - `extras` map (the service injects `start_tile=[sx,sy,sp]` automatically)

#### Requirements context (for requirement-gated nodes)

Provide player/context stats via `options.extras` so requirement checks can pass:

- Preferred: `extras.requirements_map` as an object map of key → integer value
  ```json
  {
    "options": {
      "extras": {
        "requirements_map": { "magic": 55, "agility": 60 }
      }
    }
  }
  ```
- Alternative: `extras.requirements` as an array of `{key, value}` pairs
  ```json
  {
    "options": {
      "extras": {
        "requirements": [
          { "key": "magic", "value": 55 },
          { "key": "agility", "value": 60 }
        ]
      }
    }
  }
  ```

Notes:
- Keys must match the `requirements.key` values in the DB. Comparisons are defined in the DB (`==`, `!=`, `>=`, `>`, `<=`, `<`).
- If a DB requirement has no key or value, it is treated as passed. If a key is missing from the provided context, that requirement fails.

- **PathResult** (`rust/navpath-core/src/models.rs`):
  ```json
  {
    "path": [[x,y,plane], ...] | null,
    "actions": [
      {
        "type": "move|door|lodestone|object|ifslot|npc|item",
        "from": { "min": [x,y,p], "max": [x2,y2,p2] },
        "to":   { "min": [x,y,p], "max": [x2,y2,p2] },
        "cost_ms": 0,
        "node": { "type": "door|...", "id": 123 } | null,
        "metadata": { /* omitted when empty */ }
      }
    ],
    "reason": "string or null",
    "expanded": 0,
    "cost_ms": 0
  }
  ```

### Example response
```json
{
  "path": [[3200,3200,0],[3201,3200,0]],
  "actions": [
    {
      "type": "move",
      "from": { "min": [3200,3200,0], "max": [3200,3200,0] },
      "to":   { "min": [3201,3200,0], "max": [3201,3200,0] },
      "cost_ms": 600
    }
  ],
  "reason": null,
  "expanded": 42,
  "cost_ms": 5
}
```

## Error responses

- **/readyz** with no `NAVPATH_DB` set: `503` body `{"ready": false, "error": "..."}`
- **/find_path** errors (e.g., missing DB): `500` body `{"error": "..."}`

## Provider and DB management

- **Default DB** comes from `NAVPATH_DB` (`rust/navpath-service/src/config.rs`).
- **Per-request override** via `db_path` in the request body.
- Providers are managed by `ProviderManager` (`rust/navpath-service/src/provider_manager.rs`): one long-lived `SqliteGraphProvider` per DB path with internal caches. Use `GET /readyz` once after startup to warm the default provider.

## Production build and run

```bash
cargo build -p navpath-service --release
./target/release/navpath-service
```

## Troubleshooting

- **No DB configured**: set `NAVPATH_DB` or pass `db_path` in the request. Error message will include: "no db_path provided and NAVPATH_DB not set".
- **Bind address in use**: change `NAVPATH_PORT` or stop the conflicting process.
- **Validate service is up**:
  ```bash
  curl -s http://127.0.0.1:8080/healthz
  curl -s http://127.0.0.1:8080/version | jq
  ```

