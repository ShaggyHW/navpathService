use std::sync::Arc;

use serde_json::{json, Value};

use crate::cost::{CostModel, DEFAULT_NODE_COST_MS};
use crate::db::navmesh::{OffmeshLinkRow, PortalRow};
use crate::db::Database;
use crate::graph::provider::{Edge, GraphProvider};
use crate::models::NodeRef;
use crate::options::SearchOptions;
use crate::geometry::wkb::{decode_exterior_ring_points, point_in_ring};

#[inline]
fn encode_cell(cell_id: i32, plane: i32) -> [i32; 3] { [cell_id, 0, plane] }

#[inline]
fn decode_cell(tile: [i32; 3]) -> (i32, i32) { (tile[0], tile[2]) }

#[derive(Clone)]
pub struct NavmeshGraphProvider {
    db: Arc<Database>,
    cost_model: CostModel,
}

impl NavmeshGraphProvider {
    pub fn new(db: Database, cost_model: CostModel) -> Self {
        Self { db: Arc::new(db), cost_model }
    }
    pub fn map_point_to_cell_id(&self, x: f64, y: f64, plane: i32) -> rusqlite::Result<Option<i32>> {
        let eps = 1e-9;
        let mut candidates = self.db.query_cells_intersecting_rect_on_plane(x, y, x, y, plane)?;
        candidates.sort_by(|a,b| (a.area.partial_cmp(&b.area).unwrap_or(std::cmp::Ordering::Equal), a.id).cmp(&(std::cmp::Ordering::Equal, b.id)));
        let mut matches: Vec<(i32, f64)> = Vec::new();
        for c in candidates.into_iter() {
            if x + eps < c.minx || x - eps > c.maxx || y + eps < c.miny || y - eps > c.maxy { continue; }
            match decode_exterior_ring_points(&c.wkb) {
                Ok(ring) => {
                    if point_in_ring([x,y], &ring, eps) { matches.push((c.id, c.area)); }
                }
                Err(_) => {
                    matches.push((c.id, c.area));
                }
            }
        }
        if matches.is_empty() { return Ok(None); }
        matches.sort_by(|a,b| (a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal), a.0).cmp(&(std::cmp::Ordering::Equal, b.0)));
        Ok(matches.first().map(|(id, _)| *id))
    }
    pub fn map_point_to_tile(&self, x: f64, y: f64, plane: i32) -> rusqlite::Result<Option<[i32;3]>> {
        let cell_opt = self.map_point_to_cell_id(x, y, plane)?;
        Ok(cell_opt.map(|cid| encode_cell(cid, plane)))
    }

    fn neighbors_portals(&self, tile: [i32; 3]) -> rusqlite::Result<Vec<Edge>> {
        let (cell_id, plane) = decode_cell(tile);
        let rows: Vec<PortalRow> = self.db.iter_portals_touching_cell(plane, cell_id)?;
        let mut edges: Vec<Edge> = Vec::with_capacity(rows.len());
        for r in rows.into_iter() {
            let next_cell = if r.a_id == cell_id { r.b_id } else if r.b_id == cell_id { r.a_id } else { continue };
            let to_tile = encode_cell(next_cell, r.plane);
            let cost_ms = (r.length.round() as i64).max(0);
            let meta = json!({
                "portal_id": r.id,
                "plane": r.plane,
                "a_id": r.a_id,
                "b_id": r.b_id,
                "x1": r.x1, "y1": r.y1,
                "x2": r.x2, "y2": r.y2,
                "length": r.length,
            });
            edges.push(Edge { type_: "move".into(), from_tile: tile, to_tile, cost_ms, node: None, metadata: Some(meta) });
        }
        edges.sort_by(|a,b| (a.to_tile[0], a.metadata.as_ref().and_then(|m| m.get("portal_id")).and_then(|v| v.as_i64()).unwrap_or(-1))
            .cmp(&(b.to_tile[0], b.metadata.as_ref().and_then(|m| m.get("portal_id")).and_then(|v| v.as_i64()).unwrap_or(-1))));
        Ok(edges)
    }

    fn neighbors_offmesh(&self, tile: [i32; 3], options: &SearchOptions) -> rusqlite::Result<Vec<Edge>> {
        let (cell_id, _plane) = decode_cell(tile);
        let mut edges: Vec<Edge> = Vec::new();
        let mut rows: Vec<OffmeshLinkRow> = self.db.iter_offmesh_links_from_cell(cell_id)?;
        let mut globals = self.db.iter_offmesh_links_global()?;
        rows.append(&mut globals);

        for r in rows.into_iter() {
            if let Some(req_id) = r.requirement_id { if !self.req_passes(options, req_id) { continue; } }
            let dst_plane = r.plane_to;
            let to_tile = encode_cell(r.dst_cell_id, dst_plane);
            if to_tile == tile { continue; }
            let db_cost_i64 = r.cost.map(|c| c.round() as i64);
            let cost_ms = match r.link_type.as_str() {
                "door" => self.cost_model.door_cost(db_cost_i64),
                "lodestone" => self.cost_model.lodestone_cost(db_cost_i64),
                _ => db_cost_i64.unwrap_or(DEFAULT_NODE_COST_MS),
            };
            let node = Some(NodeRef { type_: r.link_type.clone(), id: r.node_id });
            // Start with base fields
            let mut meta = json!({
                "offmesh_id": r.id,
                "link_type": r.link_type,
                "node_table": r.node_table,
                "node_id": r.node_id,
                "requirement_id": r.requirement_id,
                "plane_from": r.plane_from,
                "plane_to": r.plane_to,
                "src_cell_id": r.src_cell_id,
                "dst_cell_id": r.dst_cell_id,
            });
            // Try to unpack builder-provided meta_json for OG parity fields
            if let Some(mj) = r.meta_json.as_ref() {
                if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(mj) {
                    match r.link_type.as_str() {
                        "lodestone" => {
                            let lodename = m.get("lodestone").and_then(Value::as_str).unwrap_or("");
                            meta["lodestone"] = Value::from(lodename);
                            meta["target_lodestone"] = Value::from(lodename);
                            // db_row synthesis
                            let (dx, dy, dp) = m.get("dest_point").and_then(Value::as_array)
                                .and_then(|a| if a.len()==3 { Some((a[0].as_i64().unwrap_or(0) as i32, a[1].as_i64().unwrap_or(0) as i32, a[2].as_i64().unwrap_or(0) as i32)) } else { None })
                                .unwrap_or((0,0,0));
                            let mut db_row = json!({
                                "id": r.node_id,
                                "lodestone": lodename,
                                "dest": [dx, dy, dp],
                                "cost": db_cost_i64.unwrap_or(DEFAULT_NODE_COST_MS),
                                "next_node_type": m.get("next_node_type").cloned().unwrap_or(Value::Null),
                                "next_node_id": m.get("next_node_id").cloned().unwrap_or(Value::Null),
                                "requirement_id": r.requirement_id,
                            });
                            meta["db_row"] = db_row;
                        }
                        "door" => {
                            // Preserve door-specific fields
                            if let Some(v) = m.get("open_action").cloned() { meta["action"] = v; }
                            if let Some(v) = m.get("direction").cloned() { meta["db_door_direction"] = v; meta["door_direction"] = meta["db_door_direction"].clone(); }
                            if let Some(v) = m.get("real_id_open").cloned() { meta["real_id_open"] = v; }
                            if let Some(v) = m.get("real_id_closed").cloned() { meta["real_id_closed"] = v; }
                            // Build db_row with inside/outside coordinates
                            let mut db_row = json!({
                                "id": r.node_id,
                                "direction": m.get("direction").cloned().unwrap_or(Value::Null),
                                "location_open_x": Value::Null,
                                "location_open_y": Value::Null,
                                "location_open_plane": Value::Null,
                                "location_closed_x": Value::Null,
                                "location_closed_y": Value::Null,
                                "location_closed_plane": Value::Null,
                                "tile_inside_x": Value::Null,
                                "tile_inside_y": Value::Null,
                                "tile_inside_plane": Value::Null,
                                "tile_outside_x": Value::Null,
                                "tile_outside_y": Value::Null,
                                "tile_outside_plane": Value::Null,
                                "open_action": m.get("open_action").cloned().unwrap_or(Value::Null),
                                "cost": db_cost_i64.unwrap_or(DEFAULT_NODE_COST_MS),
                                "next_node_type": Value::Null,
                                "next_node_id": Value::Null,
                                "requirement_id": r.requirement_id,
                            });
                            if let Some(Value::Array(inside)) = m.get("inside") {
                                if inside.len()==3 {
                                    db_row["tile_inside_x"] = inside[0].clone();
                                    db_row["tile_inside_y"] = inside[1].clone();
                                    db_row["tile_inside_plane"] = inside[2].clone();
                                }
                            }
                            if let Some(Value::Array(outside)) = m.get("outside") {
                                if outside.len()==3 {
                                    db_row["tile_outside_x"] = outside[0].clone();
                                    db_row["tile_outside_y"] = outside[1].clone();
                                    db_row["tile_outside_plane"] = outside[2].clone();
                                }
                            }
                            // Populate open/closed door world locations if present
                            if let Some(v) = m.get("location_open_x").cloned() { db_row["location_open_x"] = v; }
                            if let Some(v) = m.get("location_open_y").cloned() { db_row["location_open_y"] = v; }
                            if let Some(v) = m.get("location_open_plane").cloned() { db_row["location_open_plane"] = v; }
                            if let Some(v) = m.get("location_closed_x").cloned() { db_row["location_closed_x"] = v; }
                            if let Some(v) = m.get("location_closed_y").cloned() { db_row["location_closed_y"] = v; }
                            if let Some(v) = m.get("location_closed_plane").cloned() { db_row["location_closed_plane"] = v; }
                            meta["db_row"] = db_row;
                        }
                        "object" => {
                            if let Some(v) = m.get("action").cloned() { meta["action"] = v; }
                            if let Some(v) = m.get("object_id").cloned() { meta["object_id"] = v; }
                            if let Some(v) = m.get("object_name").cloned() { meta["object_name"] = v; }
                            if let Some(v) = m.get("match_type").cloned() { meta["match_type"] = v; }
                            // db_row reconstruction from dest_rect/orig_rect
                            let mut db_row = json!({
                                "id": r.node_id,
                                "match_type": m.get("match_type").cloned().unwrap_or(Value::Null),
                                "object_id": m.get("object_id").cloned().unwrap_or(Value::Null),
                                "object_name": m.get("object_name").cloned().unwrap_or(Value::Null),
                                "action": m.get("action").cloned().unwrap_or(Value::Null),
                                "dest_min_x": Value::Null,
                                "dest_max_x": Value::Null,
                                "dest_min_y": Value::Null,
                                "dest_max_y": Value::Null,
                                "dest_plane": Value::Null,
                                "cost": db_cost_i64.unwrap_or(DEFAULT_NODE_COST_MS),
                                "next_node_type": m.get("next_node_type").cloned().unwrap_or(Value::Null),
                                "next_node_id": m.get("next_node_id").cloned().unwrap_or(Value::Null),
                                "requirement_id": r.requirement_id,
                            });
                            if let Some(Value::Array(rect)) = m.get("dest_rect") {
                                if rect.len()==5 {
                                    db_row["dest_min_x"] = rect[0].clone();
                                    db_row["dest_max_x"] = rect[1].clone();
                                    db_row["dest_min_y"] = rect[2].clone();
                                    db_row["dest_max_y"] = rect[3].clone();
                                    db_row["dest_plane"] = rect[4].clone();
                                }
                            }
                            if let Some(Value::Array(orect)) = m.get("orig_rect") {
                                if orect.len()==5 {
                                    db_row["orig_min_x"] = orect[0].clone();
                                    db_row["orig_max_x"] = orect[1].clone();
                                    db_row["orig_min_y"] = orect[2].clone();
                                    db_row["orig_max_y"] = orect[3].clone();
                                    db_row["orig_plane"] = orect[4].clone();
                                }
                            }
                            meta["db_row"] = db_row;
                        }
                        "npc" => {
                            if let Some(v) = m.get("action").cloned() { meta["action"] = v; }
                            if let Some(v) = m.get("npc_id").cloned() { meta["npc_id"] = v; }
                            if let Some(v) = m.get("npc_name").cloned() { meta["npc_name"] = v; }
                            if let Some(v) = m.get("match_type").cloned() { meta["match_type"] = v; }
                            let mut db_row = json!({
                                "id": r.node_id,
                                "match_type": m.get("match_type").cloned().unwrap_or(Value::Null),
                                "npc_id": m.get("npc_id").cloned().unwrap_or(Value::Null),
                                "npc_name": m.get("npc_name").cloned().unwrap_or(Value::Null),
                                "action": m.get("action").cloned().unwrap_or(Value::Null),
                                "dest_min_x": Value::Null,
                                "dest_max_x": Value::Null,
                                "dest_min_y": Value::Null,
                                "dest_max_y": Value::Null,
                                "dest_plane": Value::Null,
                                "search_radius": Value::Null,
                                "cost": db_cost_i64.unwrap_or(DEFAULT_NODE_COST_MS),
                                "orig_min_x": Value::Null,
                                "orig_max_x": Value::Null,
                                "orig_min_y": Value::Null,
                                "orig_max_y": Value::Null,
                                "orig_plane": Value::Null,
                                "next_node_type": m.get("next_node_type").cloned().unwrap_or(Value::Null),
                                "next_node_id": m.get("next_node_id").cloned().unwrap_or(Value::Null),
                                "requirement_id": r.requirement_id,
                            });
                            if let Some(Value::Array(rect)) = m.get("dest_rect") {
                                if rect.len()==5 {
                                    db_row["dest_min_x"] = rect[0].clone();
                                    db_row["dest_max_x"] = rect[1].clone();
                                    db_row["dest_min_y"] = rect[2].clone();
                                    db_row["dest_max_y"] = rect[3].clone();
                                    db_row["dest_plane"] = rect[4].clone();
                                }
                            }
                            if let Some(Value::Array(orect)) = m.get("orig_rect") {
                                if orect.len()==5 {
                                    db_row["orig_min_x"] = orect[0].clone();
                                    db_row["orig_max_x"] = orect[1].clone();
                                    db_row["orig_min_y"] = orect[2].clone();
                                    db_row["orig_max_y"] = orect[3].clone();
                                    db_row["orig_plane"] = orect[4].clone();
                                }
                            }
                            meta["db_row"] = db_row;
                        }
                        "ifslot" => {
                            let mut db_row = json!({
                                "id": r.node_id,
                                "interface_id": m.get("interface_id").cloned().unwrap_or(Value::Null),
                                "component_id": m.get("component_id").cloned().unwrap_or(Value::Null),
                                "slot_id": m.get("slot_id").cloned().unwrap_or(Value::Null),
                                "click_id": m.get("click_id").cloned().unwrap_or(Value::Null),
                                "dest_min_x": Value::Null,
                                "dest_max_x": Value::Null,
                                "dest_min_y": Value::Null,
                                "dest_max_y": Value::Null,
                                "dest_plane": Value::Null,
                                "cost": db_cost_i64.unwrap_or(DEFAULT_NODE_COST_MS),
                                "next_node_type": m.get("next_node_type").cloned().unwrap_or(Value::Null),
                                "next_node_id": m.get("next_node_id").cloned().unwrap_or(Value::Null),
                                "requirement_id": r.requirement_id,
                            });
                            if let Some(Value::Array(rect)) = m.get("dest_rect") {
                                if rect.len()==5 {
                                    db_row["dest_min_x"] = rect[0].clone();
                                    db_row["dest_max_x"] = rect[1].clone();
                                    db_row["dest_min_y"] = rect[2].clone();
                                    db_row["dest_max_y"] = rect[3].clone();
                                    db_row["dest_plane"] = rect[4].clone();
                                }
                            }
                            meta["db_row"] = db_row;
                        }
                        "item" => {
                            if let Some(v) = m.get("action").cloned() { meta["action"] = v; }
                            if let Some(v) = m.get("item_id").cloned() { meta["item_id"] = v; }
                            let mut db_row = json!({
                                "id": r.node_id,
                                "item_id": m.get("item_id").cloned().unwrap_or(Value::Null),
                                "action": m.get("action").cloned().unwrap_or(Value::Null),
                                "dest_min_x": Value::Null,
                                "dest_max_x": Value::Null,
                                "dest_min_y": Value::Null,
                                "dest_max_y": Value::Null,
                                "dest_plane": Value::Null,
                                "cost": db_cost_i64.unwrap_or(DEFAULT_NODE_COST_MS),
                                "next_node_type": m.get("next_node_type").cloned().unwrap_or(Value::Null),
                                "next_node_id": m.get("next_node_id").cloned().unwrap_or(Value::Null),
                                "requirement_id": r.requirement_id,
                            });
                            if let Some(Value::Array(rect)) = m.get("dest_rect") {
                                if rect.len()==5 {
                                    db_row["dest_min_x"] = rect[0].clone();
                                    db_row["dest_max_x"] = rect[1].clone();
                                    db_row["dest_min_y"] = rect[2].clone();
                                    db_row["dest_max_y"] = rect[3].clone();
                                    db_row["dest_plane"] = rect[4].clone();
                                }
                            }
                            meta["db_row"] = db_row;
                        }
                        _ => {}
                    }
                }
            }
            edges.push(Edge { type_: r.link_type, from_tile: tile, to_tile, cost_ms, node, metadata: Some(meta) });
        }
        edges.sort_by(|a,b| (a.to_tile[0], a.metadata.as_ref().and_then(|m| m.get("offmesh_id")).and_then(|v| v.as_i64()).unwrap_or(-1))
            .cmp(&(b.to_tile[0], b.metadata.as_ref().and_then(|m| m.get("offmesh_id")).and_then(|v| v.as_i64()).unwrap_or(-1))));
        Ok(edges)
    }

    fn req_passes(&self, options: &SearchOptions, requirement_id: i32) -> bool {
        let row = match self.db.fetch_requirement(requirement_id) { Ok(r) => r, Err(_) => None };
        let Some(r) = row else { return false };
        let mut ctx: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        if let Some(obj) = options.extras.get("requirements_map").and_then(serde_json::Value::as_object) {
            for (k,v) in obj.iter() { if let Some(i) = v.as_i64() { ctx.insert(k.clone(), i); } }
        } else if let Some(arr) = options.extras.get("requirements").and_then(serde_json::Value::as_array) {
            for item in arr { if let (Some(k), Some(v)) = (item.get("key").and_then(serde_json::Value::as_str), item.get("value").and_then(serde_json::Value::as_i64)) { ctx.insert(k.to_string(), v); } }
        }
        let key = match &r.key { Some(k) => k, None => return true };
        let req_val = match r.value { Some(v) => v, None => return true };
        let Some(actual) = ctx.get(key).copied() else { return false };
        match r.comparison.as_deref() {
            Some("==") => actual == req_val,
            Some("!=") => actual != req_val,
            Some(">=") | None => actual >= req_val,
            Some(">") => actual > req_val,
            Some("<=") => actual <= req_val,
            Some("<") => actual < req_val,
            Some(_) => actual >= req_val,
        }
    }
}

impl GraphProvider for NavmeshGraphProvider {
    fn neighbors(&self, tile: [i32; 3], _goal: [i32; 3], options: &SearchOptions) -> rusqlite::Result<Vec<Edge>> {
        let mut edges: Vec<Edge> = Vec::new();
        let portals = self.neighbors_portals(tile)?;
        edges.extend(portals);
        let offmesh = self.neighbors_offmesh(tile, options)?;
        edges.extend(offmesh);
        Ok(edges)
    }
}
