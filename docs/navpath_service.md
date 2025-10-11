# Navpath Service Mode (Persistent Provider)

This document describes how to run `navpath` as a long‑lived service to reuse the SQLite connection and hot in‑memory caches in `navpath/graph.py`, improving latency and throughput compared to launching a new process per request.

## Why a service?

- **Avoid warm‑up on every run**: `navpath/api.py::find_path()` currently opens the DB and builds caches per call, then closes.
- **Reuse hot caches in `navpath/graph.py`**:
  - Per‑plane tile existence maps built via `Database.iter_tiles_by_plane()` and kept in an LRU (`_plane_tile_sets`).
  - Lodestone rows and destination validity maps (`_ensure_lodestone_cache()`).
  - Ifslot/item table caches (`_ensure_ifslot_cache()`, `_ensure_item_cache()`).
  - Per‑tile LRU caches for touching nodes: `_get_object_nodes_touching_cached()` and `_get_npc_nodes_touching_cached()`.
  - Chain‑head index built once in `_ensure_chain_head_index()`.
- **Stable performance**: warm caches make latency predictable and reduce repeated DB work.

## Proposed architecture

- **Framework**: HTTP service using FastAPI + Uvicorn (simple JSON IO, easy to deploy/containerize).
- **ProviderManager**: A singleton registry mapping `db_path -> ProviderHandle`.
  - `ProviderHandle` holds:
    - `Database` (read‑only connection from `navpath/db.py::open_connection(check_same_thread=False)`).
    - `SqliteGraphProvider` (long‑lived instance with caches).
    - `threading.RLock` to serialize DB access across threads.
  - Lifecycle: create on first request for a given DB path; keep until process exit.
- **Per request**: build a fresh `SearchOptions` and `CostModel`, set `options.extras["start_tile"]`, then run `astar()` with the persistent provider.

## HTTP API

- POST `/find_path`
  - Request body:
    ```json
    {
      "start": [x, y, plane],
      "goal": [x, y, plane],
      "options": { /* mirrors navpath/options.py::SearchOptions fields and extras */ },
      "db_path": "optional path to SQLite DB"
    }
    ```
  - Response: `navpath/path.py::PathResult.to_json_dict()` (or actions‑only variant if `options.extras["actions_only"]` is set or a query flag is used).

- GET `/healthz`
  - Returns 200 if the process is up.

- GET `/readyz`
  - Returns 200 when the provider is warmed for the default DB path (DB opened and `_ensure_chain_head_index()` completed at least once).

- GET `/version`
  - Returns service version and optionally git SHA.

## Option safety and cache correctness

`SqliteGraphProvider` currently memoizes a resolver and chain resolutions:
- `_get_resolver(self, options)` creates a single `NodeChainResolver` the first time, capturing `SearchOptions` (including `requirements_map`, `max_chain_depth`).
- `_chain_resolution_cache[(type, id)]` stores `ChainResolution` objects without considering options.

This is efficient for one set of options but risky for a multi‑tenant service where options vary between requests.

Two safe approaches:

- **A. Per‑request resolver**
  - Always construct a fresh `NodeChainResolver` per request and avoid sharing it in the provider.
  - Clear or disable `_chain_resolution_cache` across requests.
  - Simple and safe; small overhead but acceptable.

- **B. Cache key includes options signature**
  - Compute a stable `options_signature` (e.g., hash of: requirement map, max_chain_depth, cost overrides relevant to chain links).
  - Memoize with key `(type, id, options_signature)`.
  - Keep a small LRU for these entries to bound memory.

Recommendation: start with approach A for correctness and simplicity; optimize later if profiling shows resolver work is hot.

## Concurrency model

- SQLite supports concurrent readers; Python `sqlite3.Connection` requires care.
- Open the connection with `check_same_thread=False` and guard DB use with a `threading.RLock` per `ProviderHandle`.
- Alternative (more complex): per‑request connection pool while keeping caches in memory—requires threading changes to `Database` usage throughout.

## Telemetry and logging

- Log per request (similar to `navpath/api.py::find_path()`):
  - `reason`, `expanded`, `path_len`, `total_cost_ms`, `duration_ms`, `req_filtered`, and `db_path`.
- Optional: Prometheus `/metrics` endpoint later.

## Configuration

- Environment variables for sensible defaults:
  - `NAVPATH_DB=/path/to/worldReachableTiles.db`
  - `NAVPATH_LOG_LEVEL=INFO`
  - Optional cache tunables (e.g., plane LRU size, touching‑node LRU capacity).

## Implementation plan

1. Create `navpath/service.py` with a FastAPI app.
   - POST `/find_path` builds `SearchOptions` and `CostModel` per request and calls `astar()` using the persistent `SqliteGraphProvider`.
   - GET `/healthz`, `/readyz`, `/version`.
2. Add `ProviderManager` that retains `Database` + `SqliteGraphProvider` per `db_path` and a per‑provider `RLock`.
3. Ensure option safety in `navpath/graph.py`:
   - Short term: construct a new `NodeChainResolver` per request or clear `_chain_resolution_cache` between requests.
   - Long term: change `_chain_resolution_cache` key to include an options signature.
4. Add basic timing/logging around each request.
5. Document runtime and example curl invocations.

## Example request/response

Request:
```json
{
  "start": [3200, 3200, 0],
  "goal": [3210, 3211, 0],
  "options": {
    "use_doors": true,
    "use_lodestones": true,
    "max_expansions": 1000000,
    "timeout_ms": 5000,
    "extras": {
      "requirements": [{"key": "agility", "value": 60}]
    }
  }
}
```

Response (condensed):
```json
{
  "reason": null,
  "expanded": 1234,
  "cost_ms": 9876,
  "path": [[3200,3200,0], ...],
  "actions": [{"type": "move", ...}, {"type": "door", ...}]
}
```

## Notes and trade‑offs

- Keeping a long‑lived `SqliteGraphProvider` maximizes reuse of:
  - Tile existence sets, lodestones, ifslot/items caches, chain‑head index, touching‑node LRUs.
- Since the DB is opened read‑only, caches are stable and need no invalidation during runtime.
- Resolver option leakage must be addressed before multi‑tenant use.

## Future enhancements

- Optional `/metrics` Prometheus exporter.
- Option B (options‑aware chain cache) if resolver work is hot.
- Graceful warm‑up: prebuild plane maps and chain‑head index at startup.
- gRPC façade if binary clients are preferred; keep the same provider core.
