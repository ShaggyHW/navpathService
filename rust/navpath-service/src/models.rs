use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Tile {
    pub x: i32,
    pub y: i32,
    pub plane: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequirementKV {
    pub key: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindPathRequest {
    pub start: Tile,
    pub end: Tile,
    #[serde(default)]
    pub requirements: Vec<RequirementKV>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindPathResponse {
    pub path: Vec<Tile>,
    pub actions: Vec<serde_json::Value>,
}
