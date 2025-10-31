use navpath_service::db::Db;
use navpath_service::models::Tile;
use navpath_service::planner::cluster::plan_same_cluster;
use navpath_service::planner::high_level::plan_hops;
use navpath_service::planner::graph::GraphInputs;
use navpath_service::requirements::RequirementEvaluator;
use navpath_service::db::{
    ClusterEntrance,
    ClusterIntraConnection,
    ClusterInterConnection,
    AbstractTeleportEdge,
    TeleportRequirement,
};
use rusqlite::Connection;
use tempfile::tempdir;
use std::collections::{HashMap, HashSet};

#[test]
fn same_cluster_micro_astar_returns_path() {
    // Build temp RW DB with minimal schema
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("cluster.db");
    let conn = Connection::open(&path).expect("open rw sqlite");
    conn.execute(
        "CREATE TABLE tiles (
            x INTEGER, y INTEGER, plane INTEGER,
            flag INTEGER, blocked INTEGER, walk_mask INTEGER, blocked_mask INTEGER, walk_data TEXT
        )",
        [],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE cluster_tiles (cluster_id INTEGER, x INTEGER, y INTEGER, plane INTEGER)",
        [],
    )
    .unwrap();

    // Insert a simple corridor and mark the tiles as belonging to cluster 1
    for x in 0..=2 {
        conn.execute(
            "INSERT INTO tiles (x,y,plane,flag,blocked,walk_mask,blocked_mask,walk_data) VALUES (?1,0,0,0,0,0,0,NULL)",
            rusqlite::params![x],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cluster_tiles (cluster_id,x,y,plane) VALUES (1, ?1, 0, 0)",
            rusqlite::params![x],
        )
        .unwrap();
    }
    drop(conn);

    // Re-open read-only via Db
    let db = Db::open_read_only(&path).expect("open read-only");

    let start = Tile { x: 0, y: 0, plane: 0 };
    let end = Tile { x: 2, y: 0, plane: 0 };
    let is_walkable = |x: i32, y: i32, p: i32| {
        db.get_tile(x, y, p)
            .ok()
            .flatten()
            .map(|t| t.blocked == 0)
            .unwrap_or(false)
    };

    let res = plan_same_cluster(&db, start, end, is_walkable).expect("planner ok");
    let path = res.expect("same cluster should plan micro path");
    assert_eq!(path.len(), 3);
    assert_eq!(path[0], start);
    assert_eq!(path[2], end);
}

#[test]
fn cross_cluster_hops_via_teleport_with_requirements() {
    // Build a synthetic abstract graph in-memory (no DB reads needed here)
    // Entrances: two in cluster 100 (plane 0), one in cluster 200 (plane 0)
    let e1 = ClusterEntrance { entrance_id: 1, cluster_id: 100, x: 0, y: 0, plane: 0, neighbor_dir: String::new(), teleport_edge_id: None };
    let e2 = ClusterEntrance { entrance_id: 2, cluster_id: 100, x: 10, y: 0, plane: 0, neighbor_dir: String::new(), teleport_edge_id: None };
    let e3 = ClusterEntrance { entrance_id: 3, cluster_id: 200, x: 20, y: 0, plane: 0, neighbor_dir: String::new(), teleport_edge_id: None };

    // Teleport from e2 -> e3 with requirement id 42 and cost 5
    let tp = AbstractTeleportEdge {
        edge_id: 7,
        kind: "item".to_string(),
        node_id: 123,
        src_x: None,
        src_y: None,
        src_plane: None,
        dst_x: e3.x,
        dst_y: e3.y,
        dst_plane: e3.plane,
        cost: 5,
        requirement_id: Some(42),
        src_entrance: Some(e2.entrance_id),
        dst_entrance: Some(e3.entrance_id),
    };

    // Requirement: caller must have level >= 10. We'll provide 50.
    let req = TeleportRequirement { id: 42, meta_info: None, key: Some("level".into()), value: Some("10".into()), comparison: Some(">=".into()) };
    let caller_requirements = vec![navpath_service::models::RequirementKV { key: "level".into(), value: serde_json::json!(50) }];
    let evaluator = RequirementEvaluator::new(&caller_requirements);

    let inputs = GraphInputs {
        entrances: &[e1.clone(), e2.clone(), e3.clone()],
        intra: &[] as &[ClusterIntraConnection],
        inter: &[] as &[ClusterInterConnection],
        teleports: &[tp.clone()],
        teleport_requirements: &[req.clone()],
    };

    // Cluster tiles for micro edges (start->entrance and entrance->end)
    // Cluster 100: tiles 0..=10 on y=0. Cluster 200: tiles 20..=21 on y=0
    let mut cluster_tiles: HashMap<i64, HashSet<(i32, i32, i32)>> = HashMap::new();
    let mut set100 = HashSet::new();
    for x in 0..=10 { set100.insert((x, 0, 0)); }
    cluster_tiles.insert(100, set100);
    let mut set200 = HashSet::new();
    for x in 20..=21 { set200.insert((x, 0, 0)); }
    cluster_tiles.insert(200, set200);

    let is_walkable: Box<dyn Fn(i32, i32, i32) -> bool> = Box::new(|_x: i32, _y: i32, _p: i32| true);

    let start = Tile { x: 9, y: 0, plane: 0 }; // closer to e2
    let end = Tile { x: 21, y: 0, plane: 0 };

    let plan = plan_hops(
        &inputs,
        &evaluator,
        start,
        end,
        &cluster_tiles,
        &is_walkable,
    ).expect("should find cross-cluster via teleport");

    // Expect entrance sequence [2, 3]
    assert_eq!(plan.entrances, vec![2, 3]);
}

#[test]
fn no_route_returns_none() {
    // Entrances present but no edges and micro edges disallowed due to missing cluster tiles
    let e1 = ClusterEntrance { entrance_id: 1, cluster_id: 100, x: 0, y: 0, plane: 0, neighbor_dir: String::new(), teleport_edge_id: None };
    let e2 = ClusterEntrance { entrance_id: 2, cluster_id: 200, x: 20, y: 0, plane: 0, neighbor_dir: String::new(), teleport_edge_id: None };

    let inputs = GraphInputs { entrances: &[e1, e2], intra: &[] as &[ClusterIntraConnection], inter: &[] as &[ClusterInterConnection], teleports: &[], teleport_requirements: &[] };
    let evaluator = RequirementEvaluator::new(&[]);

    // Cluster tiles do not include the start tile or end tile; micro edges won't exist
    let mut cluster_tiles: HashMap<i64, HashSet<(i32, i32, i32)>> = HashMap::new();
    let mut set100 = HashSet::new();
    set100.insert((5, 0, 0));
    cluster_tiles.insert(100, set100);
    let mut set200 = HashSet::new();
    set200.insert((25, 0, 0));
    cluster_tiles.insert(200, set200);
    let is_walkable: Box<dyn Fn(i32, i32, i32) -> bool> = Box::new(|_x: i32, _y: i32, _p: i32| true);

    let start = Tile { x: 9, y: 0, plane: 0 };
    let end = Tile { x: 21, y: 0, plane: 0 };

    let plan = plan_hops(&inputs, &evaluator, start, end, &cluster_tiles, &is_walkable);
    assert!(plan.is_none());
}
