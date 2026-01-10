use std::sync::Arc;

use axum::{extract::{Query, State}, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use navpath_core::eligibility::{build_mask_from_u32, ClientValue};

use crate::{engine_adapter, AppState, SnapshotState};

#[derive(Debug, Deserialize)]
pub struct RequirementKV {
    pub key: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Deserialize, Default)]
pub struct Profile {
    #[serde(default)]
    pub requirements: Vec<RequirementKV>,
}

fn req_has_quick_tele(reqs: &[RequirementKV]) -> bool {
    for r in reqs {
        if r.key.trim().eq_ignore_ascii_case("hasQuickTele") {
            if r.value.as_i64() == Some(1) || r.value.as_u64() == Some(1) {
                return true;
            }
            if r.value.as_bool() == Some(true) {
                return true;
            }
            if r.value.as_str().map(|s| s.trim()) == Some("1") {
                return true;
            }
        }
    }
    false
}

fn build_req_id_to_tag_index(req_words: &[u32]) -> std::collections::HashMap<u32, usize> {
    let mut map = std::collections::HashMap::new();
    let mut i = 0usize;
    while i + 3 < req_words.len() {
        map.insert(req_words[i], i / 4);
        i += 4;
    }
    map
}

fn macro_edge_allowed_by_profile(
    snap: &navpath_core::Snapshot,
    macro_idx: usize,
    req_id_to_tag_idx: &std::collections::HashMap<u32, usize>,
    mask: &navpath_core::eligibility::EligibilityMask,
) -> bool {
    let Some(bytes) = snap.macro_meta_at(macro_idx) else { return true; };
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(bytes) else { return true; };
    let Some(arr) = val.get("requirements").and_then(|v| v.as_array()) else { return true; };
    for ridv in arr {
        let Some(rid) = ridv.as_u64() else { continue; };
        let Some(&tag_idx) = req_id_to_tag_idx.get(&(rid as u32)) else { return false; };
        if !mask.is_satisfied(tag_idx) {
            return false;
        }
    }
    true
}

#[derive(Debug, Deserialize, Default)]
pub struct RouteOptions {
    #[serde(default)]
    pub return_geometry: bool,
    #[serde(default)]
    pub only_actions: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SurgeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub charges: u32,
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DiveConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub available_in_ms: f64,
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: f64,
}

fn default_cooldown_ms() -> f64 {
    20400.0
}

impl Default for SurgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            charges: 0,
            cooldown_ms: default_cooldown_ms(),
        }
    }
}

impl Default for DiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            available_in_ms: 0.0,
            cooldown_ms: default_cooldown_ms(),
        }
    }
}

/// Minimum tiles required to use surge (not worth cooldown for less)
const MIN_SURGE_TILES: usize = 5;
/// Minimum tiles required to use dive (very aggressive - use whenever possible)
const MIN_DIVE_TILES: usize = 2;
/// Maximum tiles surge/dive can cover
const MAX_ABILITY_TILES: usize = 10;
/// Minimum tiles to walk in surge direction before using surge (to establish facing)
const MIN_WALK_BEFORE_SURGE: usize = 3;

/// Represents direction for surge (must be straight line)
#[derive(Debug, Clone, Copy, PartialEq)]
enum Direction {
    North,      // y increases
    South,      // y decreases
    East,       // x increases
    West,       // x decreases
    NorthEast,  // x+, y+
    NorthWest,  // x-, y+
    SouthEast,  // x+, y-
    SouthWest,  // x-, y-
}

impl Direction {
    fn from_delta(dx: i32, dy: i32) -> Option<Self> {
        match (dx.signum(), dy.signum()) {
            (0, 1) => Some(Direction::North),
            (0, -1) => Some(Direction::South),
            (1, 0) => Some(Direction::East),
            (-1, 0) => Some(Direction::West),
            (1, 1) => Some(Direction::NorthEast),
            (-1, 1) => Some(Direction::NorthWest),
            (1, -1) => Some(Direction::SouthEast),
            (-1, -1) => Some(Direction::SouthWest),
            _ => None,
        }
    }
}

/// Extract coordinates from a move action
fn extract_move_coords(action: &serde_json::Value) -> Option<(i32, i32, i32)> {
    let to = action.get("to")?.as_array()?;
    if to.len() != 3 {
        return None;
    }
    Some((
        to[0].as_i64()? as i32,
        to[1].as_i64()? as i32,
        to[2].as_i64()? as i32,
    ))
}

/// Find the first available surge charge and return its index
fn find_available_surge_charge(charges: &[f64], elapsed_ms: f64) -> Option<usize> {
    charges.iter().position(|&available_at| available_at <= elapsed_ms)
}

/// Calculate straight-line distance between two points
fn straight_line_distance(x1: i32, y1: i32, x2: i32, y2: i32) -> f64 {
    let dx = (x2 - x1) as f64;
    let dy = (y2 - y1) as f64;
    (dx * dx + dy * dy).sqrt()
}

/// Check if a dive path is valid (straight-line distance within range)
/// Dive teleports directly to target, so we only care about straight-line distance
fn is_valid_dive_path(start: (i32, i32, i32), end: (i32, i32, i32), _tiles_walked: usize) -> bool {
    if start.2 != end.2 {
        return false; // Different planes
    }
    let straight_dist = straight_line_distance(start.0, start.1, end.0, end.1);
    // Dive can reach up to 10 tiles in a straight line regardless of walked path
    straight_dist <= (MAX_ABILITY_TILES as f64) + 0.5
}

/// Optimize actions by inserting surge and dive abilities
fn optimize_with_surge_dive(
    actions: Vec<serde_json::Value>,
    surge_config: &SurgeConfig,
    dive_config: &DiveConfig,
) -> Vec<serde_json::Value> {
    // If neither ability is enabled, return as-is
    if !surge_config.enabled && !dive_config.enabled {
        return actions;
    }

    // Track cooldowns: each charge has its own "available_at" time (all start at 0)
    let mut surge_charges: Vec<f64> = vec![0.0; surge_config.charges as usize];
    let mut dive_available_at: f64 = dive_config.available_in_ms;

    let mut result: Vec<serde_json::Value> = Vec::with_capacity(actions.len());
    let mut elapsed_ms: f64 = 0.0;
    let mut i = 0;

    while i < actions.len() {
        let action = &actions[i];
        let action_type = action.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // Only process sequences of "move" actions
        if action_type != "move" {
            // Add non-move action and accumulate its cost
            let cost = action.get("cost_ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
            elapsed_ms += cost;
            result.push(action.clone());
            i += 1;
            continue;
        }

        // We have a move action - look for a sequence we can optimize
        let mut move_sequence: Vec<(usize, i32, i32, i32, f64)> = Vec::new(); // (index, x, y, plane, cost)

        // Get the starting position from the previous action or the first move
        let start_pos = if let Some(prev) = result.last() {
            // Try to get "to" coords from previous action
            if let Some(to) = prev.get("to") {
                if let Some(arr) = to.as_array() {
                    if arr.len() == 3 {
                        Some((
                            arr[0].as_i64().unwrap_or(0) as i32,
                            arr[1].as_i64().unwrap_or(0) as i32,
                            arr[2].as_i64().unwrap_or(0) as i32,
                        ))
                    } else {
                        None
                    }
                } else if let Some(min) = to.get("min").and_then(|v| v.as_array()) {
                    if min.len() == 3 {
                        Some((
                            min[0].as_i64().unwrap_or(0) as i32,
                            min[1].as_i64().unwrap_or(0) as i32,
                            min[2].as_i64().unwrap_or(0) as i32,
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // Collect consecutive move actions
        let mut j = i;
        while j < actions.len() {
            let act = &actions[j];
            if act.get("type").and_then(|v| v.as_str()) != Some("move") {
                break;
            }
            if let Some((x, y, p)) = extract_move_coords(act) {
                let cost = act.get("cost_ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
                move_sequence.push((j, x, y, p, cost));
                j += 1;
            } else {
                break;
            }
        }

        if move_sequence.is_empty() {
            result.push(action.clone());
            i += 1;
            continue;
        }

        // Now try to find surge/dive opportunities within this move sequence
        let mut seq_idx = 0;
        while seq_idx < move_sequence.len() {
            // Determine current position
            let mut current_pos = if seq_idx == 0 {
                start_pos.unwrap_or_else(|| {
                    let (_, x, y, p, _) = move_sequence[0];
                    (x, y, p) // fallback: use first move destination (not ideal but handles edge case)
                })
            } else {
                let (_, x, y, p, _) = move_sequence[seq_idx - 1];
                (x, y, p)
            };

            let mut dive_used = false;
            let mut surge_used = false;

            // Try dive FIRST - it has no facing requirement, can be used anytime when off cooldown
            let remaining_tiles = move_sequence.len() - seq_idx;
            if remaining_tiles >= MIN_DIVE_TILES && dive_config.enabled && dive_available_at <= elapsed_ms {
                let mut best_dive_count = 0;

                for dive_count in (MIN_DIVE_TILES..=MAX_ABILITY_TILES.min(remaining_tiles)).rev() {
                    let end_idx = seq_idx + dive_count - 1;
                    let (_, end_x, end_y, end_p, _) = move_sequence[end_idx];
                    let end_pos = (end_x, end_y, end_p);

                    if is_valid_dive_path(current_pos, end_pos, dive_count) {
                        best_dive_count = dive_count;
                        break;
                    }
                }

                if best_dive_count >= MIN_DIVE_TILES {
                    let end_idx = seq_idx + best_dive_count - 1;
                    let (_, end_x, end_y, end_p, _) = move_sequence[end_idx];

                    result.push(serde_json::json!({
                        "type": "dive",
                        "from": [current_pos.0, current_pos.1, current_pos.2],
                        "to": [end_x, end_y, end_p],
                        "cost_ms": 0,
                        "tiles_covered": best_dive_count
                    }));

                    dive_available_at = elapsed_ms + dive_config.cooldown_ms;
                    seq_idx = end_idx + 1;
                    dive_used = true;

                    // Update current_pos after dive
                    current_pos = (end_x, end_y, end_p);
                }
            }

            // Try surge (requires facing direction from prior moves)
            let remaining_tiles = move_sequence.len() - seq_idx;
            if remaining_tiles >= MIN_SURGE_TILES && surge_config.enabled && !surge_charges.is_empty() {
                if let Some(charge_idx) = find_available_surge_charge(&surge_charges, elapsed_ms) {
                    let mut best_surge_count = 0;
                    let mut best_surge_dir: Option<Direction> = None;

                    for tiles in (MIN_SURGE_TILES..=MAX_ABILITY_TILES.min(remaining_tiles)).rev() {
                        let end_idx = seq_idx + tiles - 1;
                        let (_, end_x, end_y, end_p, _) = move_sequence[end_idx];

                        if end_p != current_pos.2 {
                            continue;
                        }

                        let straight_dist = straight_line_distance(current_pos.0, current_pos.1, end_x, end_y);

                        if (tiles as f64) <= straight_dist + 2.0 {
                            let dx = end_x - current_pos.0;
                            let dy = end_y - current_pos.1;
                            if let Some(dir) = Direction::from_delta(dx, dy) {
                                best_surge_count = tiles;
                                best_surge_dir = Some(dir);
                                break;
                            }
                        }
                    }

                    // Check prior moves for facing direction
                    let mut prior_moves_in_direction = 0;
                    if let Some(surge_dir) = best_surge_dir {
                        let result_len = result.len();
                        for idx in (0..result_len).rev() {
                            let prev_action = &result[idx];
                            if prev_action.get("type").and_then(|v| v.as_str()) != Some("move") {
                                break;
                            }
                            let prev_to = match extract_move_coords(prev_action) {
                                Some(c) => c,
                                None => break,
                            };
                            let prev_from = if idx > 0 {
                                let before = &result[idx - 1];
                                if let Some(to) = before.get("to") {
                                    if let Some(arr) = to.as_array() {
                                        if arr.len() == 3 {
                                            (arr[0].as_i64().unwrap_or(0) as i32,
                                             arr[1].as_i64().unwrap_or(0) as i32,
                                             arr[2].as_i64().unwrap_or(0) as i32)
                                        } else { break; }
                                    } else if let Some(min) = to.get("min").and_then(|v| v.as_array()) {
                                        if min.len() == 3 {
                                            (min[0].as_i64().unwrap_or(0) as i32,
                                             min[1].as_i64().unwrap_or(0) as i32,
                                             min[2].as_i64().unwrap_or(0) as i32)
                                        } else { break; }
                                    } else { break; }
                                } else { break; }
                            } else { break; };

                            let dx = prev_to.0 - prev_from.0;
                            let dy = prev_to.1 - prev_from.1;
                            let move_dir = Direction::from_delta(dx, dy);

                            if move_dir == Some(surge_dir) {
                                prior_moves_in_direction += 1;
                            } else {
                                break;
                            }

                            if prior_moves_in_direction >= MIN_WALK_BEFORE_SURGE {
                                break;
                            }
                        }
                    }

                    if best_surge_count >= MIN_SURGE_TILES && prior_moves_in_direction >= MIN_WALK_BEFORE_SURGE {
                        let end_idx = seq_idx + best_surge_count - 1;
                        let (_, end_x, end_y, end_p, _) = move_sequence[end_idx];

                        result.push(serde_json::json!({
                            "type": "surge",
                            "from": [current_pos.0, current_pos.1, current_pos.2],
                            "to": [end_x, end_y, end_p],
                            "cost_ms": 0,
                            "tiles_covered": best_surge_count
                        }));

                        surge_charges[charge_idx] = elapsed_ms + surge_config.cooldown_ms;
                        seq_idx = end_idx + 1;
                        surge_used = true;
                    }
                }
            }

            // If neither ability was used, walk one tile
            if !dive_used && !surge_used {
                let (orig_idx, _, _, _, cost) = move_sequence[seq_idx];
                elapsed_ms += cost;
                result.push(actions[orig_idx].clone());
                seq_idx += 1;
            }
        }

        i = j; // Skip past all the moves we processed
    }

    result
}

#[derive(Debug, Deserialize, Default)]
pub struct NodeTile { pub wx: i32, pub wy: i32, pub plane: i32 }

#[derive(Debug, Deserialize)]
pub struct RouteRequest {
    // Back-compat: allow direct ids
    #[serde(default)] pub start_id: Option<u32>,
    #[serde(default)] pub goal_id: Option<u32>,
    // Spec format: coordinates
    #[serde(default)] pub start: Option<NodeTile>,
    #[serde(default)] pub goal: Option<NodeTile>,
    #[serde(default)] pub profile: Profile,
    #[serde(default)] pub options: RouteOptions,
    // Surge and Dive abilities
    #[serde(default)] pub surge: SurgeConfig,
    #[serde(default)] pub dive: DiveConfig,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub version: u32,
    pub snapshot_hash: Option<String>,
    pub loaded_at: u64,
    pub counts: Option<Counts>,
}

#[derive(Debug, Serialize)]
pub struct Counts {
    pub nodes: u32,
    pub walk_edges: u32,
    pub macro_edges: u32,
    pub req_tags: u32,
    pub landmarks: u32,
}

#[derive(Debug, Serialize)]
pub struct RouteResponse {
    pub found: bool,
    pub cost: f32,
    pub path: Vec<u32>,
    pub length_tiles: usize,
    pub duration_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")] pub actions: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub geometry: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
pub struct TileExistsQuery {
    pub x: i32,
    pub y: i32,
    pub plane: i32,
}

#[derive(Debug, Serialize)]
pub struct TileExistsResponse {
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub walk_mask: Option<u8>,
}

pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let cur = state.current.load();
    let counts = cur.snapshot.as_ref().map(|s| s.counts()).map(|c| Counts {
        nodes: c.nodes,
        walk_edges: c.walk_edges,
        macro_edges: c.macro_edges,
        req_tags: c.req_tags,
        landmarks: c.landmarks,
    });
    let version = cur.snapshot.as_ref().map(|s| s.manifest().version).unwrap_or(0);
    Json(HealthResponse {
        version,
        snapshot_hash: cur.snapshot_hash_hex.clone(),
        loaded_at: cur.loaded_at_unix,
        counts,
    })
}

pub async fn tile_exists(
    State(state): State<AppState>,
    Query(params): Query<TileExistsQuery>,
) -> Result<Json<TileExistsResponse>, (StatusCode, String)> {
    let cur = state.current.load();

    let Some(coord_index) = cur.coord_index.as_ref() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "coordinate index not loaded".into(),
        ));
    };

    match coord_index.get(&(params.x, params.y, params.plane)) {
        Some(&node_id) => {
            Ok(Json(TileExistsResponse {
                exists: true,
                node_id: Some(node_id),
                walk_mask: None, // tiles.bin not currently loaded into state
            }))
        }
        None => Ok(Json(TileExistsResponse {
            exists: false,
            node_id: None,
            walk_mask: None,
        })),
    }
}

pub async fn route(State(state): State<AppState>, Json(req): Json<RouteRequest>) -> Result<Json<RouteResponse>, (StatusCode, String)> {
    let start = std::time::Instant::now();
    let cur = state.current.load();
    let Some(snap) = cur.snapshot.as_ref() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "snapshot not loaded".into()));
    };
    let Some(neighbors) = cur.neighbors.as_ref() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "neighbors not loaded".into()));
    };
    let globals = cur.globals.clone();
    let counts = snap.counts();

    // Resolve node ids
    let (sid, gid, used_virtual_start) = match (req.start_id, req.goal_id, req.start.as_ref(), req.goal.as_ref()) {
        (Some(sid), Some(gid), _, _) => (sid, gid, false),
        (_, _, Some(s), Some(g)) => {
            let Some(idx) = cur.coord_index.as_ref() else {
                return Err((StatusCode::SERVICE_UNAVAILABLE, "snapshot missing coordinate index; reload a v3 snapshot".into()));
            };
            let key_s = (s.wx, s.wy, s.plane);
            let key_g = (g.wx, g.wy, g.plane);
            let Some(&gid) = idx.get(&key_g) else { return Err((StatusCode::BAD_REQUEST, "goal tile not found in snapshot".into())); };
            
            // If the start coordinate isn't present in the snapshot, do NOT snap to a nearby tile.
            // Treat it as out-of-graph and force entry via a global teleport.
            let (sid, used_virtual_start) = if let Some(&sid) = idx.get(&key_s) {
                (sid, false)
            } else {
                info!(
                    start_x = s.wx, start_y = s.wy, start_plane = s.plane,
                    "start coordinate not found in snapshot; will force global teleport entry"
                );
                (0, true)
            };
            (sid, gid, used_virtual_start)
        }
        _ => {
            return Err((StatusCode::BAD_REQUEST, "missing start/goal; provide start_id/goal_id or start/goal with {wx,wy,plane}".into()));
        }
    };
    if (!used_virtual_start && sid >= counts.nodes) || gid >= counts.nodes {
        return Err((StatusCode::BAD_REQUEST, "start_id/goal_id out of range".into()));
    }

    let client_reqs: Vec<(String, serde_json::Value)> = req
        .profile
        .requirements
        .iter()
        .map(|kv| (kv.key.clone(), kv.value.clone()))
        .collect();

    let quick_tele = req_has_quick_tele(&req.profile.requirements);

    let req_words: Vec<u32> = snap.req_tags().iter().collect();
    let req_id_to_tag_idx = build_req_id_to_tag_index(&req_words);
    let mask = build_mask_from_u32(
        &req_words,
        client_reqs.iter().filter_map(|(k, v)| {
            if let Some(n) = v.as_i64() {
                Some((k.as_str(), ClientValue::Num(n)))
            } else if let Some(u) = v.as_u64() {
                Some((k.as_str(), ClientValue::Num(u as i64)))
            } else if let Some(b) = v.as_bool() {
                Some((k.as_str(), ClientValue::Num(if b { 1 } else { 0 })))
            } else if let Some(s) = v.as_str() {
                let st = s.trim();
                if let Ok(n) = st.parse::<i64>() {
                    Some((k.as_str(), ClientValue::Num(n)))
                } else {
                    Some((k.as_str(), ClientValue::Str(st)))
                }
            } else {
                None
            }
        }),
    );
    
    // Offload search to blocking thread pool
    let snap_arc = snap.clone();
    let neighbors_arc = neighbors.clone();
    let globals_arc = globals.clone();

    let used_virtual_start_for_search = used_virtual_start;
    let res_and_entry = tokio::task::spawn_blocking(move || {
        if used_virtual_start_for_search {
            engine_adapter::run_route_with_requirements_virtual_start(
                snap_arc,
                neighbors_arc,
                globals_arc,
                gid,
                &client_reqs,
            )
        } else {
            (engine_adapter::run_route_with_requirements(snap_arc, neighbors_arc, globals_arc, sid, gid, &client_reqs), None)
        }
    }).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let (res, virtual_entry) = res_and_entry;
    
    let duration_ms = start.elapsed().as_millis();

    // Optionally build actions/geometry
    let mut actions: Option<Vec<serde_json::Value>> = None;
    let mut geometry: Option<Vec<serde_json::Value>> = None;

    if res.found {
        // Helper to fetch coords
        let xs = snap.nodes_x();
        let ys = snap.nodes_y();
        let ps = snap.nodes_plane();
        let coord = |id: u32| -> (i32,i32,i32) {
            let i = id as usize;
            (xs.get(i).unwrap_or(0), ys.get(i).unwrap_or(0), ps.get(i).unwrap_or(0))
        };

        if req.options.return_geometry {
            let mut geom: Vec<serde_json::Value> = Vec::with_capacity(res.path.len());
            for &id in &res.path {
                let (x,y,p) = coord(id);
                geom.push(serde_json::json!([x,y,p]));
            }
            geometry = Some(geom);
        }

        // Parse global teleports encoded under a synthetic 0->0 macro edge
        let mut global_cost: std::collections::HashMap<u32, f32> = std::collections::HashMap::new();
        let mut global_meta: std::collections::HashMap<u32, serde_json::Value> = std::collections::HashMap::new();
        if let Some(idxs) = cur.macro_lookup.get(&(0, 0)) {
            for &idx_u32 in idxs {
                let idx = idx_u32 as usize;
                if let Some(bytes) = snap.macro_meta_at(idx) {
                    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(bytes) {
                        if let Some(arr) = val.get("global").and_then(|v| v.as_array()) {
                            for g in arr {
                                if let Some(dst) = g.get("dst").and_then(|v| v.as_u64()) {
                                    let dst = dst as u32;
                                    let mut allowed = true;
                                    if let Some(reqs) = g.get("requirements").and_then(|v| v.as_array()) {
                                        for ridv in reqs {
                                            let Some(rid) = ridv.as_u64() else { continue; };
                                            let Some(&tag_idx) = req_id_to_tag_idx.get(&(rid as u32)) else {
                                                allowed = false;
                                                break;
                                            };
                                            if !mask.is_satisfied(tag_idx) {
                                                allowed = false;
                                                break;
                                            }
                                        }
                                    }
                                    if !allowed {
                                        continue;
                                    }
                                    let mut cost = g.get("cost_ms").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                                    let kind = g
                                        .get("steps").and_then(|v| v.as_array())
                                        .and_then(|a| a.first())
                                        .and_then(|s| s.get("kind"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    if quick_tele && kind == "lodestone" {
                                        cost = 2400.0;
                                    }
                                    let should_replace = global_cost.get(&dst).map(|c| cost < *c).unwrap_or(true);
                                    if should_replace {
                                        global_cost.insert(dst, cost);
                                        global_meta.insert(dst, g.clone());
                                    }
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }

        if req.options.only_actions || req.options.return_geometry {
            let mut acts: Vec<serde_json::Value> = Vec::with_capacity(res.path.len().saturating_sub(1));
            
            // If we used a virtual start (non-existent start coordinate), we'll need to add the teleport action later
            // after we determine the actual teleport type from the first real action
            let mut virtual_start_action: Option<serde_json::Value> = None;
            if used_virtual_start {
                if let Some(start_coord) = req.start.as_ref() {
                    let entry_id = virtual_entry.unwrap_or(sid);
                    let (actual_x, actual_y, actual_p) = coord(entry_id);
                    virtual_start_action = Some(serde_json::json!({
                        "from": {"min": [start_coord.wx, start_coord.wy, start_coord.plane], "max": [start_coord.wx, start_coord.wy, start_coord.plane]},
                        "to":   {"min": [actual_x, actual_y, actual_p], "max": [actual_x, actual_y, actual_p]},
                        "cost_ms": 0,
                        "metadata": {"reason": "start_coordinate_not_found"}
                    }));
                }
            }
            
            for w in res.path.windows(2) {
                let (u, v) = (w[0], w[1]);
                let (x1,y1,p1) = coord(u);
                let (x2,y2,p2) = coord(v);
                
                if let Some(idxs) = cur.macro_lookup.get(&(u, v)) {
                    let mut chosen: Option<(usize, f32)> = None;
                    for &idx_u32 in idxs {
                        let idx = idx_u32 as usize;
                        if !macro_edge_allowed_by_profile(snap, idx, &req_id_to_tag_idx, &mask) {
                            continue;
                        }
                        let mut cost_ms = snap.macro_w().get(idx).unwrap_or(0.0);
                        let k = snap.macro_kind_first().get(idx).unwrap_or(0);
                        if quick_tele && k == 2 {
                            cost_ms = 2400.0;
                        }
                        if chosen.map_or(true, |(_, best_cost)| cost_ms < best_cost) {
                            chosen = Some((idx, cost_ms));
                        }
                    }
                    let (idx, mut cost_ms) = if let Some(best) = chosen {
                        best
                    } else {
                        let idx = idxs.first().copied().unwrap_or(0) as usize;
                        (idx, snap.macro_w().get(idx).unwrap_or(0.0))
                    };

                    let k = snap.macro_kind_first().get(idx).unwrap_or(0);
                    let kid = snap.macro_id_first().get(idx).unwrap_or(0);
                    let kstr = match k {
                        1 => "door",
                        2 => "lodestone",
                        3 => "npc",
                        4 => "object",
                        5 => "item",
                        6 => "ifslot",
                        _ => "teleport",
                    };
                    if quick_tele && kstr == "lodestone" {
                        cost_ms = 2400.0;
                    }
                    let mut meta = if let Some(bytes) = snap.macro_meta_at(idx) {
                        serde_json::from_slice(bytes).unwrap_or(serde_json::json!({}))
                    } else {
                        serde_json::json!({})
                    };
                    
                    // For doors, compute dynamic approach direction (IN/OUT) using db_row tile_inside/tile_outside
                    if kstr == "door" {
                        // helper to extract [x,y,p] to tuple
                        fn arr_to_tuple(v: &serde_json::Value) -> Option<(i32,i32,i32)> {
                            let a = v.as_array()?;
                            if a.len() != 3 { return None; }
                            let x = a[0].as_i64()? as i32;
                            let y = a[1].as_i64()? as i32;
                            let p = a[2].as_i64()? as i32;
                            Some((x,y,p))
                        }
                        if let Some(db_row) = meta.get("db_row") {
                            let tin = db_row.get("tile_inside").and_then(arr_to_tuple);
                            let tout = db_row.get("tile_outside").and_then(arr_to_tuple);
                            let from = (x1,y1,p1);
                            let to = (x2,y2,p2);
                            let dir = if tout.is_some() && Some(from) == tout { Some("IN") }
                                      else if tin.is_some() && Some(from) == tin { Some("OUT") }
                                      else if tin.is_some() && Some(to) == tin { Some("IN") }
                                      else if tout.is_some() && Some(to) == tout { Some("OUT") }
                                      else { None };
                            if let Some(d) = dir {
                                if let Some(obj) = meta.as_object_mut() {
                                    obj.insert("door_direction".to_string(), serde_json::Value::String(d.to_string()));
                                }
                            }
                        }
                    }
                    // Remove duplicated top-level db_row; it exists inside per-step entries already
                    if let Some(obj) = meta.as_object_mut() {
                        obj.remove("db_row");
                    }
                    acts.push(serde_json::json!({
                        "type": kstr,
                        "from": {"min": [x1,y1,p1], "max": [x1,y1,p1]},
                        "to":   {"min": [x2,y2,p2], "max": [x2,y2,p2]},
                        "cost_ms": cost_ms,
                        "node": {"type": kstr, "id": kid},
                        "metadata": meta
                    }));
                } else {
                    // Walk edge or unknown
                    // Check if it is a valid walk edge by querying neighbor provider
                    let mut is_walk = false;
                    let mut w_cost = 1.0; // Default
                    // This is slightly inefficient to scan walk neighbors but degree is small
                    let (wd, ww) = neighbors.walk.neighbors(u);
                    for (i, &neighbor) in wd.iter().enumerate() {
                        if neighbor == v {
                            is_walk = true;
                            w_cost = ww[i];
                            break;
                        }
                    }
                    
                    if is_walk {
                         acts.push(serde_json::json!({
                            "type": "move",
                            "to":   [x2,y2,p2],
                            "cost_ms": w_cost.round()
                        }));
                    } else if let Some(gc) = global_cost.get(&v).cloned() {
                        let meta = global_meta.get(&v).cloned().unwrap_or(serde_json::json!({}));
                        // Prefer the specific step kind (e.g., "lodestone", "npc") if present in metadata
                        let kstr = meta
                            .get("steps").and_then(|v| v.as_array())
                            .and_then(|a| a.first())
                            .and_then(|s| s.get("kind"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("global_teleport");
                        if quick_tele && kstr == "lodestone" {
                            acts.push(serde_json::json!({
                                "type": kstr,
                                "from": {"min": [x1,y1,p1], "max": [x1,y1,p1]},
                                "to":   {"min": [x2,y2,p2], "max": [x2,y2,p2]},
                                "cost_ms": 2400.0,
                                "metadata": meta
                            }));
                        } else {
                            acts.push(serde_json::json!({
                                "type": kstr,
                                "from": {"min": [x1,y1,p1], "max": [x1,y1,p1]},
                                "to":   {"min": [x2,y2,p2], "max": [x2,y2,p2]},
                                "cost_ms": gc,
                                "metadata": meta
                            }));
                        }
                    } else {
                        // Fallback: unknown edge kind; emit as generic teleport with zero cost
                        acts.push(serde_json::json!({
                            "type": "teleport",
                            "from": {"min": [x1,y1,p1], "max": [x1,y1,p1]},
                            "to":   {"min": [x2,y2,p2], "max": [x2,y2,p2]},
                            "cost_ms": 0
                        }));
                    }
                }
            }
            
            // If we had a virtual start, add a synthetic first action derived from the selected global teleport metadata/cost.
            if let Some(mut virtual_action) = virtual_start_action {
                if let Some(entry_id) = virtual_entry {
                    if let Some(gc) = global_cost.get(&entry_id).cloned() {
                        virtual_action["cost_ms"] = serde_json::Value::Number(serde_json::Number::from_f64(gc as f64).unwrap_or_else(|| serde_json::Number::from(0)));
                    }
                    if let Some(meta) = global_meta.get(&entry_id).cloned() {
                        let kstr = meta
                            .get("steps").and_then(|v| v.as_array())
                            .and_then(|a| a.first())
                            .and_then(|s| s.get("kind"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("global_teleport");
                        virtual_action["type"] = serde_json::Value::String(kstr.to_string());
                        if let Some(obj) = virtual_action.get_mut("metadata").and_then(|v| v.as_object_mut()) {
                            obj.insert("teleport".to_string(), meta);
                        }
                    } else {
                        virtual_action["type"] = serde_json::Value::String("global_teleport".to_string());
                    }
                } else {
                    virtual_action["type"] = serde_json::Value::String("global_teleport".to_string());
                }
                acts.insert(0, virtual_action);
            }

            // Apply surge/dive optimization to the actions
            let optimized_acts = optimize_with_surge_dive(acts, &req.surge, &req.dive);
            actions = Some(optimized_acts);
        }
    }

    let resp = RouteResponse {
        found: res.found,
        cost: res.cost,
        path: res.path.clone(),
        length_tiles: res.path.len(),
        duration_ms,
        actions,
        geometry,
    };
    if let Ok(bytes) = serde_json::to_vec_pretty(&resp) {
        let _ = std::fs::write("/home/query/Dev/navpathService/result.json", bytes);
    }
    info!(
        duration_ms = duration_ms,
        found = res.found,
        cost = res.cost,
        length = res.path.len(),
        "route request completed"
    );
    Ok(Json(resp))
}

#[derive(Debug, Serialize)]
pub struct ReloadResponse { pub reloaded: bool, pub snapshot_hash: Option<String>, pub loaded_at: u64 }

pub async fn reload(State(state): State<AppState>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let cur = state.current.load();
    let path = cur.path.clone();

    match navpath_core::Snapshot::open(&path) {
        Ok(new_snap) => {
            let new_hash = crate::read_tail_hash_hex(&path);
            // Build coord index from newly loaded snapshot
            let idx = Arc::new(crate::build_coord_index(&new_snap));
            // Pre-compute neighbors and globals
            let (neighbors, globals, macro_lookup) = crate::engine_adapter::build_neighbor_provider(&new_snap);
            let new_state = SnapshotState {
                path: path.clone(),
                snapshot: Some(Arc::new(new_snap)),
                neighbors: Some(Arc::new(neighbors)),
                globals: Arc::new(globals),
                macro_lookup: Arc::new(macro_lookup),
                loaded_at_unix: crate::now_unix(),
                snapshot_hash_hex: new_hash.clone(),
                coord_index: Some(idx),
            };
            state.current.store(Arc::new(new_state));
            info!(path=?path, hash=?new_hash, "reloaded snapshot");
            let latest = state.current.load();
            Ok(Json(serde_json::json!({
                "reloaded": true,
                "snapshot_hash": latest.snapshot_hash_hex,
                "loaded_at": latest.loaded_at_unix
            })))
        }
        Err(e) => {
            warn!(error=?e, path=?path, "reload failed; keeping old snapshot");
            Err((StatusCode::CONFLICT, e.to_string()))
        }
    }
}
