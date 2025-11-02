use crate::models::Tile;
use serde::Serialize;

pub fn serialize_path(path: &[Tile]) -> Vec<[i32; 3]> {
    path.iter().map(|t| [t.x, t.y, t.plane]).collect()
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Bounds {
    pub min: [i32; 3],
    pub max: [i32; 3],
}

impl Bounds {
    pub fn from_tile(t: Tile) -> Self { Self { min: [t.x, t.y, t.plane], max: [t.x, t.y, t.plane] } }

    pub fn from_min_max_plane(min_x: i32, max_x: i32, min_y: i32, max_y: i32, plane: i32) -> Self {
        Self { min: [min_x, min_y, plane], max: [max_x, max_y, plane] }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MoveAction {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub from: Bounds,
    pub to: Bounds,
    pub cost_ms: i64,
}

pub fn move_action(_from: Tile, to: Tile, cost_ms: i64) -> serde_json::Value {
    let act = MoveAction { kind: "move", from: Bounds::from_tile(_from), to: Bounds::from_tile(to), cost_ms };
    serde_json::to_value(act).expect("serialize move action")
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NodeRef {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LodestoneDbRow {
    pub id: i64,
    pub lodestone: String,
    pub dest: [i32; 3],
    pub cost: i64,
    pub next_node_type: Option<String>,
    pub next_node_id: Option<i64>,
    pub requirement_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LodestoneMetadata {
    pub lodestone: String,
    pub target_lodestone: String,
    pub db_row: LodestoneDbRow,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LodestoneAction {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub from: Bounds,
    pub to: Bounds,
    pub cost_ms: i64,
    pub node: NodeRef,
    pub metadata: LodestoneMetadata,
}

pub fn lodestone_action(
    from: Tile,
    to: Tile,
    cost_ms: i64,
    node_id: i64,
    lodestone_name: &str,
    target_lodestone: &str,
    requirement_id: Option<i64>,
) -> serde_json::Value {
    let node = NodeRef { kind: "lodestone", id: node_id };
    let db_row = LodestoneDbRow {
        id: node_id,
        lodestone: lodestone_name.to_string(),
        dest: [to.x, to.y, to.plane],
        cost: cost_ms as i64,
        next_node_type: None,
        next_node_id: None,
        requirement_id,
    };
    let meta = LodestoneMetadata { lodestone: lodestone_name.to_string(), target_lodestone: target_lodestone.to_string(), db_row };
    let act = LodestoneAction {
        kind: "lodestone",
        from: Bounds::from_tile(from),
        to: Bounds::from_tile(to),
        cost_ms,
        node,
        metadata: meta,
    };
    serde_json::to_value(act).expect("serialize lodestone action")
}

// Placeholders for future action types to match expected shapes
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DoorAction { #[serde(rename = "type")] pub kind: &'static str, pub from: Bounds, pub to: Bounds, pub cost_ms: i64 }
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NpcAction { #[serde(rename = "type")] pub kind: &'static str, pub from: Bounds, pub to: Bounds, pub cost_ms: i64 }
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ObjectAction { #[serde(rename = "type")] pub kind: &'static str, pub from: Bounds, pub to: Bounds, pub cost_ms: i64 }
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ItemAction { #[serde(rename = "type")] pub kind: &'static str, pub from: Bounds, pub to: Bounds, pub cost_ms: i64 }
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IfSlotAction { #[serde(rename = "type")] pub kind: &'static str, pub from: Bounds, pub to: Bounds, pub cost_ms: i64 }

pub fn door_action(from: Tile, to: Tile, cost_ms: i64) -> serde_json::Value { serde_json::to_value(DoorAction{kind:"door",from:Bounds::from_tile(from),to:Bounds::from_tile(to),cost_ms}).unwrap() }
pub fn npc_action(from: Tile, to: Tile, cost_ms: i64) -> serde_json::Value { serde_json::to_value(NpcAction{kind:"npc",from:Bounds::from_tile(from),to:Bounds::from_tile(to),cost_ms}).unwrap() }
pub fn object_action(from: Tile, to: Tile, cost_ms: i64) -> serde_json::Value { serde_json::to_value(ObjectAction{kind:"object",from:Bounds::from_tile(from),to:Bounds::from_tile(to),cost_ms}).unwrap() }
pub fn item_action(from: Tile, to: Tile, cost_ms: i64) -> serde_json::Value { serde_json::to_value(ItemAction{kind:"item",from:Bounds::from_tile(from),to:Bounds::from_tile(to),cost_ms}).unwrap() }
pub fn ifslot_action(from: Tile, to: Tile, cost_ms: i64) -> serde_json::Value { serde_json::to_value(IfSlotAction{kind:"ifslot",from:Bounds::from_tile(from),to:Bounds::from_tile(to),cost_ms}).unwrap() }
