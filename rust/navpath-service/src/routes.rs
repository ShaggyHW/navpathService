use std::sync::Arc;
use std::collections::{HashMap, HashSet};

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use itertools::izip;

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
    #[serde(skip_serializing)]
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
    let counts = snap.counts();

    // Resolve node ids
    let (sid, gid) = match (req.start_id, req.goal_id, req.start.as_ref(), req.goal.as_ref()) {
        (Some(sid), Some(gid), _, _) => (sid, gid),
        (_, _, Some(s), Some(g)) => {
            let Some(idx) = cur.coord_index.as_ref() else {
                return Err((StatusCode::SERVICE_UNAVAILABLE, "snapshot missing coordinate index; reload a v3 snapshot".into()));
            };
            let key_s = (s.wx, s.wy, s.plane);
            let key_g = (g.wx, g.wy, g.plane);
            let Some(&sid) = idx.get(&key_s) else { return Err((StatusCode::BAD_REQUEST, "start tile not found in snapshot".into())); };
            let Some(&gid) = idx.get(&key_g) else { return Err((StatusCode::BAD_REQUEST, "goal tile not found in snapshot".into())); };
            (sid, gid)
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
    let res = engine_adapter::run_route_with_requirements(snap.clone(), sid, gid, &client_reqs);
    let duration_ms = start.elapsed().as_millis();

    // Optionally build actions/geometry
    let mut actions: Option<Vec<serde_json::Value>> = None;
    let mut geometry: Option<Vec<serde_json::Value>> = None;

    if res.found {
        // Precompute maps to classify edges
        let mut macro_pairs: HashSet<(u32,u32)> = HashSet::new();
        let mut macro_cost: HashMap<(u32,u32), f32> = HashMap::new();
        let mut macro_kind: HashMap<(u32,u32), u32> = HashMap::new();
        let mut macro_id: HashMap<(u32,u32), u32> = HashMap::new();
        let mut macro_meta: HashMap<(u32,u32), serde_json::Value> = HashMap::new();
        // Build per-edge maps with index for metadata lookup
        let msrc = snap.macro_src();
        let mdst = snap.macro_dst();
        let mw = snap.macro_w();
        let mk = snap.macro_kind_first();
        let mi = snap.macro_id_first();
        for (idx, (s, d, w)) in izip!(msrc.iter(), mdst.iter(), mw.iter()).enumerate() {
            macro_pairs.insert((s, d));
            macro_cost.insert((s, d), w);
            macro_kind.insert((s, d), mk.get(idx).unwrap_or(0));
            macro_id.insert((s, d), mi.get(idx).unwrap_or(0));
            if let Some(bytes) = snap.macro_meta_at(idx) {
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(bytes) {
                    macro_meta.insert((s, d), val);
                }
            }
        }
        let mut walk_cost: HashMap<(u32,u32), f32> = HashMap::new();
        for (s, d, w) in izip!(snap.walk_src().iter(), snap.walk_dst().iter(), snap.walk_w().iter()) {
            walk_cost.insert((s, d), w);
        }

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
        for (idx, (s, d)) in izip!(msrc.iter(), mdst.iter()).enumerate() {
            if s == 0 && d == 0 {
                if let Some(bytes) = snap.macro_meta_at(idx) {
                    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(bytes) {
                        if let Some(arr) = val.get("global").and_then(|v| v.as_array()) {
                            for g in arr {
                                if let Some(dst) = g.get("dst").and_then(|v| v.as_u64()) {
                                    let dst = dst as u32;
                                    let cost = g.get("cost_ms").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                                    global_cost.insert(dst, cost);
                                    global_meta.insert(dst, g.clone());
                                }
                            }
                        }
                    }
                }
                break;
            }
        }

        if req.options.only_actions || req.options.return_geometry {
            let mut acts: Vec<serde_json::Value> = Vec::with_capacity(res.path.len().saturating_sub(1));
            for w in res.path.windows(2) {
                let (u, v) = (w[0], w[1]);
                let (x1,y1,p1) = coord(u);
                let (x2,y2,p2) = coord(v);
                if macro_pairs.contains(&(u, v)) {
                    let cost_ms = macro_cost.get(&(u, v)).cloned().unwrap_or(0.0);
                    let k = macro_kind.get(&(u, v)).cloned().unwrap_or(0);
                    let kid = macro_id.get(&(u, v)).cloned().unwrap_or(0);
                    let kstr = match k {
                        1 => "door",
                        2 => "lodestone",
                        3 => "npc",
                        4 => "object",
                        5 => "item",
                        6 => "ifslot",
                        _ => "teleport",
                    };
                    let mut meta = macro_meta.get(&(u, v)).cloned().unwrap_or(serde_json::json!({}));
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
                } else if let Some(wc) = walk_cost.get(&(u, v)).cloned() {
                    let cost_ms = wc.round();
                    acts.push(serde_json::json!({
                        "type": "move",
                        "to":   [x2,y2,p2],
                        "cost_ms": cost_ms
                    }));
                } else if let Some(gc) = global_cost.get(&v).cloned() {
                    let meta = global_meta.get(&v).cloned().unwrap_or(serde_json::json!({}));
                    acts.push(serde_json::json!({
                        "type": "global_teleport",
                        "from": {"min": [x1,y1,p1], "max": [x1,y1,p1]},
                        "to":   {"min": [x2,y2,p2], "max": [x2,y2,p2]},
                        "cost_ms": gc,
                        "metadata": meta
                    }));
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
    Ok(Json(resp))
}

#[derive(Debug, Serialize)]
pub struct ReloadResponse { pub reloaded: bool, pub snapshot_hash: Option<String>, pub loaded_at: u64 }

pub async fn reload(State(state): State<AppState>) -> Result<Json<ReloadResponse>, StatusCode> {
    let cur = state.current.load();
    let path = cur.path.clone();

    match navpath_core::Snapshot::open(&path) {
        Ok(new_snap) => {
            let new_hash = crate::read_tail_hash_hex(&path);
            // Build coord index from newly loaded snapshot
            let idx = Arc::new(crate::build_coord_index(&new_snap));
            let new_state = SnapshotState {
                path: path.clone(),
                snapshot: Some(Arc::new(new_snap)),
                loaded_at_unix: crate::now_unix(),
                snapshot_hash_hex: new_hash.clone(),
                coord_index: Some(idx),
            };
            state.current.store(Arc::new(new_state));
            info!(path=?path, hash=?new_hash, "reloaded snapshot");
            let latest = state.current.load();
            Ok(Json(ReloadResponse { reloaded: true, snapshot_hash: latest.snapshot_hash_hex.clone(), loaded_at: latest.loaded_at_unix }))
        }
        Err(e) => {
            warn!(error=?e, path=?path, "reload failed; keeping old snapshot");
            Err(StatusCode::CONFLICT)
        }
    }
}
