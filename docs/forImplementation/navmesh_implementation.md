# Migrating from Tile-Based Graph to Navmesh

This document explains how to adapt the existing tile-based pathfinding system (used in `navpath-core` and `navpath-service`) to a **navmesh-based** navigation graph built from the generated `navmesh.db`.

---

## Overview

You already have:
- A Python builder (`navmesh_builder.py`) that exports `cells`, `portals`, and `offmesh_links` into a SQLite database.
- A working A* pathfinder with Jump Point Search (JPS) on a tile grid.
- An API service (`navpath-service`) wrapping this core.

You **do not** need a new pathfinding algorithm.  
A* remains the same; only the **graph provider** and **path reconstruction** layers change.

---

## 1. Replace the Tile Graph with a Navmesh Graph

### Current
`SqliteGraphProvider` enumerates tile neighbors using `tiles`, `doors`, `objects`, etc.

### Target
Create a new `NavmeshGraphProvider` that reads from:

- `cells(id, plane, kind, wkb, …)` → graph nodes  
- `portals(a_id, b_id, x1, y1, x2, y2, length)` → undirected movement edges  
- `offmesh_links(src_cell_id?, dst_cell_id, cost, plane_from?, plane_to?, …)` → teleport/door/interaction edges  

#### Steps
- Add `navpath-core/db/navmesh.rs` to mirror your existing database wrappers.
- Implement `GraphProvider` for `NavmeshGraphProvider`:
  - **Movement neighbors** come from `portals` where `a_id == current_cell`.
  - **Off-mesh neighbors** come from `offmesh_links` with `src_cell_id == current_cell`.
  - **Start/goal mapping**: use an R-tree query to find which `cell` contains `(x, y, plane)`.

A* can remain unchanged; it will now operate on **cell IDs** instead of tile coordinates.

---

## 2. Replace JPS with a Funnel Algorithm

JPS is a grid optimization and not applicable to navmeshes.

After A* finds a sequence of **cells**, reconstruct the path by:
1. Gathering the ordered **portal segments** traversed.
2. Including the start and end points.
3. Running the **funnel (string-pulling)** algorithm to output waypoints.

Add `navpath-core/funnel.rs` implementing a simple funnel algorithm, and call it from `astar::reconstruct_path`.

If desired, convert the resulting world coordinates back into tile centers for API compatibility.

---

## 3. Cost and Heuristic Adjustments

### Costs
- **Portal edges**: use `portals.length`.
- **Off-mesh links**: use `offmesh_links.cost` or fall back to `CostModel` defaults (door, lodestone, etc.).
- **Plane changes**: apply an extra cost for cross-plane transitions.

### Heuristic
- **Within-plane**: Euclidean distance from cell centroid → goal centroid.
- **Cross-plane or teleport cases**: zero or multi-stage heuristic to preserve admissibility.

---

## 4. Requirements and Actions

Off-mesh links can store requirement and action metadata in `meta_json`.

During neighbor expansion:
- Skip edges that require unmet conditions.

During reconstruction:
- Convert off-mesh links to `ActionStep`s (e.g., open door, use lodestone).

This reuses the existing requirement and action logic with minimal change.

---

## 5. Service Integration

Add a configuration option:
```env
NAVPATH_GRAPH_MODE=grid|navmesh
