use navpath_service::models::{RequirementKV, Tile};
use navpath_service::planner::graph::{build_graph, BuildOptions, EdgeKind, GraphInputs, NodeKind};
use navpath_service::requirements::RequirementEvaluator;
use navpath_service::db::{ClusterEntrance, ClusterIntraConnection, ClusterInterConnection, AbstractTeleportEdge, TeleportRequirement};

fn sample_entrance(id: i64, cluster_id: i64, x: i64, y: i64, plane: i64) -> ClusterEntrance {
    ClusterEntrance {
        entrance_id: id,
        cluster_id,
        x,
        y,
        plane,
        neighbor_dir: "N".to_string(),
        teleport_edge_id: None,
    }
}

#[test]
fn graph_builds_nodes_and_edges_without_teleport_when_gated() {
    let entrances = vec![
        sample_entrance(1, 10, 0, 0, 0),
        sample_entrance(2, 10, 1, 0, 0),
        sample_entrance(3, 20, 2, 0, 0),
    ];

    let intra = vec![ClusterIntraConnection { entrance_from: 1, entrance_to: 2, cost: 5, path_blob: Some(vec![1,2,3]) }];
    let inter = vec![ClusterInterConnection { entrance_from: 2, entrance_to: 3, cost: 7 }];

    // Teleport 1 -> 3 requires token=="yes"
    let teleports = vec![AbstractTeleportEdge {
        edge_id: 42,
        kind: "door".to_string(),
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

    let teleport_requirements = vec![TeleportRequirement{
        id: 100,
        meta_info: None,
        key: Some("token".to_string()),
        value: Some("yes".to_string()),
        comparison: Some("==".to_string()),
    }];

    let inputs = GraphInputs { 
        entrances: &entrances, 
        intra: &intra, 
        inter: &inter, 
        teleports: &teleports, 
        teleport_requirements: &teleport_requirements,
    };

    // Caller does NOT provide token, so teleport is gated out
    let eval = RequirementEvaluator::new(&[]);
    let opts = BuildOptions { start: Tile { x: -1, y: -1, plane: 0 }, end: Tile { x: 99, y: 99, plane: 0 } };
    let g = build_graph(&inputs, &eval, &opts);

    // 3 entrances + 2 virtual nodes
    assert_eq!(g.nodes.len(), 5);

    // Expect intra and inter edges only (2), teleport gated
    assert_eq!(g.edges.len(), 2);

    // Check that the Intra edge carries path_blob
    let has_intra_with_blob = g.edges.iter().any(|e| matches!(&e.kind, EdgeKind::Intra { path_blob: Some(b) } if !b.is_empty()));
    assert!(has_intra_with_blob);

    // Check virtual nodes types present
    let has_start = g.nodes.iter().any(|n| matches!(n.kind, NodeKind::VirtualStart(_)));
    let has_end = g.nodes.iter().any(|n| matches!(n.kind, NodeKind::VirtualEnd(_)));
    assert!(has_start && has_end);
}

#[test]
fn graph_includes_teleport_when_requirements_met() {
    let entrances = vec![
        sample_entrance(1, 10, 0, 0, 0),
        sample_entrance(2, 10, 1, 0, 0),
        sample_entrance(3, 20, 2, 0, 0),
    ];

    let intra = vec![ClusterIntraConnection { entrance_from: 1, entrance_to: 2, cost: 5, path_blob: None }];
    let inter = vec![ClusterInterConnection { entrance_from: 2, entrance_to: 3, cost: 7 }];

    let teleports = vec![AbstractTeleportEdge {
        edge_id: 42,
        kind: "door".to_string(),
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

    let teleport_requirements = vec![TeleportRequirement{
        id: 100,
        meta_info: None,
        key: Some("token".to_string()),
        value: Some("yes".to_string()),
        comparison: Some("==".to_string()),
    }];

    let inputs = GraphInputs { 
        entrances: &entrances, 
        intra: &intra, 
        inter: &inter, 
        teleports: &teleports, 
        teleport_requirements: &teleport_requirements,
    };

    // Caller provides token=yes, teleport allowed
    let caller = vec![RequirementKV { key: "token".into(), value: serde_json::json!("yes") }];
    let eval = RequirementEvaluator::new(&caller);
    let opts = BuildOptions { start: Tile { x: -1, y: -1, plane: 0 }, end: Tile { x: 99, y: 99, plane: 0 } };
    let g = build_graph(&inputs, &eval, &opts);

    // 3 entrances + 2 virtual nodes
    assert_eq!(g.nodes.len(), 5);

    // Expect intra, inter, and teleport edges (3)
    assert_eq!(g.edges.len(), 3);

    // Ensure a teleport edge exists
    let has_tp = g.edges.iter().any(|e| matches!(e.kind, EdgeKind::Teleport { .. }));
    assert!(has_tp);
}
