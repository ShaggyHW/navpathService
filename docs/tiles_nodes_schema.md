# worldReachableTiles.db Schema (Tiles and Node Tables)

This document describes the structure of the tiles and node-related tables present in `worldReachableTiles.db`.

- tables inspected: `tiles`, `door_nodes`, `lodestone_nodes`, `object_nodes`, `ifslot_nodes`, `npc_nodes`, `item_nodes`, supporting table `requirements`, and SQLite-managed `sqlite_sequence`
- inspection source: `sqlite3 worldReachableTiles.db ".schema"` and sample data queries (Oct 12 2025)

## tiles

Represents per-tile metadata and directional constraints.

- **Columns**
  - `x`: INTEGER, NOT NULL
  - `y`: INTEGER, NOT NULL
  - `plane`: INTEGER, NOT NULL
  - `tiledata`: INTEGER, NULL
  - `category`: TEXT, NULL
  - `allowed_directions`: TEXT, NULL
  - `blocked_directions`: TEXT, NULL
- **Constraints**
  - PRIMARY KEY (`x`, `y`, `plane`)
- **Foreign Keys**
  - None declared
- **Indexes**
  - None declared

### Example row

```json
{
  "x": 1022,
  "y": 1761,
  "plane": 1,
  "tiledata": 38,
  "category": "partially_open",
  "allowed_directions": "north,east,northeast",
  "blocked_directions": "west,south,northwest,southeast,southwest"
}
```

## door_nodes

Represents door interaction nodes with details for both open and closed states.

- **Columns**
  - `id`: INTEGER, PRIMARY KEY AUTOINCREMENT (implicitly NOT NULL)
  - `direction`: TEXT, NULL, CHECK `direction` IN ('IN','OUT')
  - `real_id_open`: INTEGER, NOT NULL
  - `real_id_closed`: INTEGER, NOT NULL
  - `location_open_x`: INTEGER, NOT NULL
  - `location_open_y`: INTEGER, NOT NULL
  - `location_open_plane`: INTEGER, NOT NULL
  - `location_closed_x`: INTEGER, NOT NULL
  - `location_closed_y`: INTEGER, NOT NULL
  - `location_closed_plane`: INTEGER, NOT NULL
  - `tile_inside_x`: INTEGER, NOT NULL
  - `tile_inside_y`: INTEGER, NOT NULL
  - `tile_inside_plane`: INTEGER, NOT NULL
  - `tile_outside_x`: INTEGER, NOT NULL
  - `tile_outside_y`: INTEGER, NOT NULL
  - `tile_outside_plane`: INTEGER, NOT NULL
  - `open_action`: TEXT, NULL
  - `cost`: INTEGER, NULL
  - `next_node_type`: TEXT, NULL, CHECK IN ('object','npc','ifslot','door','lodestone','item')
  - `next_node_id`: INTEGER, NULL
  - `requirement_id`: INTEGER, NULL, REFERENCES `requirements`(`id`)
- **Constraints**
  - CHECK constraint on `direction` restricting values to 'IN' or 'OUT'
  - CHECK constraint on `next_node_type` restricting values to 'object','npc','ifslot','door','lodestone','item'
  - `direction` is nullable despite the CHECK constraint
- **Foreign Keys**
  - `requirement_id` REFERENCES `requirements`(`id`)
- **Indexes**
  - None declared

### Example row

```json
{
  "id": 1,
  "direction": null,
  "real_id_open": 28693,
  "real_id_closed": 28691,
  "location_open_x": 2943,
  "location_open_y": 3440,
  "location_open_plane": 0,
  "location_closed_x": 2942,
  "location_closed_y": 3440,
  "location_closed_plane": 0,
  "tile_inside_x": 2942,
  "tile_inside_y": 3440,
  "tile_inside_plane": 0,
  "tile_outside_x": 2943,
  "tile_outside_y": 3440,
  "tile_outside_plane": 0,
  "open_action": "Open",
  "cost": 600,
  "next_node_type": null,
  "next_node_id": null,
  "requirement_id": null
}
```

## lodestone_nodes

Represents lodestone teleport nodes and their destination coordinates.

- **Columns**
  - `id`: INTEGER, PRIMARY KEY AUTOINCREMENT
  - `lodestone`: TEXT, NOT NULL
  - `dest_x`: INTEGER, NOT NULL
  - `dest_y`: INTEGER, NOT NULL
  - `dest_plane`: INTEGER, NOT NULL
  - `cost`: INTEGER, NULL
  - `next_node_type`: TEXT, NULL, CHECK IN ('object','npc','ifslot','door','lodestone','item')
  - `next_node_id`: INTEGER, NULL
  - `requirement_id`: INTEGER, NULL, REFERENCES `requirements`(`id`)
- **Constraints**
  - CHECK constraint on `next_node_type` restricting values to 'object','npc','ifslot','door','lodestone','item'
- **Foreign Keys**
  - `requirement_id` REFERENCES `requirements`(`id`)
- **Indexes**
  - None declared

### Example row

```json
{
  "id": 3,
  "lodestone": "AL_KHARID",
  "dest_x": 3297,
  "dest_y": 3184,
  "dest_plane": 0,
  "cost": 17000,
  "next_node_type": null,
  "next_node_id": null,
  "requirement_id": 3
}
```

## object_nodes

Represents world object interaction nodes with support for matching by id or name and optional origin/destination bounds.

- **Columns**
  - `id`: INTEGER, PRIMARY KEY AUTOINCREMENT
  - `match_type`: TEXT, NOT NULL, CHECK `match_type` IN ('id','name','any')
  - `object_id`: INTEGER, NULL
  - `object_name`: TEXT, NULL
  - `action`: TEXT, NULL
  - `dest_min_x`: INTEGER, NULL
  - `dest_max_x`: INTEGER, NULL
  - `dest_min_y`: INTEGER, NULL
  - `dest_max_y`: INTEGER, NULL
  - `dest_plane`: INTEGER, NULL
  - `orig_min_x`: INTEGER, NULL
  - `orig_max_x`: INTEGER, NULL
  - `orig_min_y`: INTEGER, NULL
  - `orig_max_y`: INTEGER, NULL
  - `orig_plane`: INTEGER, NULL
  - `search_radius`: INTEGER, NOT NULL, DEFAULT 20
  - `cost`: INTEGER, NULL
  - `next_node_type`: TEXT, NULL, CHECK IN ('object','npc','ifslot','door','lodestone','item')
  - `next_node_id`: INTEGER, NULL
  - `requirement_id`: INTEGER, NULL, REFERENCES `requirements`(`id`)
- **Constraints**
  - CHECK constraint on `match_type` restricting values to 'id', 'name', 'any'
  - CHECK constraint on `next_node_type` restricting values to 'object','npc','ifslot','door','lodestone','item'
- **Foreign Keys**
  - `requirement_id` REFERENCES `requirements`(`id`)
- **Indexes**
  - None declared

### Example row

```json
{
  "id": 0,
  "match_type": "name",
  "object_id": null,
  "object_name": "Ladder",
  "action": "Climb-down",
  "dest_min_x": 3017,
  "dest_max_x": 3022,
  "dest_min_y": 9737,
  "dest_max_y": 9741,
  "dest_plane": 0,
  "orig_min_x": 3017,
  "orig_max_x": 3021,
  "orig_min_y": 3337,
  "orig_max_y": 3341,
  "orig_plane": 0,
  "search_radius": 20,
  "cost": 200,
  "next_node_type": null,
  "next_node_id": null,
  "requirement_id": 30
}
```

## ifslot_nodes

Represents UI interaction nodes for interface/component/slot combinations and optional destination bounds.

- **Columns**
  - `id`: INTEGER, PRIMARY KEY AUTOINCREMENT
  - `interface_id`: INTEGER, NOT NULL
  - `component_id`: INTEGER, NOT NULL
  - `slot_id`: INTEGER, NULL
  - `click_id`: INTEGER, NOT NULL
  - `dest_min_x`: INTEGER, NULL
  - `dest_max_x`: INTEGER, NULL
  - `dest_min_y`: INTEGER, NULL
  - `dest_max_y`: INTEGER, NULL
  - `dest_plane`: INTEGER, NULL
  - `cost`: INTEGER, NULL
  - `next_node_type`: TEXT, NULL, CHECK IN ('object','npc','ifslot','door','lodestone','item')
  - `next_node_id`: INTEGER, NULL
  - `requirement_id`: INTEGER, NULL, REFERENCES `requirements`(`id`)
- **Constraints**
  - CHECK constraint on `next_node_type` restricting values to 'object','npc','ifslot','door','lodestone','item'
- **Foreign Keys**
  - `requirement_id` REFERENCES `requirements`(`id`)
- **Indexes**
  - None declared

### Example row

```json
{
  "id": 1,
  "interface_id": 1184,
  "component_id": 15,
  "slot_id": -1,
  "click_id": 0,
  "dest_min_x": null,
  "dest_max_x": null,
  "dest_min_y": null,
  "dest_max_y": null,
  "dest_plane": null,
  "cost": 300,
  "next_node_type": "ifslot",
  "next_node_id": 2,
  "requirement_id": null
}
```

## npc_nodes

Represents NPC interaction nodes with support for matching by id or name.

- **Columns**
  - `id`: INTEGER, PRIMARY KEY AUTOINCREMENT
  - `match_type`: TEXT, NOT NULL, CHECK `match_type` IN ('id','name','any')
  - `npc_id`: INTEGER, NULL
  - `npc_name`: TEXT, NULL
  - `action`: TEXT, NULL
  - `dest_min_x`: INTEGER, NULL
  - `dest_max_x`: INTEGER, NULL
  - `dest_min_y`: INTEGER, NULL
  - `dest_max_y`: INTEGER, NULL
  - `dest_plane`: INTEGER, NULL
  - `search_radius`: INTEGER, NOT NULL, DEFAULT 20
  - `cost`: INTEGER, NULL
  - `orig_min_x`: INTEGER, NULL
  - `orig_max_x`: INTEGER, NULL
  - `orig_min_y`: INTEGER, NULL
  - `orig_max_y`: INTEGER, NULL
  - `orig_plane`: INTEGER, NULL
  - `next_node_type`: TEXT, NULL, CHECK IN ('object','npc','ifslot','door','lodestone','item')
  - `next_node_id`: INTEGER, NULL
  - `requirement_id`: INTEGER, NULL, REFERENCES `requirements`(`id`)
- **Constraints**
  - CHECK constraint on `match_type` restricting values to 'id', 'name', 'any'
  - CHECK constraint on `next_node_type` restricting values to 'object','npc','ifslot','door','lodestone','item'
- **Foreign Keys**
  - `requirement_id` REFERENCES `requirements`(`id`)
- **Indexes**
  - None declared

### Example row

```json
{
  "id": 1,
  "match_type": "id",
  "npc_id": 376,
  "npc_name": null,
  "action": "Pay fare",
  "dest_min_x": 2953,
  "dest_max_x": 2960,
  "dest_min_y": 3146,
  "dest_max_y": 3163,
  "dest_plane": 0,
  "search_radius": 20,
  "cost": null,
  "orig_min_x": 3024,
  "orig_max_x": 3029,
  "orig_min_y": 3212,
  "orig_max_y": 3225,
  "orig_plane": 0,
  "next_node_type": "ifslot",
  "next_node_id": 1,
  "requirement_id": 1
}
```

## item_nodes

Represents item usage nodes that may trigger transitions to destination bounds.

- **Columns**
  - `id`: INTEGER, PRIMARY KEY (no AUTOINCREMENT clause; values supplied by data loader)
  - `item_id`: INTEGER, NULL
  - `action`: TEXT, NULL
  - `dest_min_x`: INTEGER, NULL
  - `dest_max_x`: INTEGER, NULL
  - `dest_min_y`: INTEGER, NULL
  - `dest_max_y`: INTEGER, NULL
  - `dest_plane`: INTEGER, NULL
  - `cost`: INTEGER, NULL
  - `next_node_type`: TEXT, NULL, CHECK IN ('object','npc','ifslot','door','lodestone','item')
  - `next_node_id`: INTEGER, NULL
  - `requirement_id`: INTEGER, NULL, REFERENCES `requirements`(`id`)
- **Constraints**
  - CHECK constraint on `next_node_type` restricting values to 'object','npc','ifslot','door','lodestone','item'
- **Foreign Keys**
  - `requirement_id` REFERENCES `requirements`(`id`)
- **Indexes**
  - None declared

### Example row

```json
{
  "id": 1,
  "item_id": 19480,
  "action": "Read",
  "dest_min_x": 3299,
  "dest_max_x": 3309,
  "dest_min_y": 3544,
  "dest_max_y": 3554,
  "dest_plane": 0,
  "cost": null,
  "next_node_type": null,
  "next_node_id": null,
  "requirement_id": 29
}
```

## requirements

Represents requirement expressions referenced by node tables via `requirement_id`.

- **Columns**
  - `id`: INTEGER, PRIMARY KEY AUTOINCREMENT
  - `metaInfo`: TEXT, NULL
  - `key`: TEXT, NULL
  - `value`: INTEGER, NULL
  - `comparison`: TEXT, NULL
- **Foreign Keys**
  - None declared
- **Indexes**
  - None declared

### Example row

```json
{
  "id": 1,
  "metaInfo": "moneypouch",
  "key": "coins",
  "value": 30,
  "comparison": ">="
}
```

## sqlite_sequence

SQLite-managed bookkeeping table tracking AUTOINCREMENT sequences for tables that declare them.

- **Columns**
  - `name`: TEXT, table name owning the AUTOINCREMENT sequence
  - `seq`: INTEGER, current sequence value
- **Notes**
  - Present only because `door_nodes`, `lodestone_nodes`, `object_nodes`, `ifslot_nodes`, and `npc_nodes` use `PRIMARY KEY AUTOINCREMENT`.

### Example row

```json
{
  "name": "door_nodes",
  "seq": 4
}
```

## Notes

- There is no table named exactly `nodes` in `worldReachableTiles.db`. Instead, node data is partitioned across the tables above. See also `tiles` for tile-level data.
- Foreign key references to `requirements(id)` are declared via the `requirement_id` column in each node table.
- NOT NULLs, CHECK constraints, and defaults are captured directly from the schema.
- `sqlite3` reports one automatic index, `sqlite_autoindex_tiles_1`, backing the `tiles` primary key; no manually defined indexes or triggers exist.
- The `requirements` comparison column contains relational operators such as '<', '=', '>', '<=', '>='.
