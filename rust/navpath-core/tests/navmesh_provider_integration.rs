use navpath_core::{CostModel, Database, SearchOptions};
use navpath_core::graph::navmesh_provider::NavmeshGraphProvider;
use navpath_core::graph::provider::GraphProvider;
use navpath_core::astar::AStar;
use navpath_core::funnel::string_pull;
use rusqlite::Connection;

fn wkb_polygon_of_ring(points: &[[f64; 2]]) -> Vec<u8> {
    let mut pts = points.to_vec();
    if pts.first() != pts.last() { pts.push(points[0]); }
    let npoints = pts.len() as u32;
    let mut out = Vec::with_capacity(1 + 4 + 4 + 4 + (npoints as usize) * 16);
    out.push(1u8); // little-endian
    out.extend_from_slice(&3u32.to_le_bytes()); // Polygon
    out.extend_from_slice(&1u32.to_le_bytes()); // 1 ring
    out.extend_from_slice(&npoints.to_le_bytes());
    for p in &pts {
        out.extend_from_slice(&p[0].to_le_bytes());
        out.extend_from_slice(&p[1].to_le_bytes());
    }
    out
}

fn setup_navmesh_db() -> Database {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE cells (
            id INTEGER PRIMARY KEY,
            plane INTEGER NOT NULL,
            kind TEXT NOT NULL,
            wkb BLOB NOT NULL,
            area REAL NOT NULL,
            minx REAL NOT NULL,
            miny REAL NOT NULL,
            maxx REAL NOT NULL,
            maxy REAL NOT NULL
        );
        CREATE VIRTUAL TABLE rtree_cells USING rtree(id, minx, maxx, miny, maxy);
        CREATE TABLE portals (
            id INTEGER PRIMARY KEY,
            plane INTEGER NOT NULL,
            a_id INTEGER NOT NULL,
            b_id INTEGER NOT NULL,
            x1 REAL NOT NULL,
            y1 REAL NOT NULL,
            x2 REAL NOT NULL,
            y2 REAL NOT NULL,
            length REAL NOT NULL
        );
        CREATE TABLE offmesh_links (
            id INTEGER PRIMARY KEY,
            link_type TEXT NOT NULL,
            node_table TEXT NOT NULL,
            node_id INTEGER NOT NULL,
            requirement_id INTEGER NULL,
            cost REAL NULL,
            plane_from INTEGER NULL,
            plane_to INTEGER NOT NULL,
            src_cell_id INTEGER NULL,
            dst_cell_id INTEGER NOT NULL,
            meta_json TEXT NULL
        );
        CREATE TABLE requirements (
            id INTEGER PRIMARY KEY,
            metaInfo TEXT, key TEXT, value INTEGER, comparison TEXT
        );
        "#,
    ).unwrap();

    // Two adjacent unit squares on plane 0: cell 1 at [0,0]-[1,1], cell 2 at [1,0]-[2,1]
    let ring1 = vec![[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,1.0]];
    let ring2 = vec![[1.0,0.0],[2.0,0.0],[2.0,1.0],[1.0,1.0]];
    let wkb1 = wkb_polygon_of_ring(&ring1);
    let wkb2 = wkb_polygon_of_ring(&ring2);
    conn.execute("INSERT INTO cells (id,plane,kind,wkb,area,minx,miny,maxx,maxy) VALUES (1,0,'polygon',?1,1.0,0.0,0.0,1.0,1.0)", [wkb1]).unwrap();
    conn.execute("INSERT INTO cells (id,plane,kind,wkb,area,minx,miny,maxx,maxy) VALUES (2,0,'polygon',?1,1.0,1.0,0.0,2.0,1.0)", [wkb2]).unwrap();
    conn.execute("INSERT INTO rtree_cells (id,minx,maxx,miny,maxy) VALUES (1,0.0,1.0,0.0,1.0)", []).unwrap();
    conn.execute("INSERT INTO rtree_cells (id,minx,maxx,miny,maxy) VALUES (2,1.0,2.0,0.0,1.0)", []).unwrap();

    // Portal between cell 1 and 2 along x=1, y in [0.25,0.75]
    conn.execute("INSERT INTO portals (id,plane,a_id,b_id,x1,y1,x2,y2,length) VALUES (10,0,1,2,1.0,0.25,1.0,0.75,0.5)", []).unwrap();

    // Requirement and an offmesh link from cell 1 to cell 2 (as example), gated
    conn.execute("INSERT INTO requirements (id, metaInfo, key, value, comparison) VALUES (7, 'lvl', 'magic', 50, '>=')", []).unwrap();
    conn.execute(
        "INSERT INTO offmesh_links (id,link_type,node_table,node_id,requirement_id,cost,plane_from,plane_to,src_cell_id,dst_cell_id,meta_json) VALUES (20,'door','door_nodes',5,7,10.0,0,0,1,2,NULL)",
        [],
    ).unwrap();

    Database::from_connection(conn)
}

#[test]
fn mapping_and_portal_neighbors_work_and_are_sorted() {
    let db = setup_navmesh_db();
    let prov = NavmeshGraphProvider::new(db, CostModel::default());

    // Map start/goal
    let s = prov.map_point_to_tile(0.5, 0.5, 0).unwrap().expect("start cell");
    let g = prov.map_point_to_tile(1.5, 0.5, 0).unwrap().expect("goal cell");
    assert_eq!(s[0], 1);
    assert_eq!(g[0], 2);

    // Neighbors from cell 1 include portal to cell 2
    let opts = SearchOptions::default();
    let edges = prov.neighbors(s, g, &opts).unwrap();
    assert!(edges.iter().any(|e| e.type_ == "move" && e.to_tile[0] == 2));

    // Deterministic sort by (to_cell, id) implies the portal move appears before offmesh when both exist
    let mut idx_move = None;
    let mut idx_off = None;
    for (i, e) in edges.iter().enumerate() {
        if e.type_ == "move" && e.to_tile[0] == 2 { idx_move = Some(i); }
        if e.type_ == "door" && e.to_tile[0] == 2 { idx_off = Some(i); }
    }
    if let (Some(im), Some(io)) = (idx_move, idx_off) {
        assert!(im < io, "portal move should sort before offmesh to same dst cell");
    }
}

#[test]
fn offmesh_requirement_gating_respected() {
    let db = setup_navmesh_db();
    let prov = NavmeshGraphProvider::new(db, CostModel::default());
    let s = prov.map_point_to_tile(0.5, 0.5, 0).unwrap().unwrap();
    let g = prov.map_point_to_tile(1.5, 0.5, 0).unwrap().unwrap();

    // Without requirements map -> gated out
    let opts = SearchOptions::default();
    let edges = prov.neighbors(s, g, &opts).unwrap();
    let has_door = edges.iter().any(|e| e.type_ == "door");
    assert!(!has_door, "door offmesh should be gated without requirements_map");

    // With requirements_map magic>=50 -> allowed
    let mut opts2 = SearchOptions::default();
    opts2.extras.insert("requirements_map".into(), serde_json::json!({"magic": 50}));
    let edges2 = prov.neighbors(s, g, &opts2).unwrap();
    assert!(edges2.iter().any(|e| e.type_ == "door"));
}

fn setup_navmesh_db_lshape() -> Database {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE cells (
            id INTEGER PRIMARY KEY,
            plane INTEGER NOT NULL,
            kind TEXT NOT NULL,
            wkb BLOB NOT NULL,
            area REAL NOT NULL,
            minx REAL NOT NULL,
            miny REAL NOT NULL,
            maxx REAL NOT NULL,
            maxy REAL NOT NULL
        );
        CREATE VIRTUAL TABLE rtree_cells USING rtree(id, minx, maxx, miny, maxy);
        CREATE TABLE portals (
            id INTEGER PRIMARY KEY,
            plane INTEGER NOT NULL,
            a_id INTEGER NOT NULL,
            b_id INTEGER NOT NULL,
            x1 REAL NOT NULL,
            y1 REAL NOT NULL,
            x2 REAL NOT NULL,
            y2 REAL NOT NULL,
            length REAL NOT NULL
        );
        "#,
    ).unwrap();

    let ring1 = vec![[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,1.0]];
    let ring2 = vec![[1.0,0.0],[2.0,0.0],[2.0,1.0],[1.0,1.0]];
    let ring3 = vec![[1.0,1.0],[2.0,1.0],[2.0,2.0],[1.0,2.0]];
    let wkb1 = wkb_polygon_of_ring(&ring1);
    let wkb2 = wkb_polygon_of_ring(&ring2);
    let wkb3 = wkb_polygon_of_ring(&ring3);
    conn.execute("INSERT INTO cells (id,plane,kind,wkb,area,minx,miny,maxx,maxy) VALUES (1,0,'polygon',?1,1.0,0.0,0.0,1.0,1.0)", [wkb1]).unwrap();
    conn.execute("INSERT INTO cells (id,plane,kind,wkb,area,minx,miny,maxx,maxy) VALUES (2,0,'polygon',?1,1.0,1.0,0.0,2.0,1.0)", [wkb2]).unwrap();
    conn.execute("INSERT INTO cells (id,plane,kind,wkb,area,minx,miny,maxx,maxy) VALUES (3,0,'polygon',?1,1.0,1.0,1.0,2.0,2.0)", [wkb3]).unwrap();
    conn.execute("INSERT INTO rtree_cells (id,minx,maxx,miny,maxy) VALUES (1,0.0,1.0,0.0,1.0)", []).unwrap();
    conn.execute("INSERT INTO rtree_cells (id,minx,maxx,miny,maxy) VALUES (2,1.0,2.0,0.0,1.0)", []).unwrap();
    conn.execute("INSERT INTO rtree_cells (id,minx,maxx,miny,maxy) VALUES (3,1.0,2.0,1.0,2.0)", []).unwrap();

    conn.execute("INSERT INTO portals (id,plane,a_id,b_id,x1,y1,x2,y2,length) VALUES (10,0,1,2,1.0,0.25,1.0,0.75,0.5)", []).unwrap();
    conn.execute("INSERT INTO portals (id,plane,a_id,b_id,x1,y1,x2,y2,length) VALUES (11,0,2,3,1.25,1.0,1.75,1.0,0.5)", []).unwrap();

    Database::from_connection(conn)
}

#[test]
fn world_space_waypoints_via_funnel_from_astar_portals() {
    let db = setup_navmesh_db_lshape();
    let prov = NavmeshGraphProvider::new(db, CostModel::default());
    let start_xy = [0.5, 0.5];
    let goal_xy = [1.5, 1.5];
    let s = prov.map_point_to_tile(start_xy[0], start_xy[1], 0).unwrap().unwrap();
    let g = prov.map_point_to_tile(goal_xy[0], goal_xy[1], 0).unwrap().unwrap();

    let opts = SearchOptions::default();
    let cm = CostModel::default();
    let astar = AStar::new(&prov, &cm);
    let res = astar.find_path(s, g, &opts).unwrap();
    let path = res.path.expect("path");
    assert_eq!(path, vec![s, [2,0,0], g]);

    let mut portals: Vec<([f64;2],[f64;2])> = Vec::new();
    for a in res.actions {
        if a.type_ == "move" {
            if let Some(m) = a.metadata.as_ref() {
                let x1 = m.get("x1").and_then(|v| v.as_f64()).unwrap();
                let y1 = m.get("y1").and_then(|v| v.as_f64()).unwrap();
                let x2 = m.get("x2").and_then(|v| v.as_f64()).unwrap();
                let y2 = m.get("y2").and_then(|v| v.as_f64()).unwrap();
                portals.push(([x1,y1],[x2,y2]));
            }
        }
    }
    assert_eq!(portals.len(), 2);

    let wp = string_pull(start_xy, &portals, goal_xy, 1e-12);
    assert!(wp.len() >= 3);
    let corner = wp[1];
    assert!((corner[0] - 1.0).abs() < 1e-6);
    assert!(corner[1] >= 0.25 - 1e-6 && corner[1] <= 0.75 + 1e-6);
    let last = *wp.last().unwrap();
    assert!((last[0] - goal_xy[0]).abs() < 1e-12 && (last[1] - goal_xy[1]).abs() < 1e-12);
}
