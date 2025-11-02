use crate::config::Config;
use crate::errors::AppError;
use crate::models::{FindPathRequest, Tile};
use crate::planner::graph::GraphInputs;
use crate::planner::hpa::{plan as hpa_plan, HpaInputs, HpaOptions};
use crate::planner::cluster::{plan_same_cluster, plan_cluster_aware};
use crate::planner::micro_astar::find_path_4dir;
use crate::requirements::RequirementEvaluator;
use crate::serialization::{move_action, serialize_path, Bounds};
use crate::db::Db;
use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use anyhow::anyhow;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};
use tokio::task;

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    db_path: Option<PathBuf>,
}

pub fn build_router(config: Arc<Config>) -> Router {
    let state = AppState { config: Arc::clone(&config), db_path: config.db_path.clone() };

    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/version", get(version))
        .route("/find_path", post(find_path))
        .with_state(state)
}

#[derive(Serialize)]
struct Healthz {
    status: &'static str,
}

async fn healthz() -> Json<Healthz> {
    Json(Healthz { status: "ok" })
}

#[derive(Serialize)]
struct Readyz {
    ready: bool,
}

#[tracing::instrument(skip(state))]
async fn readyz(State(state): State<AppState>) -> Json<Readyz> {
    let t0 = Instant::now();
    let ready = if let Some(db_path) = &state.db_path {
        let db_path = db_path.clone();
        match task::spawn_blocking(move || {
            match Db::open_read_only(&db_path) {
                Ok(db) => {
                    db.list_clusters(1).is_ok()
                }
                Err(_) => false,
            }
        })
        .await
        {
            Ok(ok) => ok,
            Err(_) => false,
        }
    } else {
        false
    };
    let dt = t0.elapsed();
    debug!(ready, took_ms = %dt.as_millis(), "readyz probe completed");
    Json(Readyz { ready })
}

#[derive(Serialize)]
struct Version<'a> {
    version: &'a str,
}

async fn version() -> Json<Version<'static>> {
    Json(Version { version: env!("CARGO_PKG_VERSION") })
}

#[derive(Deserialize, Default)]
struct FindPathQuery {
    #[serde(default)]
    only_actions: Option<String>,
}

fn parse_only_actions(flag: &Option<String>) -> bool {
    match flag.as_deref() {
        Some("1") | Some("true") | Some("TRUE") | Some("True") => true,
        _ => false,
    }
}

fn expand_next_node_chain(db: &Db, evaluator: &RequirementEvaluator, first_action: &serde_json::Value) -> Vec<serde_json::Value> {
    let mut successors = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let max_steps = 32;
    let mut steps = 0;

    let mut curr_type_opt = first_action.get("metadata")
        .and_then(|m| m.get("db_row"))
        .and_then(|r| r.get("next_node_type"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let mut curr_id_opt = first_action.get("metadata")
        .and_then(|m| m.get("db_row"))
        .and_then(|r| r.get("next_node_id"))
        .and_then(|v| v.as_i64());

    let mut prev_to = first_action.get("to").cloned().unwrap_or(serde_json::Value::Null);

    while let (Some(curr_type), Some(curr_id)) = (&curr_type_opt, curr_id_opt) {
        if steps >= max_steps || seen.contains(&(curr_type.clone(), curr_id)) {
            break;
        }
        seen.insert((curr_type.clone(), curr_id));
        steps += 1;

        // Fetch row based on type
        let (row_opt, db_row_json) = match curr_type.as_str() {
            "object" => {
                if let Ok(Some(row)) = db.get_object_node(curr_id) {
                    let db_row = serde_json::json!({
                        "id": curr_id,
                        "match_type": row.match_type,
                        "object_id": row.object_id,
                        "object_name": row.object_name,
                        "action": row.action,
                        "dest_min_x": row.dest_min_x,
                        "dest_max_x": row.dest_max_x,
                        "dest_min_y": row.dest_min_y,
                        "dest_max_y": row.dest_max_y,
                        "dest_plane": row.dest_plane,
                        "orig_min_x": row.orig_min_x,
                        "orig_max_x": row.orig_max_x,
                        "orig_min_y": row.orig_min_y,
                        "orig_max_y": row.orig_max_y,
                        "orig_plane": row.orig_plane,
                        "search_radius": row.search_radius,
                        "cost": row.cost,
                        "next_node_type": row.next_node_type,
                        "next_node_id": row.next_node_id,
                        "requirement_id": row.requirement_id,
                    });
                    (Some(()), db_row) // Using () as placeholder
                } else {
                    (None, serde_json::Value::Null)
                }
            }
            "npc" => {
                if let Ok(Some(row)) = db.get_npc_node(curr_id) {
                    let db_row = serde_json::json!({
                        "id": curr_id,
                        "match_type": row.match_type,
                        "npc_id": row.npc_id,
                        "npc_name": row.npc_name,
                        "action": row.action,
                        "dest_min_x": row.dest_min_x,
                        "dest_max_x": row.dest_max_x,
                        "dest_min_y": row.dest_min_y,
                        "dest_max_y": row.dest_max_y,
                        "dest_plane": row.dest_plane,
                        "orig_min_x": row.orig_min_x,
                        "orig_max_x": row.orig_max_x,
                        "orig_min_y": row.orig_min_y,
                        "orig_max_y": row.orig_max_y,
                        "orig_plane": row.orig_plane,
                        "search_radius": row.search_radius,
                        "cost": row.cost,
                        "next_node_type": row.next_node_type,
                        "next_node_id": row.next_node_id,
                        "requirement_id": row.requirement_id,
                    });
                    (Some(()), db_row)
                } else {
                    (None, serde_json::Value::Null)
                }
            }
            "item" => {
                if let Ok(Some(row)) = db.get_item_node(curr_id) {
                    let db_row = serde_json::json!({
                        "id": curr_id,
                        "item_id": row.item_id,
                        "action": row.action,
                        "dest_min_x": row.dest_min_x,
                        "dest_max_x": row.dest_max_x,
                        "dest_min_y": row.dest_min_y,
                        "dest_max_y": row.dest_max_y,
                        "dest_plane": row.dest_plane,
                        "next_node_type": row.next_node_type,
                        "next_node_id": row.next_node_id,
                        "cost": row.cost,
                        "requirement_id": row.requirement_id,
                    });
                    (Some(()), db_row)
                } else {
                    (None, serde_json::Value::Null)
                }
            }
            "ifslot" => {
                if let Ok(Some(row)) = db.get_ifslot_node(curr_id) {
                    let db_row = serde_json::json!({
                        "id": curr_id,
                        "interface_id": row.interface_id,
                        "component_id": row.component_id,
                        "slot_id": row.slot_id,
                        "click_id": row.click_id,
                        "dest_min_x": row.dest_min_x,
                        "dest_max_x": row.dest_max_x,
                        "dest_min_y": row.dest_min_y,
                        "dest_max_y": row.dest_max_y,
                        "dest_plane": row.dest_plane,
                        "cost": row.cost,
                        "next_node_type": row.next_node_type,
                        "next_node_id": row.next_node_id,
                        "requirement_id": row.requirement_id,
                    });
                    (Some(()), db_row)
                } else {
                    (None, serde_json::Value::Null)
                }
            }
            "door" => {
                if let Ok(Some(row)) = db.get_door_node(curr_id) {
                    let db_row = serde_json::json!({
                        "id": curr_id,
                        "direction": row.direction,
                        "real_id_open": row.real_id_open,
                        "real_id_closed": row.real_id_closed,
                        "location_open_x": row.location_open_x,
                        "location_open_y": row.location_open_y,
                        "location_open_plane": row.location_open_plane,
                        "location_closed_x": row.location_closed_x,
                        "location_closed_y": row.location_closed_y,
                        "location_closed_plane": row.location_closed_plane,
                        "tile_inside_x": row.tile_inside_x,
                        "tile_inside_y": row.tile_inside_y,
                        "tile_inside_plane": row.tile_inside_plane,
                        "tile_outside_x": row.tile_outside_x,
                        "tile_outside_y": row.tile_outside_y,
                        "tile_outside_plane": row.tile_outside_plane,
                        "open_action": row.open_action,
                        "cost": row.cost,
                        "next_node_type": row.next_node_type,
                        "next_node_id": row.next_node_id,
                        "requirement_id": row.requirement_id,
                    });
                    (Some(()), db_row)
                } else {
                    (None, serde_json::Value::Null)
                }
            }
            "lodestone" => {
                if let Ok(Some(row)) = db.get_lodestone_node(curr_id) {
                    let db_row = serde_json::json!({
                        "id": curr_id,
                        "lodestone": row.lodestone,
                        "dest": [
                            row.dest_x.unwrap_or_default(),
                            row.dest_y.unwrap_or_default(),
                            row.dest_plane.unwrap_or_default()
                        ],
                        "cost": row.cost,
                        "next_node_type": row.next_node_type,
                        "next_node_id": row.next_node_id,
                        "requirement_id": row.requirement_id,
                    });
                    (Some(()), db_row)
                } else {
                    (None, serde_json::Value::Null)
                }
            }
            _ => (None, serde_json::Value::Null),
        };

        if row_opt.is_none() {
            break;
        }

        // Extract fields from db_row_json
        let requirement_id = db_row_json.get("requirement_id").and_then(|v| v.as_i64());
        let cost = db_row_json.get("cost").and_then(|v| v.as_i64()).unwrap_or_default();
        let next_type = db_row_json.get("next_node_type").and_then(|v| v.as_str()).map(|s| s.to_string());
        let next_id = db_row_json.get("next_node_id").and_then(|v| v.as_i64());

        // Check requirements
        if let Some(req_id) = requirement_id {
            if let Ok(Some(req)) = db.get_teleport_requirement(req_id) {
                if !evaluator.satisfies_all(&[req]) {
                    break;
                }
            }
        }

        // Build to_bounds: prefer dest_*, else orig_* for object/npc, or special for lodestone
        let to_bounds_val = if curr_type == "lodestone" {
            if let Some(dest) = db_row_json.get("dest").and_then(|d| d.as_array()) {
                if dest.len() >= 3 {
                    let x = dest[0].as_i64().unwrap_or_default() as i32;
                    let y = dest[1].as_i64().unwrap_or_default() as i32;
                    let p = dest[2].as_i64().unwrap_or_default() as i32;
                    serde_json::to_value(crate::serialization::Bounds::from_tile(crate::models::Tile { x, y, plane: p })).unwrap()
                } else {
                    prev_to.clone()
                }
            } else {
                prev_to.clone()
            }
        } else {
            // For others, use dest_min/max or orig_min/max
            let min_x = db_row_json.get("dest_min_x").and_then(|v| v.as_i64()).or_else(|| db_row_json.get("orig_min_x").and_then(|v| v.as_i64())).unwrap_or_default() as i32;
            let max_x = db_row_json.get("dest_max_x").and_then(|v| v.as_i64()).or_else(|| db_row_json.get("orig_max_x").and_then(|v| v.as_i64())).unwrap_or_default() as i32;
            let min_y = db_row_json.get("dest_min_y").and_then(|v| v.as_i64()).or_else(|| db_row_json.get("orig_min_y").and_then(|v| v.as_i64())).unwrap_or_default() as i32;
            let max_y = db_row_json.get("dest_max_y").and_then(|v| v.as_i64()).or_else(|| db_row_json.get("orig_max_y").and_then(|v| v.as_i64())).unwrap_or_default() as i32;
            let plane = db_row_json.get("dest_plane").and_then(|v| v.as_i64()).or_else(|| db_row_json.get("orig_plane").and_then(|v| v.as_i64())).unwrap_or_default() as i32;
            serde_json::to_value(crate::serialization::Bounds::from_min_max_plane(min_x, max_x, min_y, max_y, plane)).unwrap()
        };

        // Construct successor action
        let successor_action = serde_json::json!({
            "type": curr_type,
            "from": prev_to,
            "to": to_bounds_val,
            "cost_ms": cost,
            "node": {"type": curr_type, "id": curr_id},
            "edge_id": null,
            "requirement_id": requirement_id,
            "metadata": {
                "db_row": db_row_json
            }
        });

        successors.push(successor_action);

        prev_to = to_bounds_val;
        curr_type_opt = next_type;
        curr_id_opt = next_id;
    }

    successors
}

async fn find_path(
    State(state): State<AppState>,
    Query(q): Query<FindPathQuery>,
    Json(body): Json<FindPathRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let t_total = Instant::now();
    // Validate tiles are integers by type system; ensure same plane
    if body.start.plane != body.end.plane && state.db_path.is_none() {
        return Err(AppError::BadRequest("cross-plane paths require database/teleports enabled".to_string()));
    }

    // Build requirement evaluator (ignore extra fields by design)
    let evaluator = RequirementEvaluator::new(body.requirements.as_slice());
    let _ = &evaluator; // reserved for future teleport gating in full HPA*

    // If DB is configured, open per-request read-only connection to check walkability
    let db_opt: Option<std::sync::Arc<Db>> = match &state.db_path {
        Some(p) => Db::open_read_only(p).ok().map(std::sync::Arc::new),
        None => None,
    };
    let db_for_walk = db_opt.clone();
    let is_walkable = move |x: i32, y: i32, p: i32| -> bool {
        if let Some(ref db) = db_for_walk {
            db.get_tile(x, y, p)
                .ok()
                .flatten()
                .map(|t| t.blocked == 0)
                .unwrap_or(true)
        } else {
            true
        }
    };

    // Allowed area: if DB is configured, constrain to tiles that exist in DB; else unconstrained
    let plane = body.start.plane;
    let db_for_allowed = db_opt.clone();
    let allowed = move |x: i32, y: i32| -> bool {
        if let Some(ref db) = db_for_allowed {
            db.get_tile(x, y, plane).ok().flatten().is_some()
        } else {
            true
        }
    };

    // Run planner
    let t_algo = Instant::now();
    // Always require DB/HPA; treat missing DB as internal error (service should not start without NAVPATH_DB)
    let db = match db_opt {
        Some(db) => db,
        None => return Err(AppError::Internal(anyhow::anyhow!("missing database configuration (NAVPATH_DB)"))),
    };
    let (path_tiles, hpa_extra_actions): (Vec<Tile>, Vec<serde_json::Value>) = {
        // Prefer cluster-aware planner which assembles micro-bridges for entrance hops
        match plan_cluster_aware(db.as_ref(), &evaluator, body.start, body.end, &is_walkable) {
            Ok(Some(res)) => (res.path, res.actions),
            Ok(None) | Err(_) => {
                // Preserve previous behavior: try same-cluster micro, then general micro, then legacy HPA
                if let Ok(Some(path)) = plan_same_cluster(db.as_ref(), body.start, body.end, &is_walkable) {
                    (path, Vec::new())
                } else if let Some(path) = find_path_4dir(body.start, body.end, &allowed, |x, y| is_walkable(x, y, plane)) {
                    (path, Vec::new())
                } else {
                    // Fallback to legacy HPA
        // Gather inputs for both planes when start and end differ
        let plane_s = body.start.plane;
        let plane_e = body.end.plane;
        // Entrances (if unavailable, treat as no path rather than internal error)
        let mut entrances = match db.list_cluster_entrances_by_plane(plane_s) {
            Ok(v) => v,
            Err(_) => return Err(AppError::BadRequest("no path found".to_string())),
        };
        if plane_e != plane_s {
            let mut more = match db.list_cluster_entrances_by_plane(plane_e) {
                Ok(v) => v,
                Err(_) => return Err(AppError::BadRequest("no path found".to_string())),
            };
            entrances.append(&mut more);
        }
        // Intra connections
        let mut intra = match db.list_cluster_intraconnections_by_plane(plane_s) {
            Ok(v) => v,
            Err(_) => return Err(AppError::BadRequest("no path found".to_string())),
        };
        if plane_e != plane_s {
            let mut more = match db.list_cluster_intraconnections_by_plane(plane_e) {
                Ok(v) => v,
                Err(_) => return Err(AppError::BadRequest("no path found".to_string())),
            };
            intra.append(&mut more);
        }
        // Inter connections
        let mut inter = match db.list_cluster_interconnections_by_plane(plane_s) {
            Ok(v) => v,
            Err(_) => return Err(AppError::BadRequest("no path found".to_string())),
        };
        if plane_e != plane_s {
            let mut more = match db.list_cluster_interconnections_by_plane(plane_e) {
                Ok(v) => v,
                Err(_) => return Err(AppError::BadRequest("no path found".to_string())),
            };
            inter.append(&mut more);
        }
        // Teleports (edges allowed to connect across the two planes)
        let teleports = match db.list_abstract_teleport_edges_for_planes(plane_s, plane_e) {
            Ok(v) => v,
            Err(_) => return Err(AppError::BadRequest("no path found".to_string())),
        };
        let teleport_requirements = match db.list_teleport_requirements() {
            Ok(v) => v,
            Err(_) => return Err(AppError::BadRequest("no path found".to_string())),
        };

        // Build cluster tiles map for all clusters referenced by entrances
        let mut cluster_tiles: std::collections::HashMap<i64, std::collections::HashSet<(i32, i32, i32)>> =
            std::collections::HashMap::new();
        for e in entrances.iter() {
            let cid = e.cluster_id;
            if !cluster_tiles.contains_key(&cid) {
                if let Ok(tiles) = db.list_cluster_tiles(cid) {
                    let mut set = std::collections::HashSet::new();
                    for t in tiles {
                        set.insert((t.x as i32, t.y as i32, t.plane as i32));
                    }
                    cluster_tiles.insert(cid, set);
                }
            }
        }

        // Build inputs and run HPA
        let graph_inputs = GraphInputs {
            entrances: &entrances,
            intra: &intra,
            inter: &inter,
            teleports: &teleports,
            teleport_requirements: &teleport_requirements,
        };
        let hpa_inputs = HpaInputs {
            graph_inputs,
            cluster_tiles,
            is_walkable: Box::new(move |x: i32, y: i32, p: i32| is_walkable(x, y, p)),
        };
        let opts = HpaOptions { start: body.start, end: body.end };
        match hpa_plan(&hpa_inputs, &evaluator, &opts) {
            Some(res) => (res.path, res.actions),
            None => return Err(AppError::BadRequest("no path found".to_string())),
        }
                }
            }
        }
    };
    let algo_ms = t_algo.elapsed().as_millis();

    // Serialize response
    let only_actions = parse_only_actions(&q.only_actions);
    let move_cost = state.config.move_cost_ms.unwrap_or(200) as i64;
    // Interleave move actions with non-move actions (e.g., teleports including same-plane like doors)
    // by aligning their (from,to) Bounds to consecutive tile pairs in path_tiles.
    let mut actions: Vec<serde_json::Value> = Vec::new();
    let mut action_map: std::collections::HashMap<(i32,i32,i32,i32,i32,i32), Vec<serde_json::Value>> = std::collections::HashMap::new();
    // Index non-move actions by their (from_tile -> to_tile) pair
    for act in hpa_extra_actions.into_iter() {
        let (fx, fy, fp, tx, ty, tp) = match (
            act.get("from").and_then(|v| v.get("min")).and_then(|a| a.as_array()),
            act.get("to").and_then(|v| v.get("min")).and_then(|a| a.as_array()),
        ) {
            (Some(fa), Some(ta)) if fa.len() >= 3 && ta.len() >= 3 => {
                let fx = fa[0].as_i64().unwrap_or_default() as i32;
                let fy = fa[1].as_i64().unwrap_or_default() as i32;
                let fp = fa[2].as_i64().unwrap_or_default() as i32;
                let tx = ta[0].as_i64().unwrap_or_default() as i32;
                let ty = ta[1].as_i64().unwrap_or_default() as i32;
                let tp = ta[2].as_i64().unwrap_or_default() as i32;
                (fx, fy, fp, tx, ty, tp)
            }
            _ => { actions.push(act); continue; }
        };
        action_map.entry((fx,fy,fp,tx,ty,tp)).or_default().push(act);
    }
    for w in path_tiles.windows(2) {
        let a = w[0];
        let b = w[1];
        let key = (a.x, a.y, a.plane, b.x, b.y, b.plane);
        if let Some(mut acts) = action_map.remove(&key) {
            // Insert any associated non-move actions for this hop in order
            actions.append(&mut acts);
            // Do not add a move for this hop; the teleport action covers it
        } else if a.plane != b.plane {
            // Plane changed but no explicit action found: skip move to avoid fake cross-plane step
            // This should not happen in well-formed plans
        } else {
            actions.push(move_action(a, b, move_cost));
        }
    }
    // Any remaining actions (unexpected) are appended at the end to avoid loss
    for mut leftover in action_map.into_values() { actions.append(&mut leftover); }

    // Enrich non-move actions with metadata, using DB lookups
    // This preserves existing fields and adds a `metadata` object similar to legacy outputs
    let mut enriched = Vec::with_capacity(actions.len());
    for mut act in actions.into_iter() {
        let kind_opt = act.get("type").and_then(|v| v.as_str()).map(|s| s.to_string());
        if let Some(kind) = kind_opt {
            if kind == "move" { enriched.push(act); continue; }
            // Extract node_id if present
            let node_id_opt = act.get("node").and_then(|n| n.get("id")).and_then(|v| v.as_i64());
            match (kind.as_str(), node_id_opt) {
                ("lodestone", Some(node_id)) => {
                    if let Ok(Some(row)) = db.get_lodestone_node(node_id) {
                        let lodestone_name = row.lodestone.clone().unwrap_or_default();
                        // Build db_row subset similar to legacy
                        let dest = [
                            row.dest_x.unwrap_or_default() as i32,
                            row.dest_y.unwrap_or_default() as i32,
                            row.dest_plane.unwrap_or_default() as i32,
                        ];
                        let cost_ms = act.get("cost_ms").and_then(|v| v.as_i64()).unwrap_or(row.cost.unwrap_or_default());
                        let metadata = serde_json::json!({
                            "lodestone": lodestone_name,
                            "target_lodestone": lodestone_name,
                            "db_row": {
                                "id": node_id,
                                "lodestone": row.lodestone.unwrap_or_default(),
                                "dest": dest,
                                "cost": cost_ms,
                                "next_node_type": row.next_node_type,
                                "next_node_id": row.next_node_id,
                                "requirement_id": row.requirement_id,
                            }
                        });
                        act.as_object_mut().unwrap().insert("metadata".into(), metadata);
                    }
                    enriched.push(act.clone());
                    let successors = expand_next_node_chain(&db, &evaluator, &act);
                    enriched.extend(successors);
                }
                ("object", Some(node_id)) => {
                    if let Ok(Some(row)) = db.get_object_node(node_id) {
                        let action = row.action.clone().unwrap_or_default();
                        let match_type = row.match_type.clone().unwrap_or_default();
                        let metadata = serde_json::json!({
                            "action": action.clone(),
                            "object_id": row.object_id.unwrap_or_default(),
                            "match_type": match_type.clone(),
                            "db_row": {
                                "id": node_id,
                                "match_type": match_type,
                                "object_id": row.object_id.unwrap_or_default(),
                                "object_name": row.object_name,
                                "action": action,
                                "dest_min_x": row.dest_min_x.unwrap_or_default(),
                                "dest_max_x": row.dest_max_x.unwrap_or_default(),
                                "dest_min_y": row.dest_min_y.unwrap_or_default(),
                                "dest_max_y": row.dest_max_y.unwrap_or_default(),
                                "dest_plane": row.dest_plane.unwrap_or_default(),
                                "orig_min_x": row.orig_min_x.unwrap_or_default(),
                                "orig_max_x": row.orig_max_x.unwrap_or_default(),
                                "orig_min_y": row.orig_min_y.unwrap_or_default(),
                                "orig_max_y": row.orig_max_y.unwrap_or_default(),
                                "orig_plane": row.orig_plane.unwrap_or_default(),
                                "search_radius": row.search_radius.unwrap_or_default(),
                                "cost": row.cost.unwrap_or_default(),
                                "next_node_type": row.next_node_type,
                                "next_node_id": row.next_node_id,
                                "requirement_id": row.requirement_id,
                            }
                        });
                        act.as_object_mut().unwrap().insert("metadata".into(), metadata);
                    }
                    enriched.push(act.clone());
                    let successors = expand_next_node_chain(&db, &evaluator, &act);
                    enriched.extend(successors);
                }
                ("npc", Some(node_id)) => {
                    if let Ok(Some(row)) = db.get_npc_node(node_id) {
                        let action = row.action.clone().unwrap_or_default();
                        let match_type = row.match_type.clone().unwrap_or_default();
                        let metadata = serde_json::json!({
                            "action": action.clone(),
                            "npc_id": row.npc_id.unwrap_or_default(),
                            "match_type": match_type.clone(),
                            "db_row": {
                                "id": node_id,
                                "match_type": match_type,
                                "npc_id": row.npc_id.unwrap_or_default(),
                                "npc_name": row.npc_name,
                                "action": action,
                                "dest_min_x": row.dest_min_x.unwrap_or_default(),
                                "dest_max_x": row.dest_max_x.unwrap_or_default(),
                                "dest_min_y": row.dest_min_y.unwrap_or_default(),
                                "dest_max_y": row.dest_max_y.unwrap_or_default(),
                                "dest_plane": row.dest_plane.unwrap_or_default(),
                                "orig_min_x": row.orig_min_x.unwrap_or_default(),
                                "orig_max_x": row.orig_max_x.unwrap_or_default(),
                                "orig_min_y": row.orig_min_y.unwrap_or_default(),
                                "orig_max_y": row.orig_max_y.unwrap_or_default(),
                                "orig_plane": row.orig_plane.unwrap_or_default(),
                                "search_radius": row.search_radius.unwrap_or_default(),
                                "cost": row.cost.unwrap_or_default(),
                                "next_node_type": row.next_node_type,
                                "next_node_id": row.next_node_id,
                                "requirement_id": row.requirement_id,
                            }
                        });
                        act.as_object_mut().unwrap().insert("metadata".into(), metadata);
                    }
                    enriched.push(act.clone());
                    let successors = expand_next_node_chain(&db, &evaluator, &act);
                    enriched.extend(successors);
                }
                ("item", Some(node_id)) => {
                    if let Ok(Some(row)) = db.get_item_node(node_id) {
                        let action = row.action.clone().unwrap_or_default();
                        let metadata = serde_json::json!({
                            "action": action.clone(),
                            "item_id": row.item_id.unwrap_or_default(),
                            "db_row": {
                                "id": node_id,
                                "item_id": row.item_id.unwrap_or_default(),
                                "action": action,
                                "dest_min_x": row.dest_min_x.unwrap_or_default(),
                                "dest_max_x": row.dest_max_x.unwrap_or_default(),
                                "dest_min_y": row.dest_min_y.unwrap_or_default(),
                                "dest_max_y": row.dest_max_y.unwrap_or_default(),
                                "dest_plane": row.dest_plane.unwrap_or_default(),
                                "next_node_type": row.next_node_type,
                                "next_node_id": row.next_node_id,
                                "cost": row.cost.unwrap_or_default(),
                                "requirement_id": row.requirement_id,
                            }
                        });
                        act.as_object_mut().unwrap().insert("metadata".into(), metadata);
                    }
                    enriched.push(act.clone());
                    let successors = expand_next_node_chain(&db, &evaluator, &act);
                    enriched.extend(successors);
                }
                ("ifslot", Some(node_id)) => {
                    if let Ok(Some(row)) = db.get_ifslot_node(node_id) {
                        let metadata = serde_json::json!({
                            "db_row": {
                                "id": node_id,
                                "interface_id": row.interface_id.unwrap_or_default(),
                                "component_id": row.component_id.unwrap_or_default(),
                                "slot_id": row.slot_id.unwrap_or_default(),
                                "click_id": row.click_id.unwrap_or_default(),
                                "dest_min_x": row.dest_min_x.unwrap_or_default(),
                                "dest_max_x": row.dest_max_x.unwrap_or_default(),
                                "dest_min_y": row.dest_min_y.unwrap_or_default(),
                                "dest_max_y": row.dest_max_y.unwrap_or_default(),
                                "dest_plane": row.dest_plane.unwrap_or_default(),
                                "cost": row.cost.unwrap_or_default(),
                                "next_node_type": row.next_node_type,
                                "next_node_id": row.next_node_id,
                                "requirement_id": row.requirement_id,
                            }
                        });
                        act.as_object_mut().unwrap().insert("metadata".into(), metadata);
                    }
                    enriched.push(act.clone());
                    let successors = expand_next_node_chain(&db, &evaluator, &act);
                    enriched.extend(successors);
                }
                ("door", Some(node_id)) => {
                    if let Ok(Some(row)) = db.get_door_node(node_id) {
                        let metadata = serde_json::json!({
                            "db_row": {
                                "id": node_id,
                                "direction": row.direction,
                                "real_id_open": row.real_id_open,
                                "real_id_closed": row.real_id_closed,
                                "location_open_x": row.location_open_x,
                                "location_open_y": row.location_open_y,
                                "location_open_plane": row.location_open_plane,
                                "location_closed_x": row.location_closed_x,
                                "location_closed_y": row.location_closed_y,
                                "location_closed_plane": row.location_closed_plane,
                                "tile_inside_x": row.tile_inside_x,
                                "tile_inside_y": row.tile_inside_y,
                                "tile_inside_plane": row.tile_inside_plane,
                                "tile_outside_x": row.tile_outside_x,
                                "tile_outside_y": row.tile_outside_y,
                                "tile_outside_plane": row.tile_outside_plane,
                                "open_action": row.open_action,
                                "cost": row.cost,
                                "next_node_type": row.next_node_type,
                                "next_node_id": row.next_node_id,
                                "requirement_id": row.requirement_id,
                            }
                        });
                        act.as_object_mut().unwrap().insert("metadata".into(), metadata);
                    }
                    enriched.push(act.clone());
                    let successors = expand_next_node_chain(&db, &evaluator, &act);
                    enriched.extend(successors);
                }
                _ => { enriched.push(act); }
            }
        } else {
            enriched.push(act);
        }
    }
    let actions = enriched;

    let resp = if only_actions {
        serde_json::json!({ "actions": actions })
    } else {
        serde_json::json!({ "path": serialize_path(&path_tiles), "actions": actions })
    };

    if let Some(path) = &state.config.debug_result_path {
        if let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(&resp).unwrap_or_default()) {
            warn!(error = %e, file = %path.display(), "failed to write debug result file");
        }
    }

    let total_ms = t_total.elapsed().as_millis();
    info!(
        start = %format!("{}:{}:{}", body.start.x, body.start.y, body.start.plane),
        end = %format!("{}:{}:{}", body.end.x, body.end.y, body.end.plane),
        path_len = path_tiles.len(),
        actions = actions.len(),
        algo_ms = %algo_ms,
        total_ms = %total_ms,
        "find_path completed"
    );

    Ok(Json(resp))
}
