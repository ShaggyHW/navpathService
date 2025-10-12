use crate::db::queries::*;
use crate::db::rows::*;
use crate::models::Tile;
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;

pub mod rows;
pub mod queries;
pub mod open;
pub mod navmesh;

pub struct Database {
    conn: Connection,
}

fn is_no_such_table(err: &rusqlite::Error) -> bool {
    match err {
        rusqlite::Error::SqliteFailure(_, Some(msg)) => msg.contains("no such table"),
        _ => false,
    }
}

fn map_object_row(r: &Row) -> rusqlite::Result<ObjectNodeRow> {
    Ok(ObjectNodeRow {
        id: r.get("id")?,
        match_type: r.get("match_type")?,
        object_id: r.get("object_id")?,
        object_name: r.get("object_name")?,
        action: r.get("action")?,
        dest_min_x: r.get("dest_min_x")?,
        dest_max_x: r.get("dest_max_x")?,
        dest_min_y: r.get("dest_min_y")?,
        dest_max_y: r.get("dest_max_y")?,
        dest_plane: r.get("dest_plane")?,
        orig_min_x: r.get("orig_min_x")?,
        orig_max_x: r.get("orig_max_x")?,
        orig_min_y: r.get("orig_min_y")?,
        orig_max_y: r.get("orig_max_y")?,
        orig_plane: r.get("orig_plane")?,
        search_radius: r.get("search_radius")?,
        cost: r.get("cost")?,
        next_node_type: r.get("next_node_type")?,
        next_node_id: r.get("next_node_id")?,
        requirement_id: r.get("requirement_id")?,
    })
}

fn map_lodestone_row(r: &Row) -> rusqlite::Result<LodestoneNodeRow> {
    let dest: Tile = [r.get("dest_x")?, r.get("dest_y")?, r.get("dest_plane")?];
    Ok(LodestoneNodeRow {
        id: r.get("id")?,
        lodestone: r.get("lodestone")?,
        dest,
        cost: r.get("cost")?,
        next_node_type: r.get("next_node_type")?,
        next_node_id: r.get("next_node_id")?,
        requirement_id: r.get("requirement_id")?,
    })
}

fn map_ifslot_row(r: &Row) -> rusqlite::Result<IfslotNodeRow> {
    Ok(IfslotNodeRow {
        id: r.get("id")?,
        interface_id: r.get("interface_id")?,
        component_id: r.get("component_id")?,
        slot_id: r.get("slot_id")?,
        click_id: r.get("click_id")?,
        dest_min_x: r.get("dest_min_x")?,
        dest_max_x: r.get("dest_max_x")?,
        dest_min_y: r.get("dest_min_y")?,
        dest_max_y: r.get("dest_max_y")?,
        dest_plane: r.get("dest_plane")?,
        cost: r.get("cost")?,
        next_node_type: r.get("next_node_type")?,
        next_node_id: r.get("next_node_id")?,
        requirement_id: r.get("requirement_id")?,
    })
}

fn map_npc_row(r: &Row) -> rusqlite::Result<NpcNodeRow> {
    Ok(NpcNodeRow {
        id: r.get("id")?,
        match_type: r.get("match_type")?,
        npc_id: r.get("npc_id")?,
        npc_name: r.get("npc_name")?,
        action: r.get("action")?,
        dest_min_x: r.get("dest_min_x")?,
        dest_max_x: r.get("dest_max_x")?,
        dest_min_y: r.get("dest_min_y")?,
        dest_max_y: r.get("dest_max_y")?,
        dest_plane: r.get("dest_plane")?,
        orig_min_x: r.get("orig_min_x")?,
        orig_max_x: r.get("orig_max_x")?,
        orig_min_y: r.get("orig_min_y")?,
        orig_max_y: r.get("orig_max_y")?,
        orig_plane: r.get("orig_plane")?,
        search_radius: r.get("search_radius")?,
        cost: r.get("cost")?,
        next_node_type: r.get("next_node_type")?,
        next_node_id: r.get("next_node_id")?,
        requirement_id: r.get("requirement_id")?,
    })
}

fn map_item_row(r: &Row) -> rusqlite::Result<ItemNodeRow> {
    Ok(ItemNodeRow {
        id: r.get("id")?,
        item_id: r.get("item_id")?,
        action: r.get("action")?,
        dest_min_x: r.get("dest_min_x")?,
        dest_max_x: r.get("dest_max_x")?,
        dest_min_y: r.get("dest_min_y")?,
        dest_max_y: r.get("dest_max_y")?,
        dest_plane: r.get("dest_plane")?,
        cost: r.get("cost")?,
        next_node_type: r.get("next_node_type")?,
        next_node_id: r.get("next_node_id")?,
        requirement_id: r.get("requirement_id")?,
    })
}

impl Database {
    /// Open a SQLite database in read-only mode with PRAGMAs suitable for RO workloads.
    /// Falls back to normal open if read-only flags are unsupported by the platform.
    pub fn open_read_only<P: AsRef<Path>>(path: P) -> rusqlite::Result<Self> {
        let cfg = crate::db::open::DbOpenConfig::from_env();
        let conn = crate::db::open::open_read_only_with_config(path, &cfg)?;
        Ok(Self { conn })
    }

    /// Construct from an existing connection (useful for tests).
    pub fn from_connection(conn: Connection) -> Self { Self { conn } }

    pub fn fetch_tile(&self, x: i32, y: i32, plane: i32) -> rusqlite::Result<Option<TileRow>> {
        let mut stmt = self.conn.prepare_cached(TILE_BY_COORD)?;
        let row = stmt.query_row(params![x, y, plane], |r| map_tile_row(r)).optional()?;
        Ok(row)
    }

    pub fn iter_tiles_by_plane(&self, plane: i32) -> rusqlite::Result<Vec<TileRow>> {
        let mut stmt = self.conn.prepare_cached(TILES_BY_PLANE)?;
        let mut rows = stmt.query(params![plane])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(map_tile_row(r)?);
        }
        Ok(out)
    }

    pub fn iter_door_nodes(&self) -> rusqlite::Result<Vec<DoorNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(ALL_DOORS) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(map_door_row(r)?);
        }
        Ok(out)
    }

    pub fn iter_door_nodes_touching(&self, tile: Tile) -> rusqlite::Result<Vec<DoorNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(DOOR_BY_TILE) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let (x, y, p) = (tile[0], tile[1], tile[2]);
        let mut rows = stmt.query(params![x, y, p])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(map_door_row(r)?);
        }
        Ok(out)
    }

    pub fn fetch_requirement(&self, id: i32) -> rusqlite::Result<Option<RequirementRow>> {
        let mut stmt = self.conn.prepare_cached(REQUIREMENT_BY_ID)?;
        let row = stmt.query_row(params![id], |r| map_requirement_row(r)).optional()?;
        Ok(row)
    }

    pub fn fetch_door_node(&self, id: i32) -> rusqlite::Result<Option<DoorNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(DOOR_BY_ID) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let row = stmt.query_row(params![id], |r| map_door_row(r)).optional()?;
        Ok(row)
    }

    pub fn fetch_object_node(&self, id: i32) -> rusqlite::Result<Option<ObjectNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(OBJECT_BY_ID) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let row = stmt.query_row(params![id], |r| map_object_row(r)).optional()?;
        Ok(row)
    }

    pub fn iter_object_nodes(&self) -> rusqlite::Result<Vec<ObjectNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(ALL_OBJECTS) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? { out.push(map_object_row(r)?); }
        Ok(out)
    }

    pub fn iter_object_nodes_touching(&self, tile: Tile) -> rusqlite::Result<Vec<ObjectNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(OBJECT_BY_ORIGIN_TILE) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let (x, y, p) = (tile[0], tile[1], tile[2]);
        let mut rows = stmt.query(params![x, y, p])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? { out.push(map_object_row(r)?); }
        Ok(out)
    }

    pub fn iter_lodestone_nodes(&self) -> rusqlite::Result<Vec<LodestoneNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(ALL_LODESTONES) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? { out.push(map_lodestone_row(r)?); }
        Ok(out)
    }

    pub fn fetch_lodestone_node(&self, id: i32) -> rusqlite::Result<Option<LodestoneNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(LODESTONE_BY_ID) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let row = stmt.query_row(params![id], |r| map_lodestone_row(r)).optional()?;
        Ok(row)
    }

    pub fn iter_ifslot_nodes(&self) -> rusqlite::Result<Vec<IfslotNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(ALL_IFSLOTS) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? { out.push(map_ifslot_row(r)?); }
        Ok(out)
    }

    pub fn fetch_ifslot_node(&self, id: i32) -> rusqlite::Result<Option<IfslotNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(IFSLOT_BY_ID) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let row = stmt.query_row(params![id], |r| map_ifslot_row(r)).optional()?;
        Ok(row)
    }

    pub fn iter_npc_nodes(&self) -> rusqlite::Result<Vec<NpcNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(ALL_NPCS) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? { out.push(map_npc_row(r)?); }
        Ok(out)
    }

    pub fn iter_npc_nodes_touching(&self, tile: Tile) -> rusqlite::Result<Vec<NpcNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(NPC_BY_ORIGIN_TILE) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let (x, y, p) = (tile[0], tile[1], tile[2]);
        let mut rows = stmt.query(params![x, y, p])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? { out.push(map_npc_row(r)?); }
        Ok(out)
    }

    pub fn fetch_npc_node(&self, id: i32) -> rusqlite::Result<Option<NpcNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(NPC_BY_ID) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let row = stmt.query_row(params![id], |r| map_npc_row(r)).optional()?;
        Ok(row)
    }

    pub fn iter_item_nodes(&self) -> rusqlite::Result<Vec<ItemNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(ALL_ITEMS) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? { out.push(map_item_row(r)?); }
        Ok(out)
    }

    pub fn fetch_item_node(&self, id: i32) -> rusqlite::Result<Option<ItemNodeRow>> {
        let mut stmt = match self.conn.prepare_cached(ITEM_BY_ID) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let row = stmt.query_row(params![id], |r| map_item_row(r)).optional()?;
        Ok(row)
    }

    /// Generic fetch by node type; returns a dynamic NodeRow wrapper.
    /// Currently supports: "door", "object". Other types may be added later.
    pub fn fetch_node(&self, node_type: &str, id: i32) -> rusqlite::Result<Option<NodeRow>> {
        let t = node_type.trim().to_lowercase();
        match t.as_str() {
            "door" => Ok(self.fetch_door_node(id)?.map(NodeRow::Door)),
            "object" => Ok(self.fetch_object_node(id)?.map(NodeRow::Object)),
            "lodestone" => Ok(self.fetch_lodestone_node(id)?.map(NodeRow::Lodestone)),
            "ifslot" => Ok(self.fetch_ifslot_node(id)?.map(NodeRow::Ifslot)),
            "npc" => Ok(self.fetch_npc_node(id)?.map(NodeRow::Npc)),
            "item" => Ok(self.fetch_item_node(id)?.map(NodeRow::Item)),
            _ => Ok(None),
        }
    }

    #[cfg(test)]
    pub fn conn(&self) -> &Connection { &self.conn }
}

fn map_tile_row(r: &Row) -> rusqlite::Result<TileRow> {
    Ok(TileRow {
        x: r.get("x")?,
        y: r.get("y")?,
        plane: r.get("plane")?,
        tiledata: r.get("tiledata")?,
        allowed_directions: r.get("allowed_directions")?,
        blocked_directions: r.get("blocked_directions")?,
    })
}

fn map_requirement_row(r: &Row) -> rusqlite::Result<RequirementRow> {
    Ok(RequirementRow {
        id: r.get("id")?,
        metaInfo: r.get("metaInfo")?,
        key: r.get("key")?,
        value: r.get("value")?,
        comparison: r.get("comparison")?,
    })
}

fn map_door_row(r: &Row) -> rusqlite::Result<DoorNodeRow> {
    let inside: Tile = [r.get("tile_inside_x")?, r.get("tile_inside_y")?, r.get("tile_inside_plane")?];
    let outside: Tile = [r.get("tile_outside_x")?, r.get("tile_outside_y")?, r.get("tile_outside_plane")?];
    let loc_open: Tile = [r.get("location_open_x")?, r.get("location_open_y")?, r.get("location_open_plane")?];
    let loc_closed: Tile = [r.get("location_closed_x")?, r.get("location_closed_y")?, r.get("location_closed_plane")?];
    Ok(DoorNodeRow {
        id: r.get("id")?,
        direction: r.get("direction")?,
        tile_inside: inside,
        tile_outside: outside,
        location_open: loc_open,
        location_closed: loc_closed,
        real_id_open: r.get("real_id_open")?,
        real_id_closed: r.get("real_id_closed")?,
        cost: r.get("cost")?,
        open_action: r.get("open_action")?,
        next_node_type: r.get("next_node_type")?,
        next_node_id: r.get("next_node_id")?,
        requirement_id: r.get("requirement_id")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn setup_memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // Minimal schema for tests
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
            CREATE TABLE requirements (
                id INTEGER PRIMARY KEY,
                metaInfo TEXT, key TEXT, value INTEGER, comparison TEXT
            );
            "#,
        ).unwrap();
        conn
    }

    #[test]
    fn tiles_queries_work() {
        let conn = setup_memory_db();
        conn.execute(
            "INSERT INTO tiles (x,y,plane,tiledata,allowed_directions,blocked_directions) VALUES (1,2,0,10,'3','0')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO tiles (x,y,plane,tiledata,allowed_directions,blocked_directions) VALUES (2,2,0,11,'5','0')",
            [],
        ).unwrap();
        let db = Database::from_connection(conn);
        let t = db.fetch_tile(1,2,0).unwrap().unwrap();
        assert_eq!(t.x,1); assert_eq!(t.y,2); assert_eq!(t.plane,0);
        let rows = db.iter_tiles_by_plane(0).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].x, 1);
        assert_eq!(rows[1].x, 2);
    }

    #[test]
    fn doors_queries_work() {
        let conn = setup_memory_db();
        // Insert sample tiles for completeness (not strictly required here)
        conn.execute("INSERT INTO tiles (x,y,plane) VALUES (10,10,0)", []).unwrap();
        conn.execute("INSERT INTO tiles (x,y,plane) VALUES (11,10,0)", []).unwrap();
        // Insert a door
        conn.execute(
            r#"INSERT INTO door_nodes (
                id, direction,
                tile_inside_x, tile_inside_y, tile_inside_plane,
                tile_outside_x, tile_outside_y, tile_outside_plane,
                location_open_x, location_open_y, location_open_plane,
                location_closed_x, location_closed_y, location_closed_plane,
                real_id_open, real_id_closed, cost, open_action, next_node_type, next_node_id, requirement_id
            ) VALUES (
                1, 'north',
                10,10,0,
                11,10,0,
                10,10,0,
                11,10,0,
                1001, 1002, 50, 'Open', NULL, NULL, NULL
            )"#,
            [],
        ).unwrap();
        let db = Database::from_connection(conn);
        let all = db.iter_door_nodes().unwrap();
        assert_eq!(all.len(), 1);
        let touch_inside = db.iter_door_nodes_touching([10,10,0]).unwrap();
        assert_eq!(touch_inside.len(), 1);
        let touch_outside = db.iter_door_nodes_touching([11,10,0]).unwrap();
        assert_eq!(touch_outside.len(), 1);
        let touch_other = db.iter_door_nodes_touching([12,10,0]).unwrap();
        assert_eq!(touch_other.len(), 0);
    }

    #[test]
    fn requirement_fetch_works() {
        let conn = setup_memory_db();
        conn.execute("INSERT INTO requirements (id, metaInfo, key, value, comparison) VALUES (5, 'm', 'k', 1, '>=')", []).unwrap();
        let db = Database::from_connection(conn);
        let r = db.fetch_requirement(5).unwrap().unwrap();
        assert_eq!(r.id, 5);
        assert_eq!(r.key.as_deref(), Some("k"));
    }

    #[test]
    fn object_queries_work() {
        let conn = setup_memory_db();
        conn.execute_batch(
            r#"
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
        conn.execute(
            r#"INSERT INTO object_nodes (
                id, match_type, object_id, object_name, action,
                dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                search_radius, cost, next_node_type, next_node_id, requirement_id
            ) VALUES (
                1, 'id', 100, 'Obj', 'Open',
                1000,1000,1000,1000,0,
                19,21,29,31,0,
                5, 10, NULL, NULL, NULL
            )"#,
            [],
        ).unwrap();
        let db = Database::from_connection(conn);
        let all = db.iter_object_nodes().unwrap();
        assert_eq!(all.len(), 1);
        let touching = db.iter_object_nodes_touching([20,30,0]).unwrap();
        assert_eq!(touching.len(), 1);
        let not_touching = db.iter_object_nodes_touching([5,5,0]).unwrap();
        assert_eq!(not_touching.len(), 0);
        let fetched = db.fetch_object_node(1).unwrap().unwrap();
        assert_eq!(fetched.id, 1);
        let n = db.fetch_node("object", 1).unwrap().unwrap();
        match n { NodeRow::Object(o) => assert_eq!(o.id, 1), _ => panic!("wrong variant") }
    }

    #[test]
    fn lodestone_queries_work() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE lodestone_nodes (
                id INTEGER PRIMARY KEY,
                lodestone TEXT,
                dest_x INTEGER, dest_y INTEGER, dest_plane INTEGER,
                cost INTEGER,
                next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );
            "#,
        ).unwrap();
        conn.execute(
            "INSERT INTO lodestone_nodes (id,lodestone,dest_x,dest_y,dest_plane,cost,next_node_type,next_node_id,requirement_id) VALUES (1,'Varrock',3200,3200,0,50,NULL,NULL,NULL)",
            [],
        ).unwrap();
        let db = Database::from_connection(conn);
        let all = db.iter_lodestone_nodes().unwrap();
        assert_eq!(all.len(), 1);
        let fetched = db.fetch_lodestone_node(1).unwrap().unwrap();
        assert_eq!(fetched.lodestone, "Varrock");
        let n = db.fetch_node("lodestone", 1).unwrap().unwrap();
        match n { NodeRow::Lodestone(l) => assert_eq!(l.id, 1), _ => panic!("wrong variant") }
    }

    #[test]
    fn ifslot_queries_work() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE ifslot_nodes (
                id INTEGER PRIMARY KEY,
                interface_id INTEGER, component_id INTEGER, slot_id INTEGER, click_id INTEGER,
                dest_min_x INTEGER, dest_max_x INTEGER, dest_min_y INTEGER, dest_max_y INTEGER, dest_plane INTEGER,
                cost INTEGER,
                next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );
            "#,
        ).unwrap();
        conn.execute(
            "INSERT INTO ifslot_nodes (id,interface_id,component_id,slot_id,click_id,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,cost,next_node_type,next_node_id,requirement_id) VALUES (1, 548, 12, NULL, 0, 3000,3001,4000,4000,0,20,NULL,NULL,NULL)",
            [],
        ).unwrap();
        let db = Database::from_connection(conn);
        let all = db.iter_ifslot_nodes().unwrap();
        assert_eq!(all.len(), 1);
        let fetched = db.fetch_ifslot_node(1).unwrap().unwrap();
        assert_eq!(fetched.interface_id, 548);
        let n = db.fetch_node("ifslot", 1).unwrap().unwrap();
        match n { NodeRow::Ifslot(i) => assert_eq!(i.id, 1), _ => panic!("wrong variant") }
    }

    #[test]
    fn npc_queries_work() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE npc_nodes (
                id INTEGER PRIMARY KEY,
                match_type TEXT,
                npc_id INTEGER, npc_name TEXT, action TEXT,
                dest_min_x INTEGER, dest_max_x INTEGER, dest_min_y INTEGER, dest_max_y INTEGER, dest_plane INTEGER,
                orig_min_x INTEGER, orig_max_x INTEGER, orig_min_y INTEGER, orig_max_y INTEGER, orig_plane INTEGER,
                search_radius INTEGER,
                cost INTEGER,
                next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );
            "#,
        ).unwrap();
        conn.execute(
            r#"INSERT INTO npc_nodes (
                id, match_type, npc_id, npc_name, action,
                dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                search_radius, cost, next_node_type, next_node_id, requirement_id
            ) VALUES (
                1, 'id', 200, 'Bob', 'Talk-to',
                3201,3201,3201,3201,0,
                10,12,20,22,0,
                3, 5, NULL, NULL, NULL
            )"#,
            [],
        ).unwrap();
        let db = Database::from_connection(conn);
        let all = db.iter_npc_nodes().unwrap();
        assert_eq!(all.len(), 1);
        let touching = db.iter_npc_nodes_touching([11,21,0]).unwrap();
        assert_eq!(touching.len(), 1);
        let not_touching = db.iter_npc_nodes_touching([50,50,0]).unwrap();
        assert_eq!(not_touching.len(), 0);
        let fetched = db.fetch_npc_node(1).unwrap().unwrap();
        assert_eq!(fetched.npc_name.as_deref(), Some("Bob"));
        let n = db.fetch_node("npc", 1).unwrap().unwrap();
        match n { NodeRow::Npc(npc) => assert_eq!(npc.id, 1), _ => panic!("wrong variant") }
    }

    #[test]
    fn item_queries_work() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE item_nodes (
                id INTEGER PRIMARY KEY,
                item_id INTEGER, action TEXT,
                dest_min_x INTEGER, dest_max_x INTEGER, dest_min_y INTEGER, dest_max_y INTEGER, dest_plane INTEGER,
                cost INTEGER,
                next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );
            "#,
        ).unwrap();
        conn.execute(
            "INSERT INTO item_nodes (id,item_id,action,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,cost,next_node_type,next_node_id,requirement_id) VALUES (1, 4151, 'Wield', 3210,3210,3210,3210,0,15,NULL,NULL,NULL)",
            [],
        ).unwrap();
        let db = Database::from_connection(conn);
        let all = db.iter_item_nodes().unwrap();
        assert_eq!(all.len(), 1);
        let fetched = db.fetch_item_node(1).unwrap().unwrap();
        assert_eq!(fetched.item_id, Some(4151));
        let n = db.fetch_node("item", 1).unwrap().unwrap();
        match n { NodeRow::Item(it) => assert_eq!(it.id, 1), _ => panic!("wrong variant") }
    }

    #[test]
    fn missing_tables_graceful() {
        let conn = Connection::open_in_memory().unwrap();
        let db = Database::from_connection(conn);
        assert_eq!(db.iter_object_nodes().unwrap().len(), 0);
        assert_eq!(db.iter_lodestone_nodes().unwrap().len(), 0);
        assert_eq!(db.iter_ifslot_nodes().unwrap().len(), 0);
        assert_eq!(db.iter_npc_nodes().unwrap().len(), 0);
        assert_eq!(db.iter_item_nodes().unwrap().len(), 0);
        assert!(db.fetch_object_node(1).unwrap().is_none());
        assert!(db.fetch_lodestone_node(1).unwrap().is_none());
        assert!(db.fetch_ifslot_node(1).unwrap().is_none());
        assert!(db.fetch_npc_node(1).unwrap().is_none());
        assert!(db.fetch_item_node(1).unwrap().is_none());
    }

    #[test]
    fn open_read_only_allows_reads_and_blocks_writes() {
        // Create a temp on-disk DB
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
        let path = std::env::temp_dir().join(format!("navpath_ro_{}.db", ts));
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE tiles (
                    x INTEGER, y INTEGER, plane INTEGER,
                    tiledata INTEGER, allowed_directions TEXT, blocked_directions TEXT
                );
                "#,
            ).unwrap();
            conn.execute(
                "INSERT INTO tiles (x,y,plane,tiledata,allowed_directions,blocked_directions) VALUES (3,4,0,12,'3','0')",
                [],
            ).unwrap();
        }

        // Configure env toggles (ensure query_only ON)
        env::set_var("NAVPATH_SQLITE_QUERY_ONLY", "1");
        // Open read-only via Database
        let db = Database::open_read_only(&path).unwrap();

        // Read should work
        let t = db.fetch_tile(3,4,0).unwrap().unwrap();
        assert_eq!(t.x, 3);

        // Write should fail due to read-only open
        let write_attempt = db.conn().execute("CREATE TABLE should_fail (a)", []);
        assert!(write_attempt.is_err());
    }
}
