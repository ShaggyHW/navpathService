# navpath-service crate overview

`navpath-service` is an Axum-based HTTP application that exposes the pathfinding capabilities of `navpath-core` over a REST interface. The binary is designed for read-only access to a precomputed NavPath SQLite database and emphasizes deterministic behavior, structured logging, and safe runtime configuration.

## Crate layout

- `src/main.rs` – orchestrates startup: sets up logging, resolves configuration, validates database connectivity, constructs shared state, and launches the Axum server.
- `src/config.rs` – handles environment and CLI configuration, including database path resolution, binding address, SQLite tuning, and jump point search (JPS) mode selection.
- `src/state.rs` – defines `AppState`, the shared state passed to request handlers. It stores the database path, SQLite open configuration, readiness flag, and deployment-wide JPS mode.
- `src/routes.rs` – declares HTTP routes and handlers for health, readiness, version, and pathfinding APIs. It performs per-request database opens, builds providers, and executes searches.

## Startup sequence (`src/main.rs`)

1. Initialize structured JSON logging via `tracing_subscriber` with optional `RUST_LOG`/`EnvFilter` overrides.
2. Resolve the SQLite database path using `config::resolve_db_path()` (CLI `--db` or `NAVPATH_DB`) and load optional SQLite PRAGMA tuning through `config::load_db_open_config()`.
3. Read deployment-wide JPS mode from `NAVPATH_JPS_MODE` (default `auto`, `off` disables JPS for all requests).
4. Validate database accessibility by opening it read-only using `navpath_core::db::open::open_read_only_with_config`.
5. Construct `AppState`, mark it ready, and build the Axum router via `routes::build_router`.
6. Bind to the configured socket address (`NAVPATH_HOST`/`NAVPATH_PORT`) and serve requests with `axum::serve`.

## Configuration (`src/config.rs`)

### Required

- Absolute database path from either:
  - CLI: `--db /absolute/path/to/worldReachableTiles.db`
  - Environment: `NAVPATH_DB=/absolute/path/to/worldReachableTiles.db`

### Optional environment variables

| Variable | Purpose | Default |
| --- | --- | --- |
| `NAVPATH_HOST` | Interface to bind | `0.0.0.0` |
| `NAVPATH_PORT` | TCP port | `8080` |
| `RUST_LOG` | Tracing filter (`info`, `debug`, etc.) | `info` |
| `NAVPATH_SQLITE_QUERY_ONLY` | Enables `PRAGMA query_only` when truthy | `1` |
| `NAVPATH_SQLITE_CACHE_SIZE_KB` | Read-only cache size (KB) | `200000` |
| `NAVPATH_SQLITE_MMAP_SIZE` | Memory map size (bytes) | `268435456` |
| `NAVPATH_SQLITE_TEMP_STORE` | Temp storage (`MEMORY` or `FILE`) | `MEMORY` |
| `NAVPATH_JPS_MODE` | Global JPS toggle (`auto`, `off`) | `auto` |

Invalid values fall back to defaults. SQLite tuning is best-effort: unsupported PRAGMAs are ignored.

## HTTP API (`src/routes.rs`)

### `GET /healthz`
Cheap liveness probe returning `{ "status": "ok" }`.

### `GET /readyz`
Marks readiness by checking `AppState::ready`. Returns `503` with `{ "ready": false }` until startup completes, then `{ "ready": true }`.

### `GET /version`
Returns service and core crate semantic versions:
```json
{ "service_version": "0.1.0", "core_version": "..." }
```

### `POST /find_path`
Executes pathfinding between two tiles.

**Request body (`FindPathRequest`):**
```jsonc
{
  "start": [x, y, plane],
  "goal": [x, y, plane],
  "options": { /* optional SearchOptions */ },
  "db_path": "/optional/override"  // not currently supported; yields 400
}
```

**Query parameters (`FindPathQuery`):**
- `only_actions` / alias `actions_only`: when truthy, return only the `actions` array.

**Behavior:**
1. Rejects per-request database overrides (`db`, `db_path`) with `400 ERR_DB_SELECTION_UNSUPPORTED` to enforce single-DB deployments.
2. Merges provided `SearchOptions` with defaults and injects `options.extras["start_tile"] = [start_x, start_y, start_plane]` to support core gating logic.
3. Applies deployment JPS policy: `NAVPATH_JPS_MODE=off` forces `options.extras["jps_enabled"] = false`.
4. Opens the SQLite database read-only using the deployment configuration, builds a `SqliteGraphProvider`, and runs `navpath_core::astar::AStar::find_path`.
5. Logs execution details (`reason`, node expansions, `path_len`, duration) with structured JSON.
6. Returns either the full `PathResult` or the `actions` list, depending on request flags. Errors produce `500` with a JSON payload.

## Pathfinding pipeline

1. `db::open::open_read_only_with_config` (from `navpath-core`) yields a `rusqlite::Connection` configured for read-only workloads.
2. `Database::from_connection` wraps the connection in `navpath-core` abstractions.
3. `CostModel::new(options.clone())` computes movement and interaction costs for this request.
4. `SqliteGraphProvider::new` wires the database and cost model, exposing graph queries over tiles, interaction nodes, and cached metadata.
5. `AStar::new` runs the core search algorithm, optionally using JPS pruning based on `options.extras`.
6. Results include `actions`, `path` (tiles), expansion metrics, and timing data consumed by clients.

## Logging and observability

- Structured JSON logs via `tracing` capture context (`db_path`, `jps_mode`, `reason`, `expanded`, etc.).
- Use `RUST_LOG=debug` for verbose traces during development; prefer `info` or higher in production.
- Health/readiness endpoints support standard Kubernetes-style probes.

## Deployment tips

- Warm caches by calling `GET /readyz` immediately after startup to trigger database initialization.
- Tune SQLite PRAGMAs to match host memory limits; defaults favor high cache utilization for performance.
- Run behind a reverse proxy or load balancer that performs liveness/readiness checks and provides TLS termination.
- Since the database is opened read-only per request, ensure the underlying file resides on fast storage (NVMe or memory-mapped network disk) for best latency.

## Development workflow

- Build from the workspace root: `cargo build -p navpath-service`.
- Run the service locally with a sample database and `.env` file (`rust/navpath-service/.env`) or exported variables.
- Execute workspace tests: `cargo test`.
- Modify service routes under `rust/navpath-service/src/` and core pathfinding behavior under `rust/navpath-core/src/`.
