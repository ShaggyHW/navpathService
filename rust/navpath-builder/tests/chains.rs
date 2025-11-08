use std::collections::HashMap;

use navpath_builder::build::chains::flatten_chains;
use navpath_builder::build::load_sqlite::Tile;
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
            cost REAL, requirement_id INTEGER
        );
        CREATE TABLE teleports_lodestone_nodes (
            id INTEGER PRIMARY KEY,
            dest_x INTEGER, dest_y INTEGER, dest_plane INTEGER,
            next_node_type TEXT, next_node_id INTEGER,
            cost REAL, requirement_id INTEGER
        );
        CREATE TABLE teleports_npc_nodes (
            id INTEGER PRIMARY KEY,
            orig_min_x INTEGER, orig_min_y INTEGER, orig_plane INTEGER,
            dest_min_x INTEGER, dest_min_y INTEGER, dest_plane INTEGER,
            next_node_type TEXT, next_node_id INTEGER,
            cost REAL, requirement_id INTEGER
        );
        CREATE TABLE teleports_object_nodes (
            id INTEGER PRIMARY KEY,
            orig_min_x INTEGER, orig_min_y INTEGER, orig_plane INTEGER,
            dest_min_x INTEGER, dest_min_y INTEGER, dest_plane INTEGER,
            next_node_type TEXT, next_node_id INTEGER,
            cost REAL, requirement_id INTEGER
        );
        CREATE TABLE teleports_item_nodes (
            id INTEGER PRIMARY KEY,
            dest_min_x INTEGER, dest_min_y INTEGER, dest_plane INTEGER,
            next_node_type TEXT, next_node_id INTEGER,
            cost REAL, requirement_id INTEGER
        );
        CREATE TABLE teleports_ifslot_nodes (
            id INTEGER PRIMARY KEY,
            dest_min_x INTEGER, dest_min_y INTEGER, dest_plane INTEGER,
            next_node_type TEXT, next_node_id INTEGER,
            cost REAL, requirement_id INTEGER
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
        "INSERT INTO teleports_door_nodes (id, tile_outside_x, tile_outside_y, tile_outside_plane, tile_inside_x, tile_inside_y, tile_inside_plane, next_node_type, next_node_id, cost, requirement_id)
         VALUES (1, 0, 0, 0, 1, 1, 0, 'lodestone', 2, 1.0, 100)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO teleports_lodestone_nodes (id, dest_x, dest_y, dest_plane, next_node_type, next_node_id, cost, requirement_id)
         VALUES (2, 10, 10, 0, NULL, NULL, 3.5, 200)",
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
        "INSERT INTO teleports_door_nodes (id, tile_outside_x, tile_outside_y, tile_outside_plane, tile_inside_x, tile_inside_y, tile_inside_plane, next_node_type, next_node_id, cost, requirement_id)
         VALUES (1, 5, 5, 0, NULL, NULL, NULL, 'npc', 10, 1.0, 101)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO teleports_npc_nodes (id, orig_min_x, orig_min_y, orig_plane, dest_min_x, dest_min_y, dest_plane, next_node_type, next_node_id, cost, requirement_id)
         VALUES (10, 5, 5, 0, NULL, NULL, NULL, 'object', 20, 2.0, 102)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO teleports_object_nodes (id, orig_min_x, orig_min_y, orig_plane, dest_min_x, dest_min_y, dest_plane, next_node_type, next_node_id, cost, requirement_id)
         VALUES (20, 5, 5, 0, NULL, NULL, NULL, 'lodestone', 2, 3.0, 103)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO teleports_lodestone_nodes (id, dest_x, dest_y, dest_plane, next_node_type, next_node_id, cost, requirement_id)
         VALUES (2, 100, 200, 0, NULL, NULL, 4.0, 104)",
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
        "INSERT INTO teleports_door_nodes (id, tile_outside_x, tile_outside_y, tile_outside_plane, tile_inside_x, tile_inside_y, tile_inside_plane, next_node_type, next_node_id, cost, requirement_id)
         VALUES (1, 9, 9, 0, NULL, NULL, NULL, 'door', 1, 1.0, 201)",
        [],
    )
    .unwrap();

    let tiles: Vec<Tile> = vec![];
    let mut node_id_of: HashMap<(i32, i32, i32), u32> = HashMap::new();
    node_id_of.insert((9, 9, 0), 9);

    let metas = flatten_chains(&conn, &tiles, &node_id_of).unwrap();
    assert!(metas.is_empty());
}
