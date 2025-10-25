use std::collections::{HashMap, HashSet};

use navpath_service::db::{AbstractTeleportEdge, ClusterEntrance, ClusterIntraConnection, ClusterInterConnection, TeleportRequirement};
use navpath_service::models::{RequirementKV, Tile};
use navpath_service::planner::graph::GraphInputs;
use navpath_service::planner::hpa::{plan, HpaInputs, HpaOptions};
use navpath_service::requirements::RequirementEvaluator;

fn ent(id: i64, cid: i64, x: i64, y: i64, plane: i64) -> ClusterEntrance {
    ClusterEntrance { entrance_id: id, cluster_id: cid, x, y, plane, neighbor_dir: "N".into(), teleport_edge_id: None }
}

fn tiles_set(coords: &[(i32, i32, i32)]) -> HashSet<(i32, i32, i32)> {
    coords.iter().copied().collect()
}

#[test]
fn hpa_without_teleport_gates_edge() {
    let entrances = vec![ent(1, 10, 0, 0, 0), ent(2, 10, 2, 0, 0), ent(3, 20, 4, 0, 0)];
    let intra = vec![ClusterIntraConnection { entrance_from: 1, entrance_to: 2, cost: 2, path_blob: None }];
    let inter = vec![ClusterInterConnection { entrance_from: 2, entrance_to: 3, cost: 2 }];
    let teleports = vec![AbstractTeleportEdge {
        edge_id: 99,
        kind: "teleport".into(),
        node_id: 0,
        src_x: None,
        src_y: None,
        src_plane: None,
        dst_x: 0,
        dst_y: 0,
        dst_plane: 0,
        cost: 1,
        requirement_id: Some(100),
        src_entrance: Some(1),
        dst_entrance: Some(3),
    }];
    let tp_reqs = vec![TeleportRequirement { id: 100, meta_info: None, key: Some("token".into()), value: Some("yes".into()), comparison: Some("==".into()) }];

    let inputs = GraphInputs { entrances: &entrances, intra: &intra, inter: &inter, teleports: &teleports, teleport_requirements: &tp_reqs };

    // cluster tiles: cluster 10 tiles x= -1..=2, y=0; cluster 20 tiles x=4..=5, y=0
    let mut cluster_tiles: HashMap<i64, HashSet<(i32, i32, i32)>> = HashMap::new();
    cluster_tiles.insert(10, tiles_set(&[(-1,0,0),(0,0,0),(1,0,0),(2,0,0)]));
    cluster_tiles.insert(20, tiles_set(&[(4,0,0),(5,0,0)]));

    let is_walkable = Box::new(|_x: i32, _y: i32, _p: i32| true);

    let hpa_inputs = HpaInputs { graph_inputs: inputs, cluster_tiles, is_walkable };
    let eval = RequirementEvaluator::new(&[]); // no token, teleport gated
    let opts = HpaOptions { start: Tile { x: -1, y: 0, plane: 0 }, end: Tile { x: 5, y: 0, plane: 0 } };

    let res = plan(&hpa_inputs, &eval, &opts).expect("plan");

    // Path must start and end correctly
    assert_eq!(res.path.first().copied(), Some(opts.start));
    assert_eq!(res.path.last().copied(), Some(opts.end));
    // No teleport actions due to gating
    assert!(res.actions.iter().all(|a| a.get("type") != Some(&serde_json::json!("teleport"))));
}

#[test]
fn hpa_with_teleport_included() {
    let entrances = vec![ent(1, 10, 0, 0, 0), ent(2, 10, 2, 0, 0), ent(3, 20, 4, 0, 0)];
    let intra = vec![ClusterIntraConnection { entrance_from: 1, entrance_to: 2, cost: 2, path_blob: None }];
    let inter = vec![ClusterInterConnection { entrance_from: 2, entrance_to: 3, cost: 2 }];
    let teleports = vec![AbstractTeleportEdge {
        edge_id: 99,
        kind: "teleport".into(),
        node_id: 0,
        src_x: None,
        src_y: None,
        src_plane: None,
        dst_x: 0,
        dst_y: 0,
        dst_plane: 0,
        cost: 1,
        requirement_id: Some(100),
        src_entrance: Some(1),
        dst_entrance: Some(3),
    }];
    let tp_reqs = vec![TeleportRequirement { id: 100, meta_info: None, key: Some("token".into()), value: Some("yes".into()), comparison: Some("==".into()) }];

    let inputs = GraphInputs { entrances: &entrances, intra: &intra, inter: &inter, teleports: &teleports, teleport_requirements: &tp_reqs };

    let mut cluster_tiles: HashMap<i64, HashSet<(i32, i32, i32)>> = HashMap::new();
    cluster_tiles.insert(10, tiles_set(&[(-1,0,0),(0,0,0),(1,0,0),(2,0,0)]));
    cluster_tiles.insert(20, tiles_set(&[(4,0,0),(5,0,0)]));

    let is_walkable = Box::new(|_x: i32, _y: i32, _p: i32| true);
    let hpa_inputs = HpaInputs { graph_inputs: inputs, cluster_tiles, is_walkable };

    let caller = vec![RequirementKV { key: "token".into(), value: serde_json::json!("yes") }];
    let eval = RequirementEvaluator::new(&caller);
    let opts = HpaOptions { start: Tile { x: -1, y: 0, plane: 0 }, end: Tile { x: 5, y: 0, plane: 0 } };

    let res = plan(&hpa_inputs, &eval, &opts).expect("plan");
    // Ensure teleport action exists
    assert!(res.actions.iter().any(|a| a.get("type") == Some(&serde_json::json!("teleport"))));
    // Start and end present
    assert_eq!(res.path.first().copied(), Some(opts.start));
    assert_eq!(res.path.last().copied(), Some(opts.end));
}
