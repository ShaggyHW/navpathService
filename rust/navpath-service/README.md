# navpath-service (Axum HTTP Service)

`navpath-service` exposes Navpath functionality over HTTP using Axum. It reuses a long-lived `SqliteGraphProvider` per database path to leverage warm caches for stable, low-latency responses.

No Python is required to run this service.

## Quickstart

```bash
# From the workspace root `rust/`
# 1) Build
cargo build -p navpath-service

# 2) Configure (required): path to your SQLite DB
export NAVPATH_DB=/absolute/path/to/worldReachableTiles.db

# Optional environment
export NAVPATH_HOST=0.0.0.0
export NAVPATH_PORT=8080
export RUST_LOG=info

# Optional SQLite read-only PRAGMA tuning (applied if supported)
export NAVPATH_SQLITE_QUERY_ONLY=1
export NAVPATH_SQLITE_CACHE_SIZE_KB=200000
export NAVPATH_SQLITE_MMAP_SIZE=268435456
export NAVPATH_SQLITE_TEMP_STORE=MEMORY   # or FILE

# 3) Run
cargo run -p navpath-service
```

The service binds to `${NAVPATH_HOST}:${NAVPATH_PORT}` (default `0.0.0.0:8080`).

## Endpoints

- `GET /healthz` — liveness
- `GET /version` — service and core crate versions
- `GET /readyz` — warms and reports readiness for the default DB (`NAVPATH_DB`)
- `POST /find_path` — run pathfinding

### Warmed-ready flow
To maximize performance, warm the default provider caches before traffic:
1) Start with `NAVPATH_DB` set.
2) Call `GET /readyz` once.

```bash
curl -s http://127.0.0.1:8080/readyz | jq
```

If `NAVPATH_DB` is not set, `/readyz` returns 503 with an error message.

### Request/Response: /find_path
Request shape matches `FindPathRequest` in `rust/navpath-service/src/routes.rs`:
```jsonc
{
  "start": [x, y, plane],
  "goal": [x, y, plane],
  "options": { /* optional: mirrors SearchOptions in navpath-core */ },
  "db_path": "/optional/override.db"
}
```

- `options` fields correspond to `SearchOptions` in `rust/navpath-core/src/options.rs`.
- If `db_path` is omitted, the default `NAVPATH_DB` is used.
- The service injects `options.extras["start_tile"] = [start_x, start_y, start_plane]` automatically for provider gating.
- Actions-only variant is supported when either body or query indicates it:
  - Body: `options.extras["only_actions"]` or `options.extras["actions_only"]` is truthy
  - Query: `?only_actions=true` (alias: `?actions_only=true`)

Examples:
```bash
# Minimal
curl -s -X POST http://127.0.0.1:8080/find_path \
  -H 'Content-Type: application/json' \
  -d '{"start":[3200,3200,0],"goal":[3210,3211,0]}' | jq

# With options and per-request db override
curl -s -X POST http://127.0.0.1:8080/find_path \
  -H 'Content-Type: application/json' \
  -d '{
    "start": [3200, 3200, 0],
    "goal": [3210, 3211, 0],
    "db_path": "/absolute/other.db",
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

### Actions-only responses

If you only need the `actions` array, request the actions-only variant via query or body extras.

```bash
# Query flag (preferred)
curl -s -X POST 'http://127.0.0.1:8080/find_path?only_actions=true' \
  -H 'Content-Type: application/json' \
  -d '{"start":[3200,3200,0],"goal":[3210,3211,0]}' | jq

# Query alias
curl -s -X POST 'http://127.0.0.1:8080/find_path?actions_only=true' \
  -H 'Content-Type: application/json' \
  -d '{"start":[3200,3200,0],"goal":[3210,3211,0]}' | jq

# Body extras (boolean)
curl -s -X POST http://127.0.0.1:8080/find_path \
  -H 'Content-Type: application/json' \
  -d '{
    "start": [3200, 3200, 0],
    "goal": [3210, 3211, 0],
    "options": { "extras": { "only_actions": true } }
  }' | jq

# Body extras alias and coercion ("1" is accepted)
curl -s -X POST http://127.0.0.1:8080/find_path \
  -H 'Content-Type: application/json' \
  -d '{
    "start": [3200, 3200, 0],
    "goal": [3210, 3211, 0],
    "options": { "extras": { "actions_only": "1" } }
  }' | jq
```

## Configuration

- Binding
  - `NAVPATH_HOST` (default `0.0.0.0`)
  - `NAVPATH_PORT` (default `8080`)
- Default DB
  - `NAVPATH_DB=/absolute/path/to/worldReachableTiles.db`
- Logging
  - `RUST_LOG=info` (e.g., `debug` for verbose tracing JSON logs)
- SQLite read-only PRAGMAs (applied if supported):
  - `NAVPATH_SQLITE_QUERY_ONLY=1|0`
  - `NAVPATH_SQLITE_CACHE_SIZE_KB=<KB>`
  - `NAVPATH_SQLITE_MMAP_SIZE=<bytes>`
  - `NAVPATH_SQLITE_TEMP_STORE=MEMORY|FILE`

These map to `DbOpenConfig` in `navpath-core` and are applied by `Database::open_read_only()` used in `ProviderManager`.

## Benchmark tips

- Warm with `/readyz` before measuring.
- Use a single DB for consistent cache behavior.
- Suggested tools:
  - [`hey`](https://github.com/rakyll/hey) for quick load tests
  - [`wrk`](https://github.com/wg/wrk) for sustained benchmarks
- Tune for your host:
  - `NAVPATH_SQLITE_CACHE_SIZE_KB=200000`
  - `NAVPATH_SQLITE_MMAP_SIZE=268435456`
- Monitor logs via `RUST_LOG`. Avoid debug logging in hot loops in production.

## Development

- Run all tests for the workspace:
  ```bash
  cargo test
  ```
- Typical changes live in:
  - Service: `rust/navpath-service/src/`
  - Core crate: `rust/navpath-core/src/`
