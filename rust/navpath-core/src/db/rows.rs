use crate::models::Tile;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TileRow {
    pub x: i32,
    pub y: i32,
    pub plane: i32,
    pub tiledata: Option<i64>,
    pub allowed_directions: Option<String>,
    pub blocked_directions: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementRow {
    pub id: i32,
    pub metaInfo: Option<String>,
    pub key: Option<String>,
    pub value: Option<i64>,
    pub comparison: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoorNodeRow {
    pub id: i32,
    pub direction: Option<String>,
    pub tile_inside: Tile,
    pub tile_outside: Tile,
    pub location_open: Tile,
    pub location_closed: Tile,
    pub real_id_open: i32,
    pub real_id_closed: i32,
    pub cost: Option<i64>,
    pub open_action: Option<String>,
    pub next_node_type: Option<String>,
    pub next_node_id: Option<i32>,
    pub requirement_id: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LodestoneNodeRow {
    pub id: i32,
    pub lodestone: String,
    pub dest: Tile,
    pub cost: Option<i64>,
    pub next_node_type: Option<String>,
    pub next_node_id: Option<i32>,
    pub requirement_id: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectNodeRow {
    pub id: i32,
    pub match_type: String,
    pub object_id: Option<i32>,
    pub object_name: Option<String>,
    pub action: Option<String>,
    pub dest_min_x: Option<i32>,
    pub dest_max_x: Option<i32>,
    pub dest_min_y: Option<i32>,
    pub dest_max_y: Option<i32>,
    pub dest_plane: Option<i32>,
    pub orig_min_x: Option<i32>,
    pub orig_max_x: Option<i32>,
    pub orig_min_y: Option<i32>,
    pub orig_max_y: Option<i32>,
    pub orig_plane: Option<i32>,
    pub search_radius: i32,
    pub cost: Option<i64>,
    pub next_node_type: Option<String>,
    pub next_node_id: Option<i32>,
    pub requirement_id: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IfslotNodeRow {
    pub id: i32,
    pub interface_id: i32,
    pub component_id: i32,
    pub slot_id: Option<i32>,
    pub click_id: i32,
    pub dest_min_x: Option<i32>,
    pub dest_max_x: Option<i32>,
    pub dest_min_y: Option<i32>,
    pub dest_max_y: Option<i32>,
    pub dest_plane: Option<i32>,
    pub cost: Option<i64>,
    pub next_node_type: Option<String>,
    pub next_node_id: Option<i32>,
    pub requirement_id: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NpcNodeRow {
    pub id: i32,
    pub match_type: String,
    pub npc_id: Option<i32>,
    pub npc_name: Option<String>,
    pub action: Option<String>,
    pub dest_min_x: Option<i32>,
    pub dest_max_x: Option<i32>,
    pub dest_min_y: Option<i32>,
    pub dest_max_y: Option<i32>,
    pub dest_plane: Option<i32>,
    pub orig_min_x: Option<i32>,
    pub orig_max_x: Option<i32>,
    pub orig_min_y: Option<i32>,
    pub orig_max_y: Option<i32>,
    pub orig_plane: Option<i32>,
    pub search_radius: i32,
    pub cost: Option<i64>,
    pub next_node_type: Option<String>,
    pub next_node_id: Option<i32>,
    pub requirement_id: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemNodeRow {
    pub id: i32,
    pub item_id: Option<i32>,
    pub action: Option<String>,
    pub dest_min_x: Option<i32>,
    pub dest_max_x: Option<i32>,
    pub dest_min_y: Option<i32>,
    pub dest_max_y: Option<i32>,
    pub dest_plane: Option<i32>,
    pub cost: Option<i64>,
    pub next_node_type: Option<String>,
    pub next_node_id: Option<i32>,
    pub requirement_id: Option<i32>,
}

/// Dynamic node row wrapper used by chain resolver and generic DB fetchers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeRow {
    Door(DoorNodeRow),
    Lodestone(LodestoneNodeRow),
    Object(ObjectNodeRow),
    Ifslot(IfslotNodeRow),
    Npc(NpcNodeRow),
    Item(ItemNodeRow),
}
