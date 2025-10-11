use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use crate::db::Database;

/// Immutable index of nodes that are referenced by another node's `next_node`.
/// Such nodes are NOT chain-heads and should be filtered from head generation.
#[derive(Debug, Default)]
pub struct ChainHeadIndex {
    // key: lowercased node type (e.g., "door", "object", ...)
    // val: set of node IDs that are referenced by `next_node` somewhere
    non_heads: HashMap<String, HashSet<i32>>,
}

impl ChainHeadIndex {
    pub fn new() -> Self { Self { non_heads: HashMap::new() } }

    pub fn add_non_head(&mut self, node_type: &str, id: i32) {
        let key = node_type.trim().to_lowercase();
        self.non_heads.entry(key).or_default().insert(id);
    }

    pub fn is_non_head<T: AsRef<str>>(&self, node_type: T, id: i32) -> bool {
        let key = node_type.as_ref().trim().to_lowercase();
        self.non_heads.get(&key).map(|s| s.contains(&id)).unwrap_or(false)
    }
}

/// Build-once handle for a provider-scoped chain-head index.
/// Intended to be embedded in a provider struct and initialized lazily.
#[derive(Debug, Default)]
pub struct ChainHeadIndexState {
    inner: OnceLock<Arc<ChainHeadIndex>>, // set exactly once per provider instance
}

impl ChainHeadIndexState {
    pub fn new() -> Self { Self { inner: OnceLock::new() } }

    /// Ensure the index is built exactly once for this provider, then return it.
    /// Safe for concurrent callers.
    pub fn ensure_built(&self, db: &Database) -> Arc<ChainHeadIndex> {
        // get_or_init ensures single initialization even under concurrency
        let arc = self.inner.get_or_init(|| Arc::new(build_chain_head_index(db)));
        Arc::clone(arc)
    }

    /// Returns Some(index) if it was already built, else None.
    pub fn get(&self) -> Option<Arc<ChainHeadIndex>> { self.inner.get().cloned() }
}

/// Construct the chain-head index by scanning node tables for `next_node` references.
/// This implementation is resilient to partial DB implementations: if a query isn't
/// available or returns an error, it is skipped.
pub fn build_chain_head_index(db: &Database) -> ChainHeadIndex {
    let mut idx = ChainHeadIndex::new();

    // Doors
    if let Ok(rows) = db.iter_door_nodes() {
        for r in rows {
            if let (Some(t), Some(id)) = (r.next_node_type.as_ref(), r.next_node_id) {
                idx.add_non_head(t, id);
            }
        }
    }

    // Lodestones
    if let Ok(rows) = db.iter_lodestone_nodes() {
        for r in rows {
            if let (Some(t), Some(id)) = (r.next_node_type.as_ref(), r.next_node_id) {
                idx.add_non_head(t, id);
            }
        }
    }

    // Objects
    if let Ok(rows) = db.iter_object_nodes() {
        for r in rows {
            if let (Some(t), Some(id)) = (r.next_node_type.as_ref(), r.next_node_id) {
                idx.add_non_head(t, id);
            }
        }
    }

    // Ifslots
    if let Ok(rows) = db.iter_ifslot_nodes() {
        for r in rows {
            if let (Some(t), Some(id)) = (r.next_node_type.as_ref(), r.next_node_id) {
                idx.add_non_head(t, id);
            }
        }
    }

    // NPCs
    if let Ok(rows) = db.iter_npc_nodes() {
        for r in rows {
            if let (Some(t), Some(id)) = (r.next_node_type.as_ref(), r.next_node_id) {
                idx.add_non_head(t, id);
            }
        }
    }

    // Items
    if let Ok(rows) = db.iter_item_nodes() {
        for r in rows {
            if let (Some(t), Some(id)) = (r.next_node_type.as_ref(), r.next_node_id) {
                idx.add_non_head(t, id);
            }
        }
    }

    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn index_marks_referenced_nodes_as_non_heads() {
        // Minimal in-memory schema to exercise door_nodes scanning
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
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

        // Insert a couple of rows that reference other node types
        conn.execute(
            "INSERT INTO door_nodes (id, direction, tile_inside_x, tile_inside_y, tile_inside_plane, \
             tile_outside_x, tile_outside_y, tile_outside_plane, location_open_x, location_open_y, location_open_plane, \
             location_closed_x, location_closed_y, location_closed_plane, real_id_open, real_id_closed, cost, open_action, next_node_type, next_node_id, requirement_id) \
             VALUES (1, 'north', 0,0,0, 1,0,0, 0,0,0, 1,0,0, 100, 200, 5, 'Open', 'object', 42, NULL)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO door_nodes (id, direction, tile_inside_x, tile_inside_y, tile_inside_plane, \
             tile_outside_x, tile_outside_y, tile_outside_plane, location_open_x, location_open_y, location_open_plane, \
             location_closed_x, location_closed_y, location_closed_plane, real_id_open, real_id_closed, cost, open_action, next_node_type, next_node_id, requirement_id) \
             VALUES (2, 'south', 1,0,0, 2,0,0, 1,0,0, 2,0,0, 101, 201, 10, NULL, 'lodestone', 7, NULL)",
            [],
        ).unwrap();

        let db = Database::from_connection(conn);
        let idx = build_chain_head_index(&db);
        assert!(idx.is_non_head("object", 42));
        assert!(idx.is_non_head("lodestone", 7));
        assert!(!idx.is_non_head("object", 99));
        assert!(!idx.is_non_head("npc", 1));
    }

    #[test]
    fn state_builds_once_and_is_shared() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"CREATE TABLE door_nodes (
                id INTEGER PRIMARY KEY,
                direction TEXT,
                tile_inside_x INTEGER, tile_inside_y INTEGER, tile_inside_plane INTEGER,
                tile_outside_x INTEGER, tile_outside_y INTEGER, tile_outside_plane INTEGER,
                location_open_x INTEGER, location_open_y INTEGER, location_open_plane INTEGER,
                location_closed_x INTEGER, location_closed_y INTEGER, location_closed_plane INTEGER,
                real_id_open INTEGER, real_id_closed INTEGER,
                cost INTEGER, open_action TEXT, next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );"#,
        ).unwrap();
        let db = Database::from_connection(conn);
        let state = ChainHeadIndexState::new();
        let a = state.ensure_built(&db);
        let b = state.ensure_built(&db);
        // Same Arc instance
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn index_marks_non_heads_across_all_node_types() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
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
            CREATE TABLE lodestone_nodes (
                id INTEGER PRIMARY KEY,
                lodestone TEXT,
                dest_x INTEGER, dest_y INTEGER, dest_plane INTEGER,
                cost INTEGER,
                next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
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
            CREATE TABLE ifslot_nodes (
                id INTEGER PRIMARY KEY,
                interface_id INTEGER, component_id INTEGER, slot_id INTEGER, click_id INTEGER,
                dest_min_x INTEGER, dest_max_x INTEGER, dest_min_y INTEGER, dest_max_y INTEGER, dest_plane INTEGER,
                cost INTEGER,
                next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );
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
            CREATE TABLE item_nodes (
                id INTEGER PRIMARY KEY,
                item_id INTEGER, action TEXT,
                dest_min_x INTEGER, dest_max_x INTEGER, dest_min_y INTEGER, dest_max_y INTEGER, dest_plane INTEGER,
                cost INTEGER,
                next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );
            "#,
        ).unwrap();

        // Insert one row per table, each referencing a different target type/id
        conn.execute(
            r#"INSERT INTO door_nodes (
                id, direction,
                tile_inside_x, tile_inside_y, tile_inside_plane,
                tile_outside_x, tile_outside_y, tile_outside_plane,
                location_open_x, location_open_y, location_open_plane,
                location_closed_x, location_closed_y, location_closed_plane,
                real_id_open, real_id_closed, cost, open_action, next_node_type, next_node_id, requirement_id
            ) VALUES (
                10, 'north', 0,0,0, 1,0,0, 0,0,0, 1,0,0, 100,200, 5, 'Open', 'object', 42, NULL
            )"#,
            [],
        ).unwrap();

        conn.execute(
            "INSERT INTO lodestone_nodes (id,lodestone,dest_x,dest_y,dest_plane,cost,next_node_type,next_node_id,requirement_id) \
             VALUES (11,'Falador',3200,3200,0,50,'npc',17,NULL)",
            [],
        ).unwrap();

        conn.execute(
            r#"INSERT INTO object_nodes (
                id, match_type, object_id, object_name, action,
                dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                search_radius, cost, next_node_type, next_node_id, requirement_id
            ) VALUES (
                12, 'id', 100, 'Obj', 'Open',
                1000,1000,1000,1000,0,
                10,10,10,10,0,
                3, 10, 'ifslot', 99, NULL
            )"#,
            [],
        ).unwrap();

        conn.execute(
            "INSERT INTO ifslot_nodes (id,interface_id,component_id,slot_id,click_id,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,cost,next_node_type,next_node_id,requirement_id) \
             VALUES (13, 548, 12, NULL, 0, 3000,3001,4000,4000,0,20,'item',5,NULL)",
            [],
        ).unwrap();

        conn.execute(
            r#"INSERT INTO npc_nodes (
                id, match_type, npc_id, npc_name, action,
                dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                search_radius, cost, next_node_type, next_node_id, requirement_id
            ) VALUES (
                14, 'id', 200, 'Bob', 'Talk-to',
                3201,3201,3201,3201,0,
                10,12,20,22,0,
                3, 5, 'lodestone', 7, NULL
            )"#,
            [],
        ).unwrap();

        conn.execute(
            "INSERT INTO item_nodes (id,item_id,action,dest_min_x,dest_max_x,dest_min_y,dest_max_y,dest_plane,cost,next_node_type,next_node_id,requirement_id) \
             VALUES (15, 4151, 'Wield', 3210,3210,3210,3210,0,15,'door',3,NULL)",
            [],
        ).unwrap();

        let db = Database::from_connection(conn);
        let idx = build_chain_head_index(&db);

        // Verify non-heads recorded across all node types
        assert!(idx.is_non_head("object", 42));
        assert!(idx.is_non_head("npc", 17));
        assert!(idx.is_non_head("ifslot", 99));
        assert!(idx.is_non_head("item", 5));
        assert!(idx.is_non_head("lodestone", 7));
        assert!(idx.is_non_head("door", 3));

        // Negative checks
        assert!(!idx.is_non_head("object", 41));
        assert!(!idx.is_non_head("npc", 18));
        assert!(!idx.is_non_head("ifslot", 100));
        assert!(!idx.is_non_head("item", 6));
        assert!(!idx.is_non_head("lodestone", 8));
        assert!(!idx.is_non_head("door", 4));
    }
}
