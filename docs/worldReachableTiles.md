# worldReachableTiles.db — Schema and Semantics

## Overview
- **Purpose**: A trimmed, analysis-ready SQLite snapshot containing only tiles reachable from a given start tile, plus teleport metadata to support pathfinding and movement analysis.
- **Produced by**: Rust command `TileCleaner` which:
  - Performs a BFS from the start tile over walkable edges derived from `tiles.walk_mask` with reciprocal and diagonal constraints.
  - Includes additional edges from teleports: doors, lodestones, objects, NPCs, and interface-slot teleports.
  - Writes the subset of reachable rows into a new `tiles` table and copies supporting teleport tables and the `teleports_all` view.
  - Sanitizes `tiles.walk_mask` so bits leading to tiles outside the reachable set are cleared.

## Building/Regenerating
- **Defaults**: Source `tiles.db` at repo root; output `worldReachableTiles.db` at repo root.
- **Command**:
  ```bash
  cargo run --release -- TileCleaner \
    --src ./tiles.db \
    --out ./worldReachableTiles.db \
    --start_x 3200 --start_y 3200 --start_plane 0
  ```
- **Notes**:
  - Foreign keys are disabled on the destination while tiles are created to avoid ordering issues; auxiliary tables and views are copied after tiles.
  - The `tiles` table schema and tile indexes (and later, other tables' indexes) are recreated to match the source schema.

## Schema

### Table: `tiles`
- **Columns**:
  - `x INTEGER` — world X coordinate.
  - `y INTEGER` — world Y coordinate.
  - `plane INTEGER` — Z-level (0=ground).
  - `walk_mask INTEGER` — bitmask of allowed movements from this tile.
  - `RegionID INTEGER` — region identifier.
- **Primary key**: `(x, y, plane)`.
- **Index**:
  - `idx_tiles_walkable ON tiles(x, y, plane)` (partial index for walkable tiles).
- **`walk_mask` bit mapping (bit 0..7)**:
  - 0: `left`
  - 1: `bottom`
  - 2: `right`
  - 3: `top`
  - 4: `topleft`
  - 5: `bottomleft`
  - 6: `bottomright`
  - 7: `topright`
- **Movement reconciliation (applied during build)**:
  - Cardinal moves must be reciprocal between adjacent tiles.
  - Diagonal moves require both orthogonal edges (e.g., `topleft` requires `top` and `left`).
  - After BFS determines the reachable set, any `walk_mask` bit pointing to an unreachable neighbor is cleared.

### Teleport Metadata Tables
These tables are copied to support additional movements beyond adjacency. All have `id INTEGER PRIMARY KEY` and optional `requirement_id` referencing `teleports_requirements.id` (no enforced FK in this DB).

- **`teleports_door_nodes`**
  - Key cols: `tile_outside_(x,y,plane)`, `tile_inside_(x,y,plane)`, `direction`, `open_action`, `cost`, `next_node_type`, `next_node_id`, `requirement_id`.
  - Index: `idx_tdoor_req(requirement_id)`.

- **`teleports_ifslot_nodes`**
  - Key cols: `interface_id`, `component_id`, `slot_id`, `click_id`, destination bounds `dest_min/max_(x,y)`, `dest_plane`, `cost`, `next_node_type`, `next_node_id`, `requirement_id`.
  - Index: `idx_tif_req(requirement_id)`.

- **`teleports_item_nodes`**
  - Key cols: `item_id`, `action`, destination bounds, `dest_plane`, `next_node_type`, `next_node_id`, `cost`, `requirement_id`.
  - Index: `idx_titem_req(requirement_id)`.

- **`teleports_lodestone_nodes`**
  - Key cols: `lodestone`, `dest_x`, `dest_y`, `dest_plane`, `cost`, `next_node_type`, `next_node_id`, `requirement_id`.
  - Index: `idx_tlode_req(requirement_id)`.

- **`teleports_npc_nodes`**
  - Key cols: `match_type`, `npc_id`, `npc_name`, `action`, destination bounds, origin bounds (`orig_min/max_(x,y)`, `orig_plane`), `search_radius`, `cost`, `next_node_type`, `next_node_id`, `requirement_id`.
  - Index: `idx_tnpc_req(requirement_id)`.

- **`teleports_object_nodes`**
  - Key cols: `match_type`, `object_id`, `object_name`, `action`, destination bounds, origin bounds, `search_radius`, `cost`, `next_node_type`, `next_node_id`, `requirement_id`.
  - Index: `idx_tobj_req(requirement_id)`.

- **`teleports_requirements`**
  - Cols: `metaInfo`, `key`, `value`, `comparison` (free-form requirement metadata).
  - Index: `idx_teleport_req_all(id)`.

### View: `teleports_all`
- **Signature**: `(kind, id, src_x, src_y, src_plane, dst_x, dst_y, dst_plane, cost, requirement_id)`.
- **Union of**:
  - `'door'` from `teleports_door_nodes`: `(src=tile_outside_*, dst=tile_inside_*)`.
  - `'lodestone'` from `teleports_lodestone_nodes`: `(src=NULLs, dst=dest_*)`.
  - `'npc'` from `teleports_npc_nodes`: `(src=orig_min_*, dst=dest_min_*)`.
  - `'object'` from `teleports_object_nodes`: `(src=orig_min_*, dst=dest_min_*)`.
  - `'item'` from `teleports_item_nodes`: `(src=NULLs, dst=dest_*)`.
  - `'ifslot'` from `teleports_ifslot_nodes`: `(src=NULLs, dst=CAST(dest_min_*) ...)` and only rows with non-NULL `dest_min_x`.
- **Notes**: Some `src_*` fields may be `NULL` for abstract/global teleports.

## Sample Queries
- **Count reachable tiles**:
  ```sql
  SELECT COUNT(*) FROM tiles;
  ```

- **Get a specific tile**:
  ```sql
  SELECT * FROM tiles WHERE x = 3200 AND y = 3200 AND plane = 0;
  ```

- **Find all walkable tiles (blocked = 0)**:
  ```sql
  SELECT x, y, plane FROM tiles WHERE blocked = 0;
  ```

- **Teleports touching a coordinate (as dst)**:
  ```sql
  SELECT *
  FROM teleports_all
  WHERE dst_x = 3200 AND dst_y = 3200 AND dst_plane = 0;
  ```

- **Door edges that cross a tile boundary**:
  ```sql
  SELECT id, src_x, src_y, src_plane, dst_x, dst_y, dst_plane, cost
  FROM teleports_all
  WHERE kind = 'door' AND src_x IS NOT NULL;
  ```

## Semantics Notes
- `walk_mask` bits reflect allowed neighbor moves after reconciliation and reachability sanitization; bits may differ from the source `tiles.db` because unreachable edges are cleared.
- Coordinates are world space; `plane` is the elevation level.

## Provenance
- Schema in code: `rust/src/db.rs`.
- Cleaner logic and BFS + teleports: `rust/src/commands/tile_cleaner.rs`.
- The output DB’s schema was verified via `sqlite3 .schema` against the generated file.
