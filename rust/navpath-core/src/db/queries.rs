pub const TILE_BY_COORD: &str = "SELECT x, y, plane, tiledata, allowed_directions, blocked_directions FROM tiles WHERE x = ?1 AND y = ?2 AND plane = ?3";
pub const TILES_BY_PLANE: &str = "SELECT x, y, plane, tiledata, allowed_directions, blocked_directions FROM tiles WHERE plane = ?1 ORDER BY y ASC, x ASC";

pub const ALL_DOORS: &str = "SELECT id, direction, \
    tile_inside_x, tile_inside_y, tile_inside_plane, \
    tile_outside_x, tile_outside_y, tile_outside_plane, \
    location_open_x, location_open_y, location_open_plane, \
    location_closed_x, location_closed_y, location_closed_plane, \
    real_id_open, real_id_closed, cost, open_action, next_node_type, next_node_id, requirement_id \
    FROM door_nodes";

pub const DOOR_BY_TILE: &str = "SELECT id, direction, \
    tile_inside_x, tile_inside_y, tile_inside_plane, \
    tile_outside_x, tile_outside_y, tile_outside_plane, \
    location_open_x, location_open_y, location_open_plane, \
    location_closed_x, location_closed_y, location_closed_plane, \
    real_id_open, real_id_closed, cost, open_action, next_node_type, next_node_id, requirement_id \
    FROM door_nodes WHERE (tile_inside_x = ?1 AND tile_inside_y = ?2 AND tile_inside_plane = ?3) \
    OR (tile_outside_x = ?1 AND tile_outside_y = ?2 AND tile_outside_plane = ?3)";

pub const REQUIREMENT_BY_ID: &str = "SELECT id, metaInfo, key, value, comparison FROM requirements WHERE id = ?1";

pub const DOOR_BY_ID: &str = "SELECT id, direction, \
    tile_inside_x, tile_inside_y, tile_inside_plane, \
    tile_outside_x, tile_outside_y, tile_outside_plane, \
    location_open_x, location_open_y, location_open_plane, \
    location_closed_x, location_closed_y, location_closed_plane, \
    real_id_open, real_id_closed, cost, open_action, next_node_type, next_node_id, requirement_id \
    FROM door_nodes WHERE id = ?1";

pub const OBJECT_BY_ID: &str = "SELECT id, match_type, object_id, object_name, action, \
    dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, \
    orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane, search_radius, \
    cost, next_node_type, next_node_id, requirement_id \
    FROM object_nodes WHERE id = ?1";

// All object nodes
pub const ALL_OBJECTS: &str = "SELECT id, match_type, object_id, object_name, action, \
    dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, \
    orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane, search_radius, \
    cost, next_node_type, next_node_id, requirement_id \
    FROM object_nodes";

// Object nodes touching origin tile (origin bounds include tile or origin unspecified)
pub const OBJECT_BY_ORIGIN_TILE: &str = "SELECT id, match_type, object_id, object_name, action, \
    dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, \
    orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane, search_radius, \
    cost, next_node_type, next_node_id, requirement_id \
    FROM object_nodes \
    WHERE (orig_min_x IS NULL OR orig_max_x IS NULL OR (?1 BETWEEN orig_min_x AND orig_max_x)) \
      AND (orig_min_y IS NULL OR orig_max_y IS NULL OR (?2 BETWEEN orig_min_y AND orig_max_y)) \
      AND (orig_plane IS NULL OR orig_plane = ?3)";

// Lodestone nodes
pub const ALL_LODESTONES: &str = "SELECT id, lodestone, dest_x, dest_y, dest_plane, cost, next_node_type, next_node_id, requirement_id FROM lodestone_nodes";
pub const LODESTONE_BY_ID: &str = "SELECT id, lodestone, dest_x, dest_y, dest_plane, cost, next_node_type, next_node_id, requirement_id FROM lodestone_nodes WHERE id = ?1";

// Ifslot nodes
pub const ALL_IFSLOTS: &str = "SELECT id, interface_id, component_id, slot_id, click_id, dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, cost, next_node_type, next_node_id, requirement_id FROM ifslot_nodes";
pub const IFSLOT_BY_ID: &str = "SELECT id, interface_id, component_id, slot_id, click_id, dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, cost, next_node_type, next_node_id, requirement_id FROM ifslot_nodes WHERE id = ?1";

// NPC nodes
pub const ALL_NPCS: &str = "SELECT id, match_type, npc_id, npc_name, action, dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane, search_radius, cost, next_node_type, next_node_id, requirement_id FROM npc_nodes";
pub const NPC_BY_ID: &str = "SELECT id, match_type, npc_id, npc_name, action, dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane, search_radius, cost, next_node_type, next_node_id, requirement_id FROM npc_nodes WHERE id = ?1";
pub const NPC_BY_ORIGIN_TILE: &str = "SELECT id, match_type, npc_id, npc_name, action, dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane, search_radius, cost, next_node_type, next_node_id, requirement_id FROM npc_nodes \
    WHERE (orig_min_x IS NULL OR orig_max_x IS NULL OR (?1 BETWEEN orig_min_x AND orig_max_x)) \
      AND (orig_min_y IS NULL OR orig_max_y IS NULL OR (?2 BETWEEN orig_min_y AND orig_max_y)) \
      AND (orig_plane IS NULL OR orig_plane = ?3)";

// Item nodes
pub const ALL_ITEMS: &str = "SELECT id, item_id, action, dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, cost, next_node_type, next_node_id, requirement_id FROM item_nodes";
pub const ITEM_BY_ID: &str = "SELECT id, item_id, action, dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane, cost, next_node_type, next_node_id, requirement_id FROM item_nodes WHERE id = ?1";
