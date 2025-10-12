# navpath-core crate overview

`navpath-core` provides the data structures, database abstractions, and search algorithms that power NavPath. It is a pure library crate consumed by `navpath-service` and other tooling. Major responsibilities include loading pathfinding state from a SQLite database, resolving interaction nodes, computing costs, and executing A* search with optional Jump Point Search (JPS) pruning.

## Module layout (`src/`)

- `lib.rs` – exposes top-level modules and re-exports key types like `SearchOptions`, `CostModel`, `Tile`, `PathResult`, and `JpsConfig`.
- `models.rs` – defines serializable DTOs (`Tile`, `ActionStep`, `NodeRef`, `Rect`, `PathResult`) used across the codebase and HTTP APIs.
- `options.rs` – declares `SearchOptions`, default tuning constants, and helpers such as `movement_only()`.
- `cost.rs` – implements `CostModel` for per-edge cost calculations and heuristics.
- `db/` – contains SQLite integration:
  - `open.rs` – read-only open configuration (`DbOpenConfig`) with PRAGMA tuning helpers.
  - `mod.rs` – `Database` wrapper with typed fetch/iterate helpers for tiles and node tables.
  - `rows.rs` / `queries.rs` – typed row structs and SQL string constants.
- `graph/` – graph traversal utilities:
  - `provider.rs` – `SqliteGraphProvider` implementing `GraphProvider::neighbors()` with caches, requirement gating, and node chain resolution.
  - `plane_cache.rs` – LRU-backed plane tile existence cache.
  - `touch_cache.rs` – caches nodes touching a tile (objects, NPCs) to avoid redundant queries.
  - `movement.rs` – movement edge masks and ordering helpers.
  - `chain_index.rs` – index of chain heads (detects non-head nodes in linked sequences).
- `nodes.rs` – `NodeChainResolver` that follows linked interaction chains, enforces requirements, and produces bounds.
- `astar.rs` – A* pathfinding engine with logging, reconstruction of `ActionStep`s, and optional JPS pruning hooks.
- `jps.rs` – Jump Point Search configuration and pruning implementation (`JpsConfig`, `JpsPruner`).
- `json.rs` – helper for omitting empty metadata in JSON serialization.

## Database layer (`db/`)

`Database` wraps a `rusqlite::Connection` and provides typed accessors for the NavPath schema:

- Tile fetches: `fetch_tile(x,y,plane)` and `iter_tiles_by_plane(plane)` drive movement edges and plane caches.
- Interaction node iterators and lookups: `iter_door_nodes()`, `iter_object_nodes_touching(tile)`, `fetch_requirement(id)`, etc.
- Helper structs in `rows.rs` map each table into strongly typed rows used elsewhere in the crate.
- `open::DbOpenConfig` reads environment toggles such as `NAVPATH_SQLITE_QUERY_ONLY`, `NAVPATH_SQLITE_CACHE_SIZE_KB`, and `NAVPATH_SQLITE_MMAP_SIZE`, applying them via `open_read_only_with_config()`.

All opens default to read-only. PRAGMA failures are ignored so the library works across SQLite builds.

## Graph provider (`graph/provider.rs`)

`SqliteGraphProvider` is responsible for enumerating neighbor edges around a tile based on the database contents and search options.

Key features:

- **Movement edges** – derived from tile mask bits (tiledata or allowed_directions). Respects plane existence checks via `PlaneTileCache`.
- **Doors** – two-way edges across door tiles, costed via `CostModel::door_cost`, with metadata describing door IDs and actions.
- **Lodestones** – only emitted from the start tile; requirement-gated and sorted deterministically.
- **Objects, NPCs, IF slots, Items** – each resolved through `NodeChainResolver` to follow chained interactions, honoring requirements and deduplicating destinations. Cache hits are served via `TouchingNodesCache` for per-tile queries.
- **Requirement evaluation** – uses search option extras (`requirements_map` / `requirements`) to determine eligibility.
- **Warmup** – `warm()` pre-builds chain-head indexes for stable latency.

`GraphProvider` is a trait consumed by `astar::AStar`, enabling plug-in graph sources.

## Pathfinding (`astar.rs`)

`AStar` orchestrates pathfinding:

1. Initializes logging metrics (expanded nodes, neighbor counts, JPS pruning stats).
2. Prioritizes nodes via a binary heap, applying deterministic tie-breaking for stable results.
3. Retrieves neighbors through `GraphProvider::neighbors()`, optionally pruning movement edges with `JpsPruner` when movement-only.
4. Tracks `came_from` metadata to reconstruct both tile paths and action steps, including bounding rectangles from node metadata.
5. Enforces limits (`max_expansions`, `timeout_ms` checks implemented at provider/consumer level) and returns `PathResult` with reason codes (`expansion-limit`, `no-path`, etc.).

The JPS integration reads configuration from search option extras (`jps_enabled`, `jps_allow_diagonals`, `jps_max_jump`), with nested object support (`extras.jps`).

## Search options and cost model

- `SearchOptions` (serde-enabled) defines toggles for action types, expansion/timeout limits, cost overrides, and extras map for custom metadata.
- `CostModel` converts `SearchOptions` into concrete costs and heuristics:
  - Default heuristics use Chebyshev distance for movement-only searches; otherwise zero to avoid overestimating teleport costs.
  - Node cost helpers respect per-action overrides and database-sourced costs.
  - Shared by both provider and A* for consistent cost accounting.

## Node chain resolution (`nodes.rs`)

`NodeChainResolver` traverses linked interaction nodes to determine final destination bounds and cumulative cost:

- Detects cycles, excessive depth (`max_chain_depth`), missing nodes, unmet requirements, and missing destinations.
- Builds `ChainResolution` used by the graph provider to materialize edges.
- Reuses requirement rows through an internal cache to minimize DB lookups.

## Caching infrastructure

- `PlaneTileCache` caches per-plane tile existence sets with LRU eviction.
- `TouchingNodesCache` memoizes tiles → interaction node rows for objects/NPCs to eliminate repetitive queries.
- `ChainHeadIndexState` identifies non-head nodes to avoid duplicate edges when chains share tails.

## Jump Point Search (`jps.rs`)

- `JpsConfig` encapsulates toggles for enabling JPS, allowing diagonals, and limiting jump distances.
- `JpsPruner` applies forced-neighbor logic to prune movement edges during A* expansion when movement-only (or when `extras.jps_prune_with_actions` is truthy).
- Designed to fall back gracefully when JPS is disabled or unsuitable (e.g., diagonal absence).

## JSON helpers and DTOs

- Serialization behavior matches the legacy Python service: empty metadata is omitted, field names align with previous API expectations, and all DTOs derive `Serialize`/`Deserialize` for easy interchange with the service layer.

## Testing strategy

- Each module includes targeted unit tests:
  - `astar.rs` verifies reconstruction, deterministic behavior, and JPS parity.
  - `options.rs`, `cost.rs`, `models.rs` ensure defaults and serialization match expectations.
  - `nodes.rs` exercises chain traversal edge cases (cycles, depth limits, missing destinations).
  - `graph/plane_cache.rs` confirms cache hits and LRU eviction.
  - `db/mod.rs` and related modules use in-memory SQLite setups for query coverage.

Run all tests via `cargo test -p navpath-core`.

## Integration with `navpath-service`

`navpath-service` depends on `navpath-core` for:

- Re-exported DTOs (`Tile`, `PathResult`, `ActionStep`), enabling shared JSON schemas.
- Database opening via `db::open::open_read_only_with_config()` (aligns with service `.env` variables).
- Search execution through `AStar` and `SqliteGraphProvider` constructed per request.
- Costing and options serialization used in HTTP payloads.

Any external consumer can embed `navpath-core` to perform reads against NavPath SQLite databases, provided they supply valid `SearchOptions` and respond to async requirements if needed (e.g., via extras map).
