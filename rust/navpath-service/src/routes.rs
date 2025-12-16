use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{engine_adapter, AppState, SnapshotState};

// Helper function to find the nearest reachable tile to a given coordinate
    fn find_nearest_tile(_snap: &navpath_core::Snapshot, coord_index: &std::collections::HashMap<(i32, i32, i32), u32>, target_x: i32, target_y: i32, target_plane: i32) -> u32 {
        let mut nearest_id = 0;
        let mut min_distance_sq: i64 = i64::MAX;

        // First, try to find tiles on the same plane
        for (&(x, y, plane), &id) in coord_index.iter() {
            if plane == target_plane {
                let dx = (x - target_x) as i64;
                let dy = (y - target_y) as i64;
                let dist_sq = dx * dx + dy * dy;
                if dist_sq < min_distance_sq {
                    min_distance_sq = dist_sq;
                    nearest_id = id;
                }
            }
        }

        // If no tiles found on the same plane, find the closest tile on any plane
        if min_distance_sq == i64::MAX {
            for (&(x, y, plane), &id) in coord_index.iter() {
                let dx = (x - target_x) as i64;
                let dy = (y - target_y) as i64;
                let plane_diff = (plane - target_plane) as i64;
                let plane_penalty = plane_diff * plane_diff * 10_000_i64; // Heavy penalty for plane changes
                let dist_sq = dx * dx + dy * dy + plane_penalty;
                if dist_sq < min_distance_sq {
                    min_distance_sq = dist_sq;
                    nearest_id = id;
                }
            }
        }

        nearest_id
    }

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

#[derive(Debug, Deserialize, Default)]
pub struct RouteOptions {
    #[serde(default)]
    pub return_geometry: bool,
    #[serde(default)]
    pub only_actions: bool,
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
            
            // Handle start coordinate that doesn't exist - find nearest reachable tile
            let (sid, used_virtual_start) = if let Some(&sid) = idx.get(&key_s) {
                (sid, false)
            } else {
                // Start coordinate doesn't exist, find nearest reachable tile
                let nearest_id = find_nearest_tile(snap, idx, s.wx, s.wy, s.plane);
                info!(
                    start_x = s.wx, start_y = s.wy, start_plane = s.plane,
                    nearest_id = nearest_id,
                    "start coordinate not found, using nearest reachable tile"
                );
                (nearest_id, true)
            };
            (sid, gid, used_virtual_start)
        }
        _ => {
            return Err((StatusCode::BAD_REQUEST, "missing start/goal; provide start_id/goal_id or start/goal with {wx,wy,plane}".into()));
        }
    };
    if sid >= counts.nodes || gid >= counts.nodes {
        return Err((StatusCode::BAD_REQUEST, "start_id/goal_id out of range".into()));
    }

    // Build client requirements and run route with eligibility gating
    let client_reqs: Vec<(String, serde_json::Value)> = req
        .profile
        .requirements
        .iter()
        .map(|kv| (kv.key.clone(), kv.value.clone()))
        .collect();

    let quick_tele = req_has_quick_tele(&req.profile.requirements);
    
    // Offload search to blocking thread pool
    let snap_arc = snap.clone();
    let neighbors_arc = neighbors.clone();
    let globals_arc = globals.clone();
    
    let res = tokio::task::spawn_blocking(move || {
        engine_adapter::run_route_with_requirements(snap_arc, neighbors_arc, globals_arc, sid, gid, &client_reqs)
    }).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    
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
        if let Some(&idx) = cur.macro_lookup.get(&(0, 0)) {
             if let Some(bytes) = snap.macro_meta_at(idx as usize) {
                 if let Ok(val) = serde_json::from_slice::<serde_json::Value>(bytes) {
                     if let Some(arr) = val.get("global").and_then(|v| v.as_array()) {
                         for g in arr {
                             if let Some(dst) = g.get("dst").and_then(|v| v.as_u64()) {
                                 let dst = dst as u32;
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
                                 global_cost.insert(dst, cost);
                                 global_meta.insert(dst, g.clone());
                             }
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
                    let (actual_x, actual_y, actual_p) = coord(sid);
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
                
                if let Some(&idx) = cur.macro_lookup.get(&(u, v)) {
                    let idx = idx as usize;
                    let mut cost_ms = snap.macro_w().get(idx).unwrap_or(0.0);
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
            
            // If we had a virtual start, add the action with the correct type determined from the first real action
            if let Some(mut virtual_action) = virtual_start_action {
                if let Some(first_action) = acts.first() {
                    // Copy the type and metadata from the first actual action (which should be the teleport)
                    if let Some(action_type) = first_action.get("type").and_then(|v| v.as_str()) {
                        virtual_action["type"] = serde_json::Value::String(action_type.to_string());
                        // Also copy the metadata if it exists, but keep our reason
                        if let Some(metadata) = first_action.get("metadata") {
                            if let Some(obj) = virtual_action.get_mut("metadata").and_then(|v| v.as_object_mut()) {
                                // Merge the first action's metadata, but preserve our reason
                                for (key, value) in metadata.as_object().unwrap_or(&serde_json::Map::new()) {
                                    if key != "reason" {
                                        obj.insert(key.to_string(), value.clone());
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // Fallback: no actions found, use generic teleport
                    virtual_action["type"] = serde_json::Value::String("teleport".to_string());
                }
                acts.insert(0, virtual_action);
            }
            
            actions = Some(acts);
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
