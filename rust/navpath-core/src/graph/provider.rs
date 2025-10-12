use crate::cost::CostModel;
use crate::db::rows::TileRow;
use crate::db::Database;
use crate::graph::chain_index::ChainHeadIndexState;
use crate::graph::movement::{decode_allowed_mask_str, mask_from_tiledata, MOVEMENT_ORDER};
use crate::graph::plane_cache::{PlaneTileCache, _tile_exists};
use crate::graph::touch_cache::TouchingNodesCache;
use crate::models::{NodeRef, Tile};
use crate::nodes::NodeChainResolver;
use crate::options::SearchOptions;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;
use tracing::debug;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub type_: String,
    pub from_tile: Tile,
    pub to_tile: Tile,
    pub cost_ms: i64,
    pub node: Option<NodeRef>,
    pub metadata: Option<Value>,
}

pub trait GraphProvider {
    fn neighbors(
        &self,
        tile: Tile,
        goal: Tile,
        options: &SearchOptions,
    ) -> rusqlite::Result<Vec<Edge>>;
}

pub struct SqliteGraphProvider {
    db: Arc<Database>,
    cost_model: CostModel,
    plane_cache: PlaneTileCache,
    touch_cache: TouchingNodesCache,
    chain_index: ChainHeadIndexState,
}

impl SqliteGraphProvider {
    pub fn new(db: Database, cost_model: CostModel) -> Self {
        Self {
            db: Arc::new(db),
            cost_model,
            plane_cache: PlaneTileCache::new(),
            touch_cache: TouchingNodesCache::new(),
            chain_index: ChainHeadIndexState::new(),
        }
    }    

    fn neighbors_objects(&self, tile: Tile, options: &SearchOptions) -> rusqlite::Result<Vec<Edge>> {
        let index = self.chain_index.ensure_built(self.db.as_ref());
        let rows_arc = self
            .touch_cache
            .object_nodes_touching(self.db.as_ref(), tile, |db, t| db.iter_object_nodes_touching(t))?;
        let rows: &Vec<crate::db::rows::ObjectNodeRow> = &rows_arc;
        let mut edges: Vec<Edge> = Vec::new();
        let mut seen: std::collections::HashSet<(i32, Tile)> = std::collections::HashSet::new();
        let mut resolver = NodeChainResolver::new(self.db.as_ref(), &self.cost_model, options);

        for r in rows.iter() {
            if index.is_non_head("object", r.id) { continue; }
            let start = NodeRef { type_: "object".into(), id: r.id };
            let res = resolver.resolve(&start);
            if !res.is_success() { continue; }
            let bounds = match res.destination.as_ref() { Some(b) => b, None => continue };
            let dest_plane = bounds.plane.unwrap_or(tile[2]);
            let dest: Tile = [bounds.min_x, bounds.min_y, dest_plane];
            if dest == tile { continue; }
            if !_tile_exists(self.db.as_ref(), &self.plane_cache, dest[0], dest[1], dest[2])? { continue; }
            let key = (r.id, dest);
            if !seen.insert(key) { continue; }
            let cost = res.total_cost_ms;
            let meta = json!({
                "action": r.action,
                "object_id": r.object_id,
                "object_name": r.object_name,
                "match_type": r.match_type,
                "db_row": &r,
            });
            edges.push(Edge { type_: "object".into(), from_tile: tile, to_tile: dest, cost_ms: cost, node: Some(NodeRef { type_: "object".into(), id: r.id }), metadata: Some(meta) });
        }
        edges.sort_by(|a, b| (
            a.node.as_ref().map(|n| n.id).unwrap_or(-1),
            a.to_tile,
        ).cmp(&(
            b.node.as_ref().map(|n| n.id).unwrap_or(-1),
            b.to_tile,
        )));
        Ok(edges)
    }

    fn neighbors_items(&self, tile: Tile, options: &SearchOptions) -> rusqlite::Result<Vec<Edge>> {
        let index = self.chain_index.ensure_built(self.db.as_ref());
        let rows = self.db.iter_item_nodes()?;
        let mut edges: Vec<Edge> = Vec::new();
        let mut seen: std::collections::HashSet<(i32, Tile)> = std::collections::HashSet::new();
        let mut resolver = NodeChainResolver::new(self.db.as_ref(), &self.cost_model, options);

        for r in rows.into_iter() {
            if index.is_non_head("item", r.id) { continue; }
            if let Some(req_id) = r.requirement_id { if !self.req_passes(options, req_id) { continue; } }
            let start = NodeRef { type_: "item".into(), id: r.id };
            let res = resolver.resolve(&start);
            if !res.is_success() { continue; }
            let bounds = match res.destination.as_ref() { Some(b) => b, None => continue };
            let dest_plane = bounds.plane.unwrap_or(tile[2]);
            let dest: Tile = [bounds.min_x, bounds.min_y, dest_plane];
            if dest == tile { continue; }
            if !_tile_exists(self.db.as_ref(), &self.plane_cache, dest[0], dest[1], dest[2])? { continue; }
            let key = (r.id, dest);
            if !seen.insert(key) { continue; }
            let cost = res.total_cost_ms;
            let meta = json!({
                "action": r.action,
                "item_id": r.item_id,
                "db_row": &r,
            });
            edges.push(Edge { type_: "item".into(), from_tile: tile, to_tile: dest, cost_ms: cost, node: Some(NodeRef { type_: "item".into(), id: r.id }), metadata: Some(meta) });
        }
        edges.sort_by(|a, b| (
            a.node.as_ref().map(|n| n.id).unwrap_or(-1),
            a.to_tile,
        ).cmp(&(
            b.node.as_ref().map(|n| n.id).unwrap_or(-1),
            b.to_tile,
        )));
        Ok(edges)
    }

    fn neighbors_npcs(&self, tile: Tile, options: &SearchOptions) -> rusqlite::Result<Vec<Edge>> {
        let index = self.chain_index.ensure_built(self.db.as_ref());
        let rows_arc = self
            .touch_cache
            .npc_nodes_touching(self.db.as_ref(), tile, |db, t| db.iter_npc_nodes_touching(t))?;
        let rows: &Vec<crate::db::rows::NpcNodeRow> = &rows_arc;
        let mut edges: Vec<Edge> = Vec::new();
        let mut seen: std::collections::HashSet<(i32, Tile)> = std::collections::HashSet::new();
        let mut resolver = NodeChainResolver::new(self.db.as_ref(), &self.cost_model, options);

        for r in rows.iter() {
            // Skip non-head NPCs
            if index.is_non_head("npc", r.id) { continue; }
            // Head requirement gating (in addition to resolver checks)
            if let Some(req_id) = r.requirement_id { if !self.req_passes(options, req_id) { continue; } }
            let start = NodeRef { type_: "npc".into(), id: r.id };
            let res = resolver.resolve(&start);
            if !res.is_success() { continue; }
            let bounds = match res.destination.as_ref() { Some(b) => b, None => continue };
            let dest_plane = bounds.plane.unwrap_or(tile[2]);
            let dest: Tile = [bounds.min_x, bounds.min_y, dest_plane];
            if dest == tile { continue; }
            if !_tile_exists(self.db.as_ref(), &self.plane_cache, dest[0], dest[1], dest[2])? { continue; }
            let key = (r.id, dest);
            if !seen.insert(key) { continue; }
            let cost = res.total_cost_ms;
            let meta = json!({
                "action": r.action,
                "npc_id": r.npc_id,
                "npc_name": r.npc_name,
                "match_type": r.match_type,
                "db_row": &r,
            });
            edges.push(Edge { type_: "npc".into(), from_tile: tile, to_tile: dest, cost_ms: cost, node: Some(NodeRef { type_: "npc".into(), id: r.id }), metadata: Some(meta) });
        }
        edges.sort_by(|a, b| (
            a.node.as_ref().map(|n| n.id).unwrap_or(-1),
            a.to_tile,
        ).cmp(&(
            b.node.as_ref().map(|n| n.id).unwrap_or(-1),
            b.to_tile,
        )));
        Ok(edges)
    }

    fn neighbors_ifslots(&self, tile: Tile, options: &SearchOptions) -> rusqlite::Result<Vec<Edge>> {
        let index = self.chain_index.ensure_built(self.db.as_ref());
        let rows = self.db.iter_ifslot_nodes()?;
        let mut edges: Vec<Edge> = Vec::new();
        let mut seen: std::collections::HashSet<(i32, Tile)> = std::collections::HashSet::new();
        let mut resolver = NodeChainResolver::new(self.db.as_ref(), &self.cost_model, options);

        for r in rows.into_iter() {
            if index.is_non_head("ifslot", r.id) { continue; }
            if let Some(req_id) = r.requirement_id { if !self.req_passes(options, req_id) { continue; } }
            let start = NodeRef { type_: "ifslot".into(), id: r.id };
            let res = resolver.resolve(&start);
            if !res.is_success() { continue; }
            let bounds = match res.destination.as_ref() { Some(b) => b, None => continue };
            let dest_plane = bounds.plane.unwrap_or(tile[2]);
            let dest: Tile = [bounds.min_x, bounds.min_y, dest_plane];
            if dest == tile { continue; }
            if !_tile_exists(self.db.as_ref(), &self.plane_cache, dest[0], dest[1], dest[2])? { continue; }
            let key = (r.id, dest);
            if !seen.insert(key) { continue; }
            let cost = res.total_cost_ms;
            let meta = json!({
                "interface_id": r.interface_id,
                "component_id": r.component_id,
                "slot_id": r.slot_id,
                "click_id": r.click_id,
                "db_row": &r,
            });
            edges.push(Edge { type_: "ifslot".into(), from_tile: tile, to_tile: dest, cost_ms: cost, node: Some(NodeRef { type_: "ifslot".into(), id: r.id }), metadata: Some(meta) });
        }
        edges.sort_by(|a, b| (
            a.node.as_ref().map(|n| n.id).unwrap_or(-1),
            a.to_tile,
        ).cmp(&(
            b.node.as_ref().map(|n| n.id).unwrap_or(-1),
            b.to_tile,
        )));
        Ok(edges)
    }

    pub fn set_cost_model(&mut self, cost_model: CostModel) {
        self.cost_model = cost_model;
    }

    /// Warm provider caches that are safe to prebuild.
    pub fn warm(&self) -> rusqlite::Result<()> {
        // Build chain-head index once
        let _ = self.chain_index.ensure_built(self.db.as_ref());
        Ok(())
    }

    fn neighbors_movement(&self, tile: Tile, row: &TileRow) -> rusqlite::Result<Vec<Edge>> {
        let td_mask = mask_from_tiledata(row.tiledata);
        let mask =
            td_mask.unwrap_or_else(|| decode_allowed_mask_str(row.allowed_directions.as_deref()));
        if mask == 0 {
            return Ok(vec![]);
        }
        let mut edges = Vec::new();
        for m in MOVEMENT_ORDER.iter() {
            if mask & m.bit == 0 {
                continue;
            }
            let dest = [tile[0] + m.dx, tile[1] + m.dy, tile[2]];
            if !_tile_exists(self.db.as_ref(), &self.plane_cache, dest[0], dest[1], dest[2])? {
                continue;
            }
            let cost = self.cost_model.movement_cost(tile, dest);
            edges.push(Edge {
                type_: "move".into(),
                from_tile: tile,
                to_tile: dest,
                cost_ms: cost,
                node: None,
                metadata: None,
            });
        }
        Ok(edges)
    }

    fn neighbors_doors(&self, tile: Tile) -> rusqlite::Result<Vec<Edge>> {
        let index = self.chain_index.ensure_built(self.db.as_ref());
        let mut edges = Vec::new();
        let rows = self.db.iter_door_nodes_touching(tile)?;
        for r in rows {
            // Skip non-head doors
            if index.is_non_head("door", r.id) {
                continue;
            }
            let (dest, computed_dir) = if r.tile_inside == tile {
                (r.tile_outside, Some("OUT".to_string()))
            } else if r.tile_outside == tile {
                (r.tile_inside, Some("IN".to_string()))
            } else {
                continue;
            };
            if !_tile_exists(self.db.as_ref(), &self.plane_cache, dest[0], dest[1], dest[2])? {
                continue;
            }
            let cost = self.cost_model.door_cost(r.cost);
            let meta = json!({
                "door_direction": computed_dir.unwrap_or_else(|| r.direction.clone().unwrap_or_default()),
                "db_door_direction": r.direction,
                "real_id_open": r.real_id_open,
                "real_id_closed": r.real_id_closed,
                "action": r.open_action,
                "db_row": &r,
            });
            edges.push(Edge {
                type_: "door".into(),
                from_tile: tile,
                to_tile: dest,
                cost_ms: cost,
                node: Some(NodeRef {
                    type_: "door".into(),
                    id: r.id,
                }),
                metadata: Some(meta),
            });
        }
        // Deterministic sort by (to_tile, node id)
        edges.sort_by(|a, b| {
            (a.to_tile, a.node.as_ref().map(|n| n.id).unwrap_or(-1))
                .cmp(&(b.to_tile, b.node.as_ref().map(|n| n.id).unwrap_or(-1)))
        });
        Ok(edges)
    }

    fn neighbors_lodestones(
        &self,
        tile: Tile,
        options: &SearchOptions,
    ) -> rusqlite::Result<Vec<Edge>> {
        // Start-tile gating: only emit lodestone edges at the exact start tile
        let start_opt = options
            .extras
            .get("start_tile")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                if arr.len() == 3 {
                    Some([
                        arr[0].as_i64()? as i32,
                        arr[1].as_i64()? as i32,
                        arr[2].as_i64()? as i32,
                    ])
                } else {
                    None
                }
            });
        if start_opt != Some(tile) {
            return Ok(vec![]);
        }

        let index = self.chain_index.ensure_built(self.db.as_ref());
        let mut edges = Vec::new();
        let rows = self.db.iter_lodestone_nodes()?;

        for r in rows {
            // Skip non-head lodestones
            if index.is_non_head("lodestone", r.id) {
                continue;
            }
            // Requirement gating
            if let Some(req_id) = r.requirement_id {
                if !self.req_passes(options, req_id) {
                    continue;
                }
            }
            // Destination validation: must exist and not equal current tile
            let dest = r.dest;
            if dest == tile {
                continue;
            }
            if !_tile_exists(self.db.as_ref(), &self.plane_cache, dest[0], dest[1], dest[2])? {
                continue;
            }
            let cost = self.cost_model.lodestone_cost(r.cost);
            let meta = json!({
                "lodestone": r.lodestone,
                "target_lodestone": r.lodestone,
                "db_row": &r,
            });
            edges.push(Edge {
                type_: "lodestone".into(),
                from_tile: tile,
                to_tile: dest,
                cost_ms: cost,
                node: Some(NodeRef {
                    type_: "lodestone".into(),
                    id: r.id,
                }),
                metadata: Some(meta),
            });
        }

        // Deterministic sort by (node.id, to_tile)
        edges.sort_by(|a, b| {
            (a.node.as_ref().map(|n| n.id).unwrap_or(-1), a.to_tile)
                .cmp(&(b.node.as_ref().map(|n| n.id).unwrap_or(-1), b.to_tile))
        });
        Ok(edges)
    }

    fn req_passes(&self, options: &SearchOptions, requirement_id: i32) -> bool {
        // Fetch row; if missing or error, treat as not passing
        let row = match self.db.fetch_requirement(requirement_id) {
            Ok(r) => r,
            Err(_) => None,
        };
        let Some(r) = row else { return false };
        // Build context map from options.extras (supports requirements_map or requirements array)
        let mut ctx: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        if let Some(obj) = options
            .extras
            .get("requirements_map")
            .and_then(Value::as_object)
        {
            for (k, v) in obj.iter() {
                if let Some(i) = v.as_i64() {
                    ctx.insert(k.clone(), i);
                }
            }
        } else if let Some(arr) = options.extras.get("requirements").and_then(Value::as_array) {
            for item in arr {
                if let (Some(k), Some(v)) = (
                    item.get("key").and_then(Value::as_str),
                    item.get("value").and_then(Value::as_i64),
                ) {
                    ctx.insert(k.to_string(), v);
                }
            }
        }
        // Evaluate requirement (default comparison >=)
        let key = match &r.key {
            Some(k) => k,
            None => return true,
        };
        let req_val = match r.value {
            Some(v) => v,
            None => return true,
        };
        let Some(actual) = ctx.get(key).copied() else {
            return false;
        };
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

impl GraphProvider for SqliteGraphProvider {
    fn neighbors(
        &self,
        tile: Tile,
        _goal: Tile,
        options: &SearchOptions,
    ) -> rusqlite::Result<Vec<Edge>> {
        let started_at = Instant::now();
        // Ensure chain-head index built once
        let _ = self.chain_index.ensure_built(self.db.as_ref());
        // Fetch tile row or abort
        let row = match self.db.fetch_tile(tile[0], tile[1], tile[2])? {
            Some(r) => r,
            None => {
                let duration_ms = started_at.elapsed().as_millis() as u64;
                debug!(tile=?tile, duration_ms, "neighbors_missing_tile");
                return Ok(vec![]);
            }
        };

        let mut edges: Vec<Edge> = Vec::new();
        let mut move_edges = 0usize;
        let mut door_edges = 0usize;
        let mut lodestone_edges = 0usize;
        let mut object_edges = 0usize;
        let mut ifslot_edges = 0usize;
        let mut npc_edges = 0usize;
        let mut item_edges = 0usize;
        // Movement
        let movement = self.neighbors_movement(tile, &row)?;
        move_edges = movement.len();
        edges.extend(movement);
        // Doors
        if options.use_doors {
            let doors = self.neighbors_doors(tile)?;
            door_edges = doors.len();
            edges.extend(doors);
        }
        // Lodestones (start-tile gated inside neighbors_lodestones)
        if options.use_lodestones {
            let lodestones = self.neighbors_lodestones(tile, options)?;
            lodestone_edges = lodestones.len();
            edges.extend(lodestones);
        }
        // Objects via resolver and touch-cache
        if options.use_objects {
            let objects = self.neighbors_objects(tile, options)?;
            object_edges = objects.len();
            edges.extend(objects);
        }
        // Ifslots via resolver (table-wide)
        if options.use_ifslots {
            let ifslots = self.neighbors_ifslots(tile, options)?;
            ifslot_edges = ifslots.len();
            edges.extend(ifslots);
        }
        // NPCs via resolver and touch-cache
        if options.use_npcs {
            let npcs = self.neighbors_npcs(tile, options)?;
            npc_edges = npcs.len();
            edges.extend(npcs);
        }
        // Items via resolver (table-wide)
        if options.use_items {
            let items = self.neighbors_items(tile, options)?;
            item_edges = items.len();
            edges.extend(items);
        }
        // Preserve deterministic per-type ordering: movement edges are emitted in MOVEMENT_ORDER;
        // other types are individually sorted within their neighbor functions.
        let duration_ms = started_at.elapsed().as_millis() as u64;
        let total_edges = edges.len();
        debug!(tile=?tile, duration_ms, move_edges, door_edges, lodestone_edges, object_edges, ifslot_edges, npc_edges, item_edges, total_edges, "neighbors_done");
        Ok(edges)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::json;

    fn setup_db() -> Database {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE tiles (
                x INTEGER, y INTEGER, plane INTEGER,
                tiledata INTEGER, allowed_directions TEXT, blocked_directions TEXT
            );
            CREATE TABLE door_nodes (
                id INTEGER PRIMARY KEY,
                direction TEXT,
                tile_inside_x INTEGER, tile_inside_y INTEGER, tile_inside_plane INTEGER,
                tile_outside_x INTEGER, tile_outside_y INTEGER, tile_outside_plane INTEGER,
                location_open_x INTEGER, location_open_y INTEGER, location_open_plane INTEGER,
                location_closed_x INTEGER, location_closed_y INTEGER, location_closed_plane INTEGER,
                real_id_open INTEGER, real_id_closed INTEGER,
                cost INTEGER, open_action TEXT, next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );
            "#,
        ).unwrap();
        Database::from_connection(conn)
    }    

    #[test]
    fn ifslot_neighbors_gating_and_sorting() {
        let db = setup_db();
        // Tiles
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (0,0,0)", []).unwrap();
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (10,30,0)", []).unwrap();
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (15,35,0)", []).unwrap();
        // Requirements table
        db.conn().execute("CREATE TABLE requirements (id INTEGER PRIMARY KEY, metaInfo TEXT, key TEXT, value INTEGER, comparison TEXT)", []).unwrap();
        db.conn().execute("INSERT INTO requirements (id,key,value,comparison) VALUES (7,'magic',50,'>=')", []).unwrap();
        // Ifslot table
        db.conn().execute_batch(
            r#"CREATE TABLE ifslot_nodes (
                id INTEGER PRIMARY KEY,
                interface_id INTEGER, component_id INTEGER, slot_id INTEGER, click_id INTEGER,
                dest_min_x INTEGER, dest_max_x INTEGER, dest_min_y INTEGER, dest_max_y INTEGER, dest_plane INTEGER,
                cost INTEGER,
                next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );"#,
        ).unwrap();
        // Head ifslot id=1 -> dest (10,30,0)
        db.conn().execute(
            "INSERT INTO ifslot_nodes (id,interface_id,component_id,slot_id,click_id,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,cost) VALUES (1,548,12,NULL,0,10,10,30,30,0,100)",
            [],
        ).unwrap();
        // Head ifslot id=2 -> requires magic>=50 -> dest (15,35,0)
        db.conn().execute(
            "INSERT INTO ifslot_nodes (id,interface_id,component_id,slot_id,click_id,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,cost,requirement_id) VALUES (2,548,13,NULL,0,15,15,35,35,0,150,7)",
            [],
        ).unwrap();
        // Non-head ifslot id=3 referenced by door
        db.conn().execute(
            r#"INSERT INTO door_nodes (
                id, direction,
                tile_inside_x, tile_inside_y, tile_inside_plane,
                tile_outside_x, tile_outside_y, tile_outside_plane,
                location_open_x, location_open_y, location_open_plane,
                location_closed_x, location_closed_y, location_closed_plane,
                real_id_open, real_id_closed, cost, open_action, next_node_type, next_node_id, requirement_id
            ) VALUES (
                77, 'east', 0,0,0, 0,0,0, 0,0,0, 0,0,0, 0,0, 0, NULL, 'ifslot', 3, NULL
            )"#,
            [],
        ).unwrap();
        db.conn().execute(
            "INSERT INTO ifslot_nodes (id,interface_id,component_id,slot_id,click_id,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,cost) VALUES (3,548,14,NULL,0,10,10,30,30,0,120)",
            [],
        ).unwrap();

        let gp = SqliteGraphProvider::new(db, CostModel::default());
        let mut opts = SearchOptions::default();
        opts.extras.insert("requirements_map".into(), json!({"magic": 55}));
        let edges = gp.neighbors([0,0,0], [0,0,0], &opts).unwrap();
        let mut ifs: Vec<&Edge> = edges.iter().filter(|e| e.type_ == "ifslot").collect();
        assert_eq!(ifs.len(), 2);
        ifs.sort_by(|a,b| (a.node.as_ref().unwrap().id, a.to_tile).cmp(&(b.node.as_ref().unwrap().id, b.to_tile)));
        assert_eq!(ifs[0].node.as_ref().unwrap().id, 1);
        assert_eq!(ifs[0].to_tile, [10,30,0]);
        assert_eq!(ifs[1].node.as_ref().unwrap().id, 2);
        assert_eq!(ifs[1].to_tile, [15,35,0]);
    }

    #[test]
    fn npc_neighbors_gating_sorting_and_nonhead() {
        let db = setup_db();
        // Tiles
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (0,0,0)", []).unwrap();
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (10,20,0)", []).unwrap();
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (11,21,0)", []).unwrap();
        // Requirements
        db.conn().execute("CREATE TABLE requirements (id INTEGER PRIMARY KEY, metaInfo TEXT, key TEXT, value INTEGER, comparison TEXT)", []).unwrap();
        db.conn().execute("INSERT INTO requirements (id,key,value,comparison) VALUES (8,'magic',50,'>=')", []).unwrap();
        // NPC table
        db.conn().execute_batch(
            r#"CREATE TABLE npc_nodes (
                id INTEGER PRIMARY KEY,
                match_type TEXT,
                npc_id INTEGER, npc_name TEXT, action TEXT,
                dest_min_x INTEGER, dest_max_x INTEGER, dest_min_y INTEGER, dest_max_y INTEGER, dest_plane INTEGER,
                orig_min_x INTEGER, orig_max_x INTEGER, orig_min_y INTEGER, orig_max_y INTEGER, orig_plane INTEGER,
                search_radius INTEGER, cost INTEGER, next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );"#,
        ).unwrap();
        // Head npc id=1 -> dest (10,20,0), origin includes [0,0,0]
        db.conn().execute(
            "INSERT INTO npc_nodes (id,match_type,npc_id,npc_name,action,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,orig_min_x,orig_max_x,orig_min_y,orig_max_y,orig_plane,search_radius,cost) VALUES (1,'id',200,'Bob','Talk-to',10,10,20,20,0,0,0,0,0,0,3,5)",
            [],
        ).unwrap();
        // Head npc id=2 -> requires magic>=50 -> dest (11,21,0)
        db.conn().execute(
            "INSERT INTO npc_nodes (id,match_type,npc_id,npc_name,action,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,orig_min_x,orig_max_x,orig_min_y,orig_max_y,orig_plane,search_radius,cost,requirement_id) VALUES (2,'id',201,'Alice','Talk-to',11,11,21,21,0,0,0,0,0,0,3,7,8)",
            [],
        ).unwrap();
        // Non-head npc id=3 referenced by door
        db.conn().execute(
            r#"INSERT INTO door_nodes (
                id, direction,
                tile_inside_x, tile_inside_y, tile_inside_plane,
                tile_outside_x, tile_outside_y, tile_outside_plane,
                location_open_x, location_open_y, location_open_plane,
                location_closed_x, location_closed_y, location_closed_plane,
                real_id_open, real_id_closed, cost, open_action, next_node_type, next_node_id, requirement_id
            ) VALUES (
                88, 'east', 0,0,0, 0,0,0, 0,0,0, 0,0,0, 0,0, 0, NULL, 'npc', 3, NULL
            )"#,
            [],
        ).unwrap();
        db.conn().execute(
            "INSERT INTO npc_nodes (id,match_type,npc_id,npc_name,action,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,orig_min_x,orig_max_x,orig_min_y,orig_max_y,orig_plane,search_radius,cost) VALUES (3,'id',202,'Eve','Talk-to',10,10,20,20,0,0,0,0,0,0,3,5)",
            [],
        ).unwrap();

        let gp = SqliteGraphProvider::new(db, CostModel::default());
        let mut opts = SearchOptions::default();
        opts.extras.insert("requirements_map".into(), serde_json::json!({"magic": 55}));
        let edges = gp.neighbors([0,0,0], [0,0,0], &opts).unwrap();
        let mut npcs: Vec<&Edge> = edges.iter().filter(|e| e.type_ == "npc").collect();
        // Should include id 1 and 2; exclude id 3 (non-head)
        assert_eq!(npcs.len(), 2);
        npcs.sort_by(|a,b| (a.node.as_ref().unwrap().id, a.to_tile).cmp(&(b.node.as_ref().unwrap().id, b.to_tile)));
        assert_eq!(npcs[0].node.as_ref().unwrap().id, 1);
        assert_eq!(npcs[0].to_tile, [10,20,0]);
        assert_eq!(npcs[1].node.as_ref().unwrap().id, 2);
        assert_eq!(npcs[1].to_tile, [11,21,0]);
    }

    #[test]
    fn item_neighbors_gating_and_sorting() {
        let db = setup_db();
        // Tiles
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (0,0,0)", []).unwrap();
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (10,30,0)", []).unwrap();
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (15,35,0)", []).unwrap();
        // Requirements table
        db.conn().execute("CREATE TABLE requirements (id INTEGER PRIMARY KEY, metaInfo TEXT, key TEXT, value INTEGER, comparison TEXT)", []).unwrap();
        db.conn().execute("INSERT INTO requirements (id,key,value,comparison) VALUES (9,'magic',50,'>=')", []).unwrap();
        // Item table
        db.conn().execute_batch(
            r#"CREATE TABLE item_nodes (
                id INTEGER PRIMARY KEY,
                item_id INTEGER, action TEXT,
                dest_min_x INTEGER, dest_max_x INTEGER, dest_min_y INTEGER, dest_max_y INTEGER, dest_plane INTEGER,
                cost INTEGER,
                next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );"#,
        ).unwrap();
        // Head item id=1 -> dest (10,30,0)
        db.conn().execute(
            "INSERT INTO item_nodes (id,item_id,action,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,cost) VALUES (1, 4151, 'Use', 10,10,30,30,0,100)",
            [],
        ).unwrap();
        // Head item id=2 -> requires magic>=50 -> dest (15,35,0)
        db.conn().execute(
            "INSERT INTO item_nodes (id,item_id,action,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,cost,requirement_id) VALUES (2, 4152, 'Use', 15,15,35,35,0,150,9)",
            [],
        ).unwrap();
        // Non-head item id=3 referenced by door
        db.conn().execute(
            r#"INSERT INTO door_nodes (
                id, direction,
                tile_inside_x, tile_inside_y, tile_inside_plane,
                tile_outside_x, tile_outside_y, tile_outside_plane,
                location_open_x, location_open_y, location_open_plane,
                location_closed_x, location_closed_y, location_closed_plane,
                real_id_open, real_id_closed, cost, open_action, next_node_type, next_node_id, requirement_id
            ) VALUES (
                99, 'east', 0,0,0, 0,0,0, 0,0,0, 0,0,0, 0, 0, 0, NULL, 'item', 3, NULL
            )"#,
            [],
        ).unwrap();
        db.conn().execute(
            "INSERT INTO item_nodes (id,item_id,action,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,cost) VALUES (3, 4153, 'Use', 10,10,30,30,0,120)",
            [],
        ).unwrap();

        let gp = SqliteGraphProvider::new(db, CostModel::default());
        let mut opts = SearchOptions::default();
        opts.extras.insert("requirements_map".into(), json!({"magic": 55}));
        let edges = gp.neighbors_items([0,0,0], &opts).unwrap();
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].node.as_ref().unwrap().id, 1);
        assert_eq!(edges[0].to_tile, [10,30,0]);
        assert_eq!(edges[1].node.as_ref().unwrap().id, 2);
        assert_eq!(edges[1].to_tile, [15,35,0]);
    }

    #[test]
    fn neighbors_integration_respects_toggles_and_ordering() {
        let db = setup_db();
        // Tiles: start and destinations
        db.conn().execute("INSERT INTO tiles (x,y,plane,allowed_directions) VALUES (0,0,0,'1')", []).unwrap(); // NORTH only
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (0,1,0)", []).unwrap(); // move north
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (1,0,0)", []).unwrap(); // door dest
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (2,0,0)", []).unwrap(); // lodestone dest
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (3,0,0)", []).unwrap(); // object dest
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (4,0,0)", []).unwrap(); // ifslot dest
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (5,0,0)", []).unwrap(); // npc dest
        db.conn().execute("INSERT INTO tiles (x,y,plane) VALUES (6,0,0)", []).unwrap(); // item dest

        // Door touching start tile -> (1,0,0)
        db.conn().execute(
            r#"INSERT INTO door_nodes (
                id, direction,
                tile_inside_x, tile_inside_y, tile_inside_plane,
                tile_outside_x, tile_outside_y, tile_outside_plane,
                location_open_x, location_open_y, location_open_plane,
                location_closed_x, location_closed_y, location_closed_plane,
                real_id_open, real_id_closed, cost, open_action, next_node_type, next_node_id, requirement_id
            ) VALUES (
                500, 'east', 0,0,0, 1,0,0, 0,0,0, 1,0,0, 0, 0, 50, 'Open', NULL, NULL, NULL
            )"#,
            [],
        ).unwrap();

        // Create other node tables
        db.conn().execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS lodestone_nodes (
                id INTEGER PRIMARY KEY,
                lodestone TEXT,
                dest_x INTEGER, dest_y INTEGER, dest_plane INTEGER,
                cost INTEGER,
                next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );
            CREATE TABLE IF NOT EXISTS object_nodes (
                id INTEGER PRIMARY KEY,
                match_type TEXT,
                object_id INTEGER,
                object_name TEXT,
                action TEXT,
                dest_min_x INTEGER, dest_max_x INTEGER, dest_min_y INTEGER, dest_max_y INTEGER, dest_plane INTEGER,
                orig_min_x INTEGER, orig_max_x INTEGER, orig_min_y INTEGER, orig_max_y INTEGER, orig_plane INTEGER,
                search_radius INTEGER,
                cost INTEGER,
                next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );
            CREATE TABLE IF NOT EXISTS ifslot_nodes (
                id INTEGER PRIMARY KEY,
                interface_id INTEGER, component_id INTEGER, slot_id INTEGER, click_id INTEGER,
                dest_min_x INTEGER, dest_max_x INTEGER, dest_min_y INTEGER, dest_max_y INTEGER, dest_plane INTEGER,
                cost INTEGER,
                next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );
            CREATE TABLE IF NOT EXISTS npc_nodes (
                id INTEGER PRIMARY KEY,
                match_type TEXT,
                npc_id INTEGER, npc_name TEXT, action TEXT,
                dest_min_x INTEGER, dest_max_x INTEGER, dest_min_y INTEGER, dest_max_y INTEGER, dest_plane INTEGER,
                orig_min_x INTEGER, orig_max_x INTEGER, orig_min_y INTEGER, orig_max_y INTEGER, orig_plane INTEGER,
                search_radius INTEGER,
                cost INTEGER,
                next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );
            CREATE TABLE IF NOT EXISTS item_nodes (
                id INTEGER PRIMARY KEY,
                item_id INTEGER, action TEXT,
                dest_min_x INTEGER, dest_max_x INTEGER, dest_min_y INTEGER, dest_max_y INTEGER, dest_plane INTEGER,
                cost INTEGER,
                next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );
            "#,
        ).unwrap();

        // Lodestone id=10 -> (2,0,0)
        db.conn().execute(
            "INSERT INTO lodestone_nodes (id,lodestone,dest_x,dest_y,dest_plane,cost) VALUES (10,'Start',2,0,0,5)",
            [],
        ).unwrap();

        // Object id=20 touches start -> (3,0,0)
        db.conn().execute(
            r#"INSERT INTO object_nodes (
                id, match_type, object_id, object_name, action,
                dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                search_radius, cost, next_node_type, next_node_id, requirement_id
            ) VALUES (
                20, 'id', 100, 'Obj', 'Open',
                3,3,0,0,0,
                0,0,0,0,0,
                3, 25, NULL, NULL, NULL
            )"#,
            [],
        ).unwrap();

        // Ifslot id=30 -> (4,0,0)
        db.conn().execute(
            "INSERT INTO ifslot_nodes (id,interface_id,component_id,slot_id,click_id,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,cost) VALUES (30, 548, 12, NULL, 0, 4,4,0,0,0,40)",
            [],
        ).unwrap();

        // NPC id=40 origin includes start -> (5,0,0)
        db.conn().execute(
            r#"INSERT INTO npc_nodes (
                id, match_type, npc_id, npc_name, action,
                dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                search_radius, cost, next_node_type, next_node_id, requirement_id
            ) VALUES (
                40, 'id', 200, 'Bob', 'Talk-to',
                5,5,0,0,0,
                0,0,0,0,0,
                3, 35, NULL, NULL, NULL
            )"#,
            [],
        ).unwrap();

        // Item id=50 -> (6,0,0)
        db.conn().execute(
            "INSERT INTO item_nodes (id,item_id,action,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,cost) VALUES (50, 4151, 'Use', 6,6,0,0,0,60)",
            [],
        ).unwrap();

        let gp = SqliteGraphProvider::new(db, CostModel::default());

        // With all toggles enabled; include start_tile for lodestones
        let mut opts = SearchOptions::default();
        opts.extras.insert("start_tile".into(), json!([0,0,0]));
        let edges = gp.neighbors([0,0,0], [99,99,0], &opts).unwrap();

        // Expect deterministic ordering by type sequence and per-type sorts
        let types: Vec<&str> = edges.iter().map(|e| e.type_.as_str()).collect();
        let tos: Vec<[i32;3]> = edges.iter().map(|e| e.to_tile).collect();
        assert_eq!(types, vec!["move","door","lodestone","object","ifslot","npc","item"]);
        assert_eq!(tos, vec![[0,1,0],[1,0,0],[2,0,0],[3,0,0],[4,0,0],[5,0,0],[6,0,0]]);

        // Disable all action edges: only movement remains
        let mut opts2 = SearchOptions::default();
        opts2.use_doors = false;
        opts2.use_lodestones = false;
        opts2.use_objects = false;
        opts2.use_ifslots = false;
        opts2.use_npcs = false;
        opts2.use_items = false;
        let edges2 = gp.neighbors([0,0,0], [99,99,0], &opts2).unwrap();
        assert_eq!(edges2.len(), 1);
        assert_eq!(edges2[0].type_.as_str(), "move");
        assert_eq!(edges2[0].to_tile, [0,1,0]);
    }
}
