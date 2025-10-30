# navpath-service

Axum-based HTTP service that serves pathfinding over a grid using a database-backed HPA* planner (micro A* is used internally for intra-cluster refinement). A read-only SQLite database is required.

- Health: GET /healthz → {"status":"ok"}
- Readiness: GET /readyz → {"ready":true|false} (opens DB and performs a minimal query)
- Version: GET /version → {"version":"x.y.z"}
- Pathfinding: POST /find_path → { path, actions } or only actions

## Quickstart

1) Prerequisites
- Rust (stable)
- SQLite DB file (absolute path). Example DB provided at repo root: `worldReachableTiles.db`.

2) Environment
- The service reads process environment variables. It does NOT auto-load a .env file.
- Either export values, or `source` a file before running.

Example (bash):
```bash
export NAVPATH_DB="$PWD/worldReachableTiles.db"
export NAVPATH_HOST=127.0.0.1
export NAVPATH_PORT=8080
export NAVPATH_MOVE_COST_MS=200
export RUST_LOG=info,navpath_service=debug,axum=info
```

3) Run the server
```bash
cargo run -p navpath-service
```

4) Probe
```bash
curl -s http://127.0.0.1:8080/healthz
curl -s http://127.0.0.1:8080/readyz
curl -s http://127.0.0.1:8080/version
```

## API

### POST /find_path
Request body (JSON):
```json
{
  "start": {"x": 3200, "y": 3200, "plane": 0},
  "end":   {"x": 3203, "y": 3202, "plane": 0},
  "requirements": [
    {"key": "quest.points", "value": 20},
    {"key": "has_item.gloves", "value": "true"}
  ]
}
```

Query parameters:
- `only_actions=true` to return just the `actions` array.

Response (full):
```json
{
  "path": [[3200,3200,0],[3201,3200,0],[3202,3201,0],[3203,3202,0]],
  "actions": [
    {"type":"move","from":{"min":[3200,3200,0],"max":[3200,3200,0]},"to":{"min":[3201,3200,0],"max":[3201,3200,0]},"cost_ms":200}
  ]
}
```

Response (only actions):
```bash
curl -s "http://127.0.0.1:8080/find_path?only_actions=true" \
  -H 'content-type: application/json' \
  -d '{"start":{"x":3200,"y":3200,"plane":0},"end":{"x":3203,"y":3202,"plane":0},"requirements":[]}'
```

Notes:
- `requirements` is an array of `{key, value}` pairs. Numeric comparisons are supported when both sides are numeric; otherwise string eq/neq apply.
- Walkability and allowed tiles are constrained by the DB; `NAVPATH_DB` is required.

## Environment variables
- `NAVPATH_DB` (required, absolute path): SQLite DB file. The service will refuse to start if this is not set or cannot be opened.
- `NAVPATH_HOST` (default: 127.0.0.1)
- `NAVPATH_PORT` (default: 8080)
- `NAVPATH_MOVE_COST_MS` (default: 200) per-tile move cost used when emitting actions.
- `RUST_LOG` (example: `info,navpath_service=debug,axum=info`)
- `NAVPATH_DEBUG_RESULT_PATH` (optional): absolute path where the service writes the last `/find_path` JSON response for debugging.

## Logging & Performance
- Structured logs via `tracing`/`tracing-subscriber`.
- `/find_path` logs include algorithm time and total handler time fields: `algo_ms`, `total_ms`.
- `/readyz` logs probe latency and returns `ready=false` if DB cannot be opened or minimally queried.
