use std::collections::HashMap;

use navpath_builder::build::chains::flatten_chains;
use navpath_builder::build::load_sqlite::{Tile, load_fairy_rings};
use rusqlite::{Connection, OpenFlags};

fn mem_conn() -> Connection {
    Connection::open_in_memory_with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap()
}

fn create_schema(conn: &Connection) {
    // Minimal schemas with only the columns we use
    conn.execute_batch(
        r#"
        CREATE TABLE teleports_door_nodes (
            id INTEGER PRIMARY KEY,
            tile_outside_x INTEGER, tile_outside_y INTEGER, tile_outside_plane INTEGER,
            tile_inside_x INTEGER, tile_inside_y INTEGER, tile_inside_plane INTEGER,
            next_node_type TEXT, next_node_id INTEGER,
            cost REAL, requirements TEXT
        );
        CREATE TABLE teleports_lodestone_nodes (
            id INTEGER PRIMARY KEY,
            dest_x INTEGER, dest_y INTEGER, dest_plane INTEGER,
            next_node_type TEXT, next_node_id INTEGER,
            cost REAL, requirements TEXT
        );
        CREATE TABLE teleports_npc_nodes (
            id INTEGER PRIMARY KEY,
            orig_min_x INTEGER, orig_min_y INTEGER, orig_plane INTEGER,
            dest_min_x INTEGER, dest_min_y INTEGER, dest_plane INTEGER,
            next_node_type TEXT, next_node_id INTEGER,
            cost REAL, requirements TEXT
        );
        CREATE TABLE teleports_object_nodes (
            id INTEGER PRIMARY KEY,
            orig_min_x INTEGER, orig_min_y INTEGER, orig_plane INTEGER,
            dest_min_x INTEGER, dest_min_y INTEGER, dest_plane INTEGER,
            next_node_type TEXT, next_node_id INTEGER,
            cost REAL, requirements TEXT
        );
        CREATE TABLE teleports_item_nodes (
            id INTEGER PRIMARY KEY,
            dest_min_x INTEGER, dest_min_y INTEGER, dest_plane INTEGER,
            next_node_type TEXT, next_node_id INTEGER,
            cost REAL, requirements TEXT
        );
        CREATE TABLE teleports_ifslot_nodes (
            id INTEGER PRIMARY KEY,
            dest_min_x INTEGER, dest_min_y INTEGER, dest_plane INTEGER,
            next_node_type TEXT, next_node_id INTEGER,
            cost REAL, requirements TEXT
        );
        CREATE TABLE teleports_fairy_rings_nodes (
            id INTEGER PRIMARY KEY,
            object_id INTEGER,
            x INTEGER, y INTEGER, plane INTEGER,
            cost REAL,
            code TEXT,
            action TEXT,
            next_node_type TEXT, next_node_id INTEGER,
            requirements TEXT
        );
        "#,
    )
    .unwrap();
}

#[test]
fn two_step_door_to_lodestone() {
    let conn = mem_conn();
    create_schema(&conn);

    // door(1) -> lodestone(2)
    conn.execute(
        "INSERT INTO teleports_door_nodes (id, tile_outside_x, tile_outside_y, tile_outside_plane, tile_inside_x, tile_inside_y, tile_inside_plane, next_node_type, next_node_id, cost, requirements)
         VALUES (1, 0, 0, 0, 1, 1, 0, 'lodestone', 2, 1.0, '100')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO teleports_lodestone_nodes (id, dest_x, dest_y, dest_plane, next_node_type, next_node_id, cost, requirements)
         VALUES (2, 10, 10, 0, NULL, NULL, 3.5, '200')",
        [],
    )
    .unwrap();

    let tiles: Vec<Tile> = vec![]; // not used by flatten_chains
    let mut node_id_of: HashMap<(i32, i32, i32), u32> = HashMap::new();
    node_id_of.insert((0, 0, 0), 0);
    node_id_of.insert((10, 10, 0), 1);

    let metas = flatten_chains(&conn, &tiles, &node_id_of).unwrap();
    assert_eq!(metas.len(), 1);
    let m = &metas[0];
    assert_eq!(m.src, 0);
    assert_eq!(m.dst, 1);
    assert!((m.cost - 4.5).abs() < 1e-5);
    assert_eq!(m.steps.len(), 2);
    assert_eq!(m.requirement_ids, vec![100, 200]);
}

#[test]
fn four_step_chain_door_npc_object_lodestone() {
    let conn = mem_conn();
    create_schema(&conn);

    // door(1) -> npc(10) -> object(20) -> lodestone(2)
    conn.execute(
        "INSERT INTO teleports_door_nodes (id, tile_outside_x, tile_outside_y, tile_outside_plane, tile_inside_x, tile_inside_y, tile_inside_plane, next_node_type, next_node_id, cost, requirements)
         VALUES (1, 5, 5, 0, NULL, NULL, NULL, 'npc', 10, 1.0, '101')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO teleports_npc_nodes (id, orig_min_x, orig_min_y, orig_plane, dest_min_x, dest_min_y, dest_plane, next_node_type, next_node_id, cost, requirements)
         VALUES (10, 5, 5, 0, NULL, NULL, NULL, 'object', 20, 2.0, '102')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO teleports_object_nodes (id, orig_min_x, orig_min_y, orig_plane, dest_min_x, dest_min_y, dest_plane, next_node_type, next_node_id, cost, requirements)
         VALUES (20, 5, 5, 0, NULL, NULL, NULL, 'lodestone', 2, 3.0, '103')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO teleports_lodestone_nodes (id, dest_x, dest_y, dest_plane, next_node_type, next_node_id, cost, requirements)
         VALUES (2, 100, 200, 0, NULL, NULL, 4.0, '104')",
        [],
    )
    .unwrap();

    let tiles: Vec<Tile> = vec![];
    let mut node_id_of: HashMap<(i32, i32, i32), u32> = HashMap::new();
    node_id_of.insert((5, 5, 0), 0);
    node_id_of.insert((100, 200, 0), 77);

    let metas = flatten_chains(&conn, &tiles, &node_id_of).unwrap();
    assert_eq!(metas.len(), 1);
    let m = &metas[0];
    assert_eq!(m.src, 0);
    assert_eq!(m.dst, 77);
    assert!((m.cost - 10.0).abs() < 1e-5);
    assert_eq!(m.steps.len(), 4);
    let mut reqs = m.requirement_ids.clone();
    reqs.sort();
    assert_eq!(reqs, vec![101, 102, 103, 104]);
}

#[test]
fn cycle_is_dropped() {
    let conn = mem_conn();
    create_schema(&conn);

    // door(1) -> door(1) cycle
    conn.execute(
        "INSERT INTO teleports_door_nodes (id, tile_outside_x, tile_outside_y, tile_outside_plane, tile_inside_x, tile_inside_y, tile_inside_plane, next_node_type, next_node_id, cost, requirements)
         VALUES (1, 9, 9, 0, NULL, NULL, NULL, 'door', 1, 1.0, '201')",
        [],
    )
    .unwrap();

    let tiles: Vec<Tile> = vec![];
    let mut node_id_of: HashMap<(i32, i32, i32), u32> = HashMap::new();
    node_id_of.insert((9, 9, 0), 9);

    let metas = flatten_chains(&conn, &tiles, &node_id_of).unwrap();
    assert!(metas.is_empty());
}

#[test]
fn load_fairy_rings_basic() {
    let conn = mem_conn();
    create_schema(&conn);

    // Insert two fairy ring nodes
    conn.execute(
        "INSERT INTO teleports_fairy_rings_nodes (id, object_id, x, y, plane, cost, code, action, next_node_type, next_node_id, requirements)
         VALUES (1, 12345, 3200, 3200, 0, 600.0, 'ALS', 'Ring-configure', NULL, NULL, '50;51')",
        [],
    ).unwrap();
    conn.execute(
        "INSERT INTO teleports_fairy_rings_nodes (id, object_id, x, y, plane, cost, code, action, next_node_type, next_node_id, requirements)
         VALUES (2, 12346, 3250, 3250, 0, 650.0, 'DKS', 'Ring-dial', NULL, NULL, '52')",
        [],
    ).unwrap();

    let mut node_id_of: HashMap<(i32, i32, i32), u32> = HashMap::new();
    node_id_of.insert((3200, 3200, 0), 100);
    node_id_of.insert((3250, 3250, 0), 101);

    let rings = load_fairy_rings(&conn, &node_id_of).unwrap();
    assert_eq!(rings.len(), 2);

    // First ring
    assert_eq!(rings[0].id, 1);
    assert_eq!(rings[0].node_id, 100);
    assert_eq!(rings[0].object_id, 12345);
    assert_eq!(rings[0].x, 3200);
    assert_eq!(rings[0].y, 3200);
    assert_eq!(rings[0].plane, 0);
    assert!((rings[0].cost - 600.0).abs() < 1e-5);
    assert_eq!(rings[0].code, "ALS");
    assert_eq!(rings[0].action, Some("Ring-configure".to_string()));
    assert_eq!(rings[0].requirements, vec![50, 51]);

    // Second ring
    assert_eq!(rings[1].id, 2);
    assert_eq!(rings[1].node_id, 101);
    assert_eq!(rings[1].object_id, 12346);
    assert_eq!(rings[1].code, "DKS");
    assert_eq!(rings[1].requirements, vec![52]);
}

#[test]
fn load_fairy_rings_skips_missing_tiles() {
    let conn = mem_conn();
    create_schema(&conn);

    // Insert a fairy ring at coordinates not in node_id_of
    conn.execute(
        "INSERT INTO teleports_fairy_rings_nodes (id, object_id, x, y, plane, cost, code, action, next_node_type, next_node_id, requirements)
         VALUES (1, 99999, 9999, 9999, 0, 500.0, 'XXX', NULL, NULL, NULL, NULL)",
        [],
    ).unwrap();
    // Insert one that exists
    conn.execute(
        "INSERT INTO teleports_fairy_rings_nodes (id, object_id, x, y, plane, cost, code, action, next_node_type, next_node_id, requirements)
         VALUES (2, 11111, 1000, 1000, 0, 500.0, 'CKS', NULL, NULL, NULL, NULL)",
        [],
    ).unwrap();

    let mut node_id_of: HashMap<(i32, i32, i32), u32> = HashMap::new();
    node_id_of.insert((1000, 1000, 0), 200);
    // (9999, 9999, 0) not in map

    let rings = load_fairy_rings(&conn, &node_id_of).unwrap();
    assert_eq!(rings.len(), 1);
    assert_eq!(rings[0].code, "CKS");
    assert_eq!(rings[0].node_id, 200);
}
