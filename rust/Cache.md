Sure — here’s your content converted into a clean, structured **Markdown** format:

---

# Findings

### No Complete-Path Cache

There is **no memoization of full paths** in `rust/navpath-core`.
`AStar::find_path()` (`rust/navpath-core/src/astar.rs`) recomputes each request and does **not** store results across calls.

---

### Tile Existence Cache

**`PlaneTileCache`** (`rust/navpath-core/src/graph/plane_cache.rs`)
Caches **per-plane sets of existing tiles**.
Used in `provider.rs` via `_tile_exists()` for:

* movement
* door
* object
* ifslot
* NPC
* item destination checks

---

### Per-Tile Touching Caches

**`TouchingNodesCache`** (`rust/navpath-core/src/graph/touch_cache.rs`) caches results of:

* `Database.iter_object_nodes_touching(tile)`
* `Database.iter_npc_nodes_touching(tile)`

Used by:

* `neighbors_objects()`
* `neighbors_npcs()`
  in `rust/navpath-core/src/graph/provider.rs`.

---

### Requirement Row Cache (Per-Resolution)

**`NodeChainResolver`** (`rust/navpath-core/src/nodes.rs`) caches **requirement rows in-memory** for the lifetime of a single resolver instance (per `neighbors_*` call), **not globally**.

---

### Chain-Head Index

Built once via **`ChainHeadIndexState`** and reused to filter non-head nodes; this is **not a path cache**.

---

### Service-Level Reuse (From Memory)

Per the memory *“Axum HTTP service… ProviderManager keyed by db_path”*, the service reuses a single `SqliteGraphProvider` instance per DB.
This means:

* `plane_cache` and `touch_cache` **persist across requests**
* Caches **benefit repeated calls**

*(Referenced memory ID ends with 461d.)*

---

# Answer

**No**, the cache system does **not cache complete paths**.
Caching currently accelerates:

* Neighbor discovery
* Tile existence checks
  but **not end-to-end path results**.

The recent integration (adding items to `neighbors()` and the combined ordering test) did **not** change caching scope; it preserved and continued using:

* `PlaneTileCache` for tile checks
* `TouchingNodesCache` for object/NPC per-tile queries
* Per-resolver requirement row caching

---

# Optional Next Steps

### 1. Add a Path-Level LRU Cache in A* or Service

**Key:**
`(start_tile, goal_tile, toggles in SearchOptions, requirements_map digest, start_tile for lodestones)`

**Value:**
`PathResult`

**Eviction:**
Size/time-based LRU

> Risk of staleness is low because the DB is opened read-only and requirements come from request options.

---

### 2. Broaden Neighbor Caching

* Cache `iter_lodestone_nodes()` results (read-only, small table).
* Consider a **door-touching cache** like `TouchingNodesCache` if door queries are frequent.

> If you want, I can implement a small LRU path cache in `AStar` or wrap it in the service layer.

---

# Summary

* ✅ Caching exists for **tiles** and **neighbor lists**.
* ❌ **Complete-path caching** is **not implemented**.
* ✅ Caches persist across requests via the reused `SqliteGraphProvider`.

---
