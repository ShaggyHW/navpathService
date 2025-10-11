use crate::cost::CostModel;
use crate::db::rows::*;
use crate::db::Database;
use crate::models::NodeRef;
use crate::options::SearchOptions;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bounds2D {
    pub min_x: i32,
    pub max_x: i32,
    pub min_y: i32,
    pub max_y: i32,
    pub plane: Option<i32>,
}

impl Bounds2D {
    pub fn is_valid(&self) -> bool {
        self.min_x <= self.max_x && self.min_y <= self.max_y
    }

    pub fn single_tile(tile: [i32; 3]) -> Self {
        Self { min_x: tile[0], max_x: tile[0], min_y: tile[1], max_y: tile[1], plane: Some(tile[2]) }
    }

    pub fn from_optional(
        min_x: Option<i32>,
        max_x: Option<i32>,
        min_y: Option<i32>,
        max_y: Option<i32>,
        plane: Option<i32>,
    ) -> Option<Self> {
        match (min_x, max_x, min_y, max_y) {
            (Some(ax), Some(bx), Some(ay), Some(by)) => {
                let b = Self { min_x: ax, max_x: bx, min_y: ay, max_y: by, plane };
                if b.is_valid() { Some(b) } else { None }
            }
            _ => None,
        }
    }

    pub fn merge(&self, other: &Self) -> Self {
        let plane = if self.plane == other.plane { self.plane } else { None };
        Self {
            min_x: self.min_x.min(other.min_x),
            max_x: self.max_x.max(other.max_x),
            min_y: self.min_y.min(other.min_y),
            max_y: self.max_y.max(other.max_y),
            plane,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChainLink {
    pub ref_: NodeRef,
    pub cost_ms: i64,
    pub destination: Option<Bounds2D>,
    pub row: NodeRow,
}

#[derive(Clone, Debug)]
pub struct ChainResolution {
    pub start: NodeRef,
    pub links: Vec<ChainLink>,
    pub total_cost_ms: i64,
    pub destination: Option<Bounds2D>,
    pub failure_reason: Option<String>,
}

impl ChainResolution {
    pub fn is_success(&self) -> bool { self.failure_reason.is_none() && self.destination.is_some() }
    pub fn terminal_ref(&self) -> Option<&NodeRef> { self.links.last().map(|l| &l.ref_) }
}

pub struct NodeChainResolver<'a> {
    db: &'a Database,
    cost_model: &'a CostModel,
    options: &'a SearchOptions,
    max_depth: u32,
    ctx_map: HashMap<String, i64>,
    req_cache: HashMap<i32, Option<RequirementRow>>,
}

impl<'a> NodeChainResolver<'a> {
    pub fn new(db: &'a Database, cost_model: &'a CostModel, options: &'a SearchOptions) -> Self {
        let ctx_map = build_ctx_map(options);
        Self { db, cost_model, options, max_depth: options.max_chain_depth, ctx_map, req_cache: HashMap::new() }
    }

    pub fn resolve(&mut self, start: &NodeRef) -> ChainResolution {
        let norm_type = normalise_type(&start.type_);
        let mut current = NodeRef { type_: norm_type.clone(), id: start.id };
        let chain_start = NodeRef { type_: norm_type, id: start.id };

        let mut visited: HashMap<(String, i32), ()> = HashMap::new();
        let mut links: Vec<ChainLink> = Vec::new();
        let mut total_cost: i64 = 0;
        let mut depth: u32 = 0;
        let mut failure_reason: Option<String> = None;

        loop {
            if depth >= self.max_depth {
                failure_reason = Some("chain-depth-exceeded".into());
                break;
            }
            let key = (current.type_.clone(), current.id);
            if visited.contains_key(&key) {
                failure_reason = Some("cycle-detected".into());
                break;
            }
            visited.insert(key, ());

            let row = match self.db.fetch_node(&current.type_, current.id) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    failure_reason = Some("missing-node".into());
                    break;
                }
                Err(_) => {
                    failure_reason = Some("missing-node".into());
                    break;
                }
            };

            // Requirement gating
            let req_id_opt = match &row {
                NodeRow::Door(r) => r.requirement_id,
                NodeRow::Lodestone(r) => r.requirement_id,
                NodeRow::Object(r) => r.requirement_id,
                NodeRow::Ifslot(r) => r.requirement_id,
                NodeRow::Npc(r) => r.requirement_id,
                NodeRow::Item(r) => r.requirement_id,
            };
            if let Some(req_id) = req_id_opt { if !self.passes_requirement(req_id) { failure_reason = Some("requirement-unmet".into()); break; } }

            let cost = self.node_cost(&current.type_, &row);
            let dest = self.destination_bounds(&current.type_, &row);

            links.push(ChainLink { ref_: current.clone(), cost_ms: cost, destination: dest.clone(), row: row.clone() });
            total_cost += cost;

            // Next link
            let (next_type_opt, next_id_opt) = match &row {
                NodeRow::Door(r) => (r.next_node_type.as_ref(), r.next_node_id),
                NodeRow::Lodestone(r) => (r.next_node_type.as_ref(), r.next_node_id),
                NodeRow::Object(r) => (r.next_node_type.as_ref(), r.next_node_id),
                NodeRow::Ifslot(r) => (r.next_node_type.as_ref(), r.next_node_id),
                NodeRow::Npc(r) => (r.next_node_type.as_ref(), r.next_node_id),
                NodeRow::Item(r) => (r.next_node_type.as_ref(), r.next_node_id),
            };
            match (next_type_opt, next_id_opt) {
                (Some(nt), Some(nid)) => {
                    current = NodeRef { type_: normalise_type(nt), id: nid };
                    depth += 1;
                }
                _ => break,
            }
        }

        let destination = links.last().and_then(|l| l.destination.clone());
        let mut failure_reason = failure_reason;
        if failure_reason.is_none() && destination.is_none() {
            failure_reason = Some("missing-destination".into());
        }

        ChainResolution { start: chain_start, links, total_cost_ms: total_cost, destination, failure_reason }
    }

    fn passes_requirement(&mut self, requirement_id: i32) -> bool {
        let rid = requirement_id as i32;
        if let Some(cached) = self.req_cache.get(&rid) { return cached.as_ref().map(|row| eval_requirement(row, &self.ctx_map)).unwrap_or(false); }
        let row = match self.db.fetch_requirement(rid) { Ok(r) => r, Err(_) => None };
        self.req_cache.insert(rid, row.clone());
        row.map(|r| eval_requirement(&r, &self.ctx_map)).unwrap_or(false)
    }

    fn node_cost(&self, node_type: &str, row: &NodeRow) -> i64 {
        match (node_type, row) {
            ("door", NodeRow::Door(r)) => self.cost_model.door_cost(r.cost),
            ("lodestone", NodeRow::Lodestone(r)) => self.cost_model.lodestone_cost(r.cost),
            ("object", NodeRow::Object(r)) => self.cost_model.object_cost(r.cost),
            ("ifslot", NodeRow::Ifslot(r)) => self.cost_model.ifslot_cost(r.cost),
            ("npc", NodeRow::Npc(r)) => self.cost_model.npc_cost(r.cost),
            ("item", NodeRow::Item(r)) => self.cost_model.item_cost(r.cost),
            _ => self.cost_model.step_cost_ms, // fallback shouldn't happen
        }
    }

    fn destination_bounds(&self, node_type: &str, row: &NodeRow) -> Option<Bounds2D> {
        match (node_type, row) {
            ("lodestone", NodeRow::Lodestone(r)) => Some(Bounds2D::single_tile(r.dest)),
            ("object", NodeRow::Object(r)) => Bounds2D::from_optional(r.dest_min_x, r.dest_max_x, r.dest_min_y, r.dest_max_y, r.dest_plane),
            ("ifslot", NodeRow::Ifslot(r)) => Bounds2D::from_optional(r.dest_min_x, r.dest_max_x, r.dest_min_y, r.dest_max_y, r.dest_plane),
            ("npc", NodeRow::Npc(r)) => Bounds2D::from_optional(r.dest_min_x, r.dest_max_x, r.dest_min_y, r.dest_max_y, r.dest_plane),
            ("item", NodeRow::Item(r)) => Bounds2D::from_optional(r.dest_min_x, r.dest_max_x, r.dest_min_y, r.dest_max_y, r.dest_plane),
            ("door", NodeRow::Door(r)) => {
                let a = Bounds2D::single_tile(r.tile_inside);
                let b = Bounds2D::single_tile(r.tile_outside);
                Some(a.merge(&b))
            }
            _ => None,
        }
    }
}

fn normalise_type(s: &str) -> String { s.trim().to_lowercase() }

fn build_ctx_map(opts: &SearchOptions) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    if let Some(req_map) = opts.extras.get("requirements_map").and_then(|v| v.as_object()) {
        for (k, v) in req_map {
            if let Some(i) = v.as_i64() { map.insert(k.clone(), i); }
        }
        return map;
    }
    if let Some(arr) = opts.extras.get("requirements").and_then(|v| v.as_array()) {
        for item in arr {
            if let (Some(k), Some(v)) = (item.get("key").and_then(Value::as_str), item.get("value").and_then(Value::as_i64)) {
                map.insert(k.to_string(), v);
            }
        }
    }
    map
}

fn eval_requirement(r: &RequirementRow, ctx: &HashMap<String, i64>) -> bool {
    let key = match &r.key { Some(k) => k, None => return true };
    let req_val = match r.value { Some(v) => v, None => return true };
    let actual = match ctx.get(key) { Some(v) => *v, None => return false };
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Database {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE requirements (
                id INTEGER PRIMARY KEY,
                metaInfo TEXT, key TEXT, value INTEGER, comparison TEXT
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
            CREATE TABLE object_nodes (
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
            "#,
        ).unwrap();
        Database::from_connection(conn)
    }

    #[test]
    fn detects_cycle() {
        let db = setup_db();
        // object 1 references itself -> cycle
        db.conn().execute(
            "INSERT INTO object_nodes (id, match_type, search_radius, next_node_type, next_node_id) VALUES (1, 'id', 0, 'object', 1)",
            [],
        ).unwrap();
        let opts = SearchOptions::default();
        let cm = CostModel::new(opts.clone());
        let mut r = NodeChainResolver::new(&db, &cm, &opts);
        let res = r.resolve(&NodeRef { type_: "object".into(), id: 1 });
        assert_eq!(res.failure_reason.as_deref(), Some("cycle-detected"));
        assert!(!res.is_success());
    }

    #[test]
    fn enforces_depth_limit() {
        let db = setup_db();
        // Build a 3-long chain: 1 -> 2 -> 3
        db.conn().execute("INSERT INTO object_nodes (id, match_type, search_radius, next_node_type, next_node_id) VALUES (1, 'id', 0, 'object', 2)", []).unwrap();
        db.conn().execute("INSERT INTO object_nodes (id, match_type, search_radius, next_node_type, next_node_id) VALUES (2, 'id', 0, 'object', 3)", []).unwrap();
        db.conn().execute("INSERT INTO object_nodes (id, match_type, search_radius) VALUES (3, 'id', 0)", []).unwrap();
        let mut opts = SearchOptions::default();
        opts.max_chain_depth = 2; // limit shorter than chain length
        let cm = CostModel::new(opts.clone());
        let mut r = NodeChainResolver::new(&db, &cm, &opts);
        let res = r.resolve(&NodeRef { type_: "object".into(), id: 1 });
        assert_eq!(res.failure_reason.as_deref(), Some("chain-depth-exceeded"));
        assert!(!res.is_success());
    }

    #[test]
    fn missing_destination_fails() {
        let db = setup_db();
        // object 1 -> no dest fields set
        db.conn().execute("INSERT INTO object_nodes (id, match_type, search_radius) VALUES (1, 'id', 0)", []).unwrap();
        let opts = SearchOptions::default();
        let cm = CostModel::new(opts.clone());
        let mut r = NodeChainResolver::new(&db, &cm, &opts);
        let res = r.resolve(&NodeRef { type_: "object".into(), id: 1 });
        assert_eq!(res.failure_reason.as_deref(), Some("missing-destination"));
        assert!(!res.is_success());
    }
}
