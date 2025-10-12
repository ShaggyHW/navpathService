# navmesh.db Schema Overview

- tables inspected: `meta`, `cells`, `rtree_cells` (virtual) with backing tables, `portals`, `offmesh_links`
- inspection source: `sqlite3 navmesh.db ".schema"` and sample data queries (Oct122025)

## meta

Stores metadata about the generated navmesh artifact.

- **Columns**
  - `key`: TEXT, PRIMARY KEY
  - `value`: TEXT, NULL
- **Constraints**
  - PRIMARY KEY (`key`)
- **Foreign Keys**
  - None declared
- **Indexes**
  - Implicit unique index backing the primary key

### Example row

```json
{
  "key": "source_db",
  "value": "worldReachableTiles.db"
}
```

## cells

Represents walkable navigation cells; geometry is serialized as Well Known Binary (WKB).

- **Columns**
  - `id`: INTEGER, PRIMARY KEY
  - `plane`: INTEGER, NOT NULL
  - `kind`: TEXT, NOT NULL (`'polygon'` or `'triangle'`)
  - `wkb`: BLOB, NOT NULL (geometry payload)
  - `area`: REAL, NOT NULL
  - `minx`: REAL, NOT NULL
  - `miny`: REAL, NOT NULL
  - `maxx`: REAL, NOT NULL
  - `maxy`: REAL, NOT NULL
- **Constraints**
  - PRIMARY KEY (`id`)
- **Foreign Keys**
  - None declared
- **Indexes**
  - `idx_cells_plane` on (`plane`)
  - Automatic primary-key index

### Example row

```json
{
  "id": 1,
  "plane": 1,
  "kind": "polygon",
  "wkb_bytes": 3853,
  "area": 311.0,
  "minx": 1022.0,
  "miny": 1749.0,
  "maxx": 1058.0,
  "maxy": 1770.0
}
```

## rtree_cells (virtual table)

Spatial index over cell bounding boxes, implemented via SQLite's RTree module.

- **Definition**
  - `CREATE VIRTUAL TABLE rtree_cells USING rtree(id, minx, maxx, miny, maxy)`
- **Supporting tables**
  - `rtree_cells_rowid`
  - `rtree_cells_node`
  - `rtree_cells_parent`
- **Notes**
  - Managed automatically; data mirrors entries from `cells`

## portals

Pairs cells that share a traversable boundary segment.

- **Columns**
  - `id`: INTEGER, PRIMARY KEY
  - `plane`: INTEGER, NOT NULL
  - `a_id`: INTEGER, NOT NULL (references `cells.id`; no FK declared)
  - `b_id`: INTEGER, NOT NULL (references `cells.id`; no FK declared)
  - `x1`: REAL, NOT NULL
  - `y1`: REAL, NOT NULL
  - `x2`: REAL, NOT NULL
  - `y2`: REAL, NOT NULL
  - `length`: REAL, NOT NULL
- **Constraints**
  - PRIMARY KEY (`id`)
- **Foreign Keys**
  - None declared
- **Indexes**
  - `idx_portals_plane` on (`plane`)
  - Automatic primary-key index

### Example row

```json
{
  "id": 1,
  "plane": 1,
  "a_id": 1,
  "b_id": 4,
  "x1": 1030.0,
  "y1": 1770.0,
  "x2": 1047.0,
  "y2": 1770.0,
  "length": 17.0
}
```

## offmesh_links

Represents off-mesh transitions (teleports, ladders, etc.) sourced from higher-level node tables.

- **Columns**
  - `id`: INTEGER, PRIMARY KEY
  - `link_type`: TEXT, NOT NULL
  - `node_table`: TEXT, NOT NULL
  - `node_id`: INTEGER, NOT NULL
  - `requirement_id`: INTEGER, NULL (links to `requirements.id`; unenforced)
  - `cost`: REAL, NULL
  - `plane_from`: INTEGER, NULL
  - `plane_to`: INTEGER, NOT NULL
  - `src_cell_id`: INTEGER, NULL
  - `dst_cell_id`: INTEGER, NOT NULL
  - `meta_json`: TEXT, NULL (JSON payload)
- **Constraints**
  - PRIMARY KEY (`id`)
- **Foreign Keys**
  - None declared
- **Indexes**
  - `idx_offmesh_dst` on (`dst_cell_id`)
  - Automatic primary-key index

### Example row

```json
{
  "id": 1,
  "link_type": "lodestone",
  "node_table": "lodestone_nodes",
  "node_id": 23,
  "requirement_id": 27,
  "cost": 17000.0,
  "plane_from": null,
  "plane_to": 1,
  "src_cell_id": null,
  "dst_cell_id": 39,
  "meta_json": {"lodestone":"PRIFDDINAS","dest_point":[2208,3360,1]}
}
```

## Index summary

- `idx_cells_plane` on `cells(plane)`
- `idx_portals_plane` on `portals(plane)`
- `idx_offmesh_dst` on `offmesh_links(dst_cell_id)`
- Automatic primary-key indexes for all tables
- RTree index structures backing `rtree_cells`

## Additional observations

- No explicit foreign-key constraints are declared despite cross-table references.
- `cells.wkb` stores little-endian WKB geometry; downstream tooling must decode it.
- `meta` documents provenance such as source database and generation mode.
- `offmesh_links.meta_json` carries action-specific payloads for consumers.
