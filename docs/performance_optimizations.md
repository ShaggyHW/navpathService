# Navpath Performance Optimization Plan

This document outlines actionable optimizations to speed up the pathfinding service, referencing current code in `navpath/`.

## Profiling-first plan

- **Add targeted counters**
  - Instrument `navpath/api.py::find_path()` to log neighbor count, avg neighbors per expansion, DB query counts, and time per phase.
  - Wrap `Database` methods (e.g., `fetch_tile()`, `iter_*()`) with lightweight counters to find hotspots.
- **Use profilers**
  - cProfile + snakeviz for CPU hotspots.
  - py-spy or scalene for sampling without code mods.
  - Add an env toggle (e.g., `NAVPATH_PROFILE=1`) to avoid overhead in prod.

## High-impact optimizations

- **Tile existence fast-path (avoid per-neighbor DB lookups)** (COMPLETE)
  - Current: `SqliteGraphProvider._movement_edges()` calls `db.fetch_tile()` for each of up to 8 neighbors and `_select_dest_tile()` scans tiles via `db.fetch_tile()` inside loops.
  - Change: Build an in-memory existence map per plane once, using `Database.iter_tiles_by_plane(plane)`.
    - Add to `SqliteGraphProvider`: `_tile_exists(x, y, plane) -> bool` using `set[(x,y)]` or a compact bitset per plane.
    - Replace `db.fetch_tile(*dest) is None` checks in `graph.py::_movement_edges()` and `_select_dest_tile()` with the in-memory map.
    - Memory strategy: lazily build per plane on first touch; optionally evict planes via LRU if memory is a concern.

- **Reuse node chain resolver and memoize resolutions** (COMPLETE)
  - Implemented in `navpath/graph.py`: a single `NodeChainResolver` is created per `SqliteGraphProvider` via `self._get_resolver()` and reused across `neighbors()` calls.
  - Added provider-level memo: `self._chain_resolution_cache[(type, id)] -> ChainResolution` with `_resolve_chain_cached()` to avoid duplicate resolutions.
  - Implemented a tiny per-resolver requirement cache in `navpath/nodes.py::NodeChainResolver` to avoid repeated `Database.fetch_requirement()` calls during per-link gating.
- **Cache “touching” node queries by tile** (COMPLETE)
  - Current: `Database.iter_object_nodes_touching(tile)` and `iter_npc_nodes_touching(tile)` hit SQLite per expansion.
  - Change: Add a small LRU cache (e.g., capacity 4096 tiles) in `SqliteGraphProvider` keyed by `tile` with values of the rows list for objects/NPCs.
  - Invalidate only on DB changes (not expected at runtime as we open in RO mode).

  - **Reduce heavy metadata copying**
  - Current: edges attach `asdict(row)` into `metadata["db_row"]` across `graph.py` methods.
  - Change: Make this optional behind an option flag, e.g., `options.extras["include_db_row"] = False` by default for speed. Keep minimal metadata fields only.

- **Persist provider across requests**
  - Current: `navpath/api.py::find_path()` creates a new `Database` and `SqliteGraphProvider` per call, discarding caches.
  - Change: If used in a long-lived process, host a tiny service layer that keeps a single `Database` and `SqliteGraphProvider` per DB path and reuses them, preserving: lodestone cache, ifslot/item cache, chain-head index, tile-existence map, and touching-node LRU.
  - See `docs/navpath_service.md` for the full service-mode design, endpoints, concurrency model, and implementation plan.

## A* search efficiency

- **Heuristic improvements (keep optimality)**
  - Current: `CostModel.heuristic()` returns 0 when any of `use_lodestones/objects/ifslots/npcs/items` is enabled, degrading to Dijkstra.
    - Keep admissible but informative: `h = max(chebyshev*step_cost - best_teleport_discount, 0)`, where `best_teleport_discount` is a conservative upper bound (e.g., `min_lodestone_cost` if only one teleport can apply). Mark as experimental and guarded by an option because correctness proof depends on constraints.
    - Movement-only fast path: when all action edges are disabled, keep current Chebyshev heuristic (already implemented) and consider enabling Jump Point Search for grid moves only.

- **Pruning and tie-breaking**
  - Current: tie-breaking is deterministic with `(f, h, g, seq, tile)`. Keep.
  - Add optional weighted A* (`w in [1.05..1.2]`) if slight suboptimality is acceptable for speed: `f' = g + w*h` (behind `options.extras["weighted_a_star"]`).

- **Remove unused closed set**
  - Current: `astar.py` writes `closed[current] = True` but never reads it.
  - Change: Remove `closed` to save dict writes and memory, or use it to skip neighbor gen if you ever add a reopen policy that benefits from it. As written, it’s dead weight.

## Database-level improvements

- **SQLite PRAGMA tuning for RO workloads** (set once in `db.open_connection()`):
  - `PRAGMA query_only = ON;`
  - `PRAGMA cache_size = -200000;` (approx 200MB page cache, tune per env)
  - `PRAGMA mmap_size = 268435456;` (256MB, if filesystem supports)
  - `PRAGMA temp_store = MEMORY;`
  - Note: Read-only URI mode may ignore some write-related pragmas; the above are safe for RO scenarios. Measure impact before committing large cache sizes.

- **Prepared statements and row_factory**
  - Python’s sqlite3 caches statements internally. We already use `sqlite3.Row` and then convert to dataclasses. If conversion becomes a hotspot, consider returning tuples for hot paths (e.g., tile existence pre-scan) to reduce per-row overhead.

## Graph/neighbor generation specifics

- **`graph.py::_movement_edges()`**
  - Use the existence map to avoid SQLite lookups per neighbor.
  - Micro: prebind locals (e.g., `append = edges.append`) inside loops for tiny gains after the bigger changes above.

- **`graph.py::_select_dest_tile()`**
  - Replace inner `db.fetch_tile()` checks with existence map probes.
  - Early-exit scan order is fine; consider scanning by proximity to `goal` if it improves hit rate, but keep determinism.

- **`graph.py::_ensure_chain_head_index()`**
  - It’s built lazily on first neighbor generation. Optionally build at provider init to move cost outside the first query latency.

- **`nodes.py::NodeChainResolver.resolve()`**
  - Add requirement cache similar to `SqliteGraphProvider._requirement_cache` to avoid repeated `fetch_requirement()` per link.
  - Respect provider memoization of whole chain resolutions (pass a shared cache or call back into provider).

## Configuration and defaults

- **Reasonable limits**
  - Current defaults in `options.py`: `DEFAULT_MAX_EXPANSIONS = 250_000_000`, `DEFAULT_TIMEOUT_MS = 1_000_000_000`. These are extremely high and can hide performance problems.
  - Suggest defaults like `max_expansions = 1_000_000`, `timeout_ms = 5000` with the ability to raise per call.

- **Logging**
  - Avoid `LOGGER.debug` inside hot loops unless debug logging is enabled. Guard with `if LOGGER.isEnabledFor(logging.DEBUG):` where costful formatting occurs.


## Quick wins (implementation order)

1. **Tile existence map** for `graph.py` to remove per-neighbor DB lookups.
2. **Reuse `NodeChainResolver` + memoize resolutions** in `SqliteGraphProvider`.
3. **Make `db_row` metadata optional** to cut serialization cost.
4. **Persist `SqliteGraphProvider` across requests** in a long-lived process.
5. **Lower default limits** for `max_expansions`/`timeout_ms` and add profiling counters.
6. **SQLite PRAGMAs** for read-only performance.

## Benchmark harness

- Add a small script or test that runs `navpath/api.py::find_path()` over N start/goal pairs and prints:
  - expansions, duration_ms, neighbors per expansion, DB hits per expansion (by method), cache hit rates (tile existence, touching-node LRU, chain memo), and path length.
- Store results under `docs/benchmarks/` to track improvements over time.
