# tiles.db Node Tables Schema

This document describes the structure of the node-related tables present in `tiles.db`.

- tables inspected: `door_nodes`, `lodestone_nodes`, `object_nodes`, `ifslot_nodes`, `npc_nodes`, `item_nodes`
- inspection source: SQLite PRAGMA and schema introspection run against `tiles.db`

## door_nodes

Represents door interaction nodes with details for both open and closed states.

- **Columns**
  - `id`: INTEGER, PRIMARY KEY AUTOINCREMENT, nullable in schema (implicit non-null as PK)
  - `direction`: TEXT, NOT NULL, CHECK `direction` IN ('IN','OUT')
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
- **Constraints**
  - CHECK constraint on `direction` restricting values to 'IN' or 'OUT'
  - CHECK constraint on `next_node_type` restricting values to 'object','npc','ifslot','door','lodestone','item'
- **Foreign Keys**
  - None declared
- **Indexes**
  - `idx_door_nodes_open` on (`real_id_open`)
  - `idx_door_nodes_closed` on (`real_id_closed`)

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
- **Constraints**
  - CHECK constraint on `next_node_type` restricting values to 'object','npc','ifslot','door','lodestone','item'
- **Foreign Keys**
  - None declared
- **Indexes**
  - None declared

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
  - `search_radius`: INTEGER, NOT NULL, DEFAULT 20
  - `orig_min_x`: INTEGER, NULL
  - `orig_max_x`: INTEGER, NULL
  - `orig_min_y`: INTEGER, NULL
  - `orig_max_y`: INTEGER, NULL
  - `dest_plane`: INTEGER, NULL
  - `orig_plane`: INTEGER, NULL
  - `cost`: INTEGER, NULL
  - `next_node_type`: TEXT, NULL, CHECK IN ('object','npc','ifslot','door','lodestone','item')
  - `next_node_id`: INTEGER, NULL
- **Constraints**
  - CHECK constraint on `match_type` restricting values to 'id', 'name', 'any'
  - CHECK constraint on `next_node_type` restricting values to 'object','npc','ifslot','door','lodestone','item'
- **Foreign Keys**
  - None declared
- **Indexes**
  - `idx_object_nodes_name` on (`object_name`)
  - `idx_object_nodes_id` on (`object_id`)

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
- **Constraints**
  - CHECK constraint on `next_node_type` restricting values to 'object','npc','ifslot','door','lodestone','item'
- **Foreign Keys**
  - None declared
- **Indexes**
  - `idx_ifslot_key` on composite key (`interface_id`, `component_id`, `slot_id`)

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
- **Constraints**
  - CHECK constraint on `match_type` restricting values to 'id', 'name', 'any'
  - CHECK constraint on `next_node_type` restricting values to 'object','npc','ifslot','door','lodestone','item'
- **Foreign Keys**
  - None declared
- **Indexes**
  - `idx_npc_nodes_id` on (`npc_id`)
  - `idx_npc_nodes_name` on (`npc_name`)

## item_nodes

Represents item usage nodes that may trigger transitions to destination bounds.

- **Columns**
  - `id`: INTEGER, PRIMARY KEY
  - `item_id`: INTEGER, NULL
  - `action`: TEXT, NULL
  - `dest_min_x`: INTEGER, NULL
  - `dest_max_x`: INTEGER, NULL
  - `dest_min_y`: INTEGER, NULL
  - `dest_max_y`: INTEGER, NULL
  - `dest_plane`: INTEGER, NULL
  - `next_node_type`: TEXT, NULL, CHECK IN ('object','npc','ifslot','door','lodestone','item')
  - `next_node_id`: INTEGER, NULL
- **Constraints**
  - CHECK constraint on `next_node_type` restricting values to 'object','npc','ifslot','door','lodestone','item'
- **Foreign Keys**
  - None declared
- **Indexes**
  - None declared

## Notes

- There is no table named exactly `nodes` in `tiles.db`. Instead, node data is partitioned across the tables above. See also `tiles` for tile-level data.
- No foreign key constraints are declared in these tables as per SQLite `PRAGMA foreign_key_list`.
- NOT NULLs, CHECK constraints, and defaults are captured directly from the schema.
