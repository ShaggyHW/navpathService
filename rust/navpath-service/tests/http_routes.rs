use std::collections::HashMap;
use std::sync::Arc;

use axum::{body::Body, http::{Request, StatusCode}};
use http_body_util::BodyExt;
use navpath_core::snapshot::write_snapshot;
use navpath_service::{build_router, AppState, SnapshotState, build_coord_index};
use arc_swap::ArcSwap;
use serde_json::json;
use tempfile::NamedTempFile;
use tower::ServiceExt; // for `oneshot`

fn make_snapshot_file(nodes: usize) -> tempfile::TempPath {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();
    let nodes_ids: Vec<u32> = (0..nodes as u32).collect();
    let nodes_x: Vec<i32> = (0..nodes as i32).map(|i| 3200 + i).collect();
    let nodes_y: Vec<i32> = vec![3200; nodes];
    let nodes_plane: Vec<i32> = vec![0; nodes];
    // simple chain 0->1->2
    let walk_src = vec![0u32, 1u32];
    let walk_dst = vec![1u32, 2u32];
    let walk_w = vec![1.0f32, 1.0f32];

    // Macro edges: include a synthetic 0->0 edge whose metadata encodes global teleports.
    // Global teleports are used when the requested start coordinate is not present in the snapshot.
    let macro_src: Vec<u32> = vec![0, 0];
    let macro_dst: Vec<u32> = vec![2, 0];
    let macro_w: Vec<f32> = vec![3.5, 0.0];
    let macro_kind_first: Vec<u32> = vec![2, 0]; // lodestone, then synthetic
    let macro_id_first: Vec<u32> = vec![13, 0];

    let meta0: Vec<u8> = b"{}".to_vec();
    // Keep this cost very high so it doesn't affect the normal 0->1->2 route.
    let meta1: Vec<u8> = serde_json::json!({
        "global": [{
            "dst": 1,
            "cost_ms": 10000.0,
            "steps": [{"kind": "npc"}],
            "requirements": []
        }]
    }).to_string().into_bytes();

    let macro_meta_offs: Vec<u32> = vec![0, meta0.len() as u32];
    let macro_meta_lens: Vec<u32> = vec![meta0.len() as u32, meta1.len() as u32];
    let mut macro_meta_blob: Vec<u8> = Vec::with_capacity(meta0.len() + meta1.len());
    macro_meta_blob.extend_from_slice(&meta0);
    macro_meta_blob.extend_from_slice(&meta1);
    let req: Vec<u32> = vec![];
    let landmarks: Vec<u32> = vec![];
    let lm_fw: Vec<f32> = vec![];
    let lm_bw: Vec<f32> = vec![];
    // Empty fairy ring data for basic tests
    let fairy_nodes: Vec<u32> = vec![];
    let fairy_cost_ms: Vec<f32> = vec![];
    let fairy_meta_offs: Vec<u32> = vec![];
    let fairy_meta_lens: Vec<u32> = vec![];
    let fairy_meta_blob: Vec<u8> = vec![];

    write_snapshot(
        &path,
        &nodes_ids,
        &nodes_x,
        &nodes_y,
        &nodes_plane,
        &walk_src,
        &walk_dst,
        &walk_w,
        &macro_src,
        &macro_dst,
        &macro_w,
        &macro_kind_first,
        &macro_id_first,
        &macro_meta_offs,
        &macro_meta_lens,
        &macro_meta_blob,
        &req,
        &landmarks,
        &lm_fw,
        &lm_bw,
        &fairy_nodes,
        &fairy_cost_ms,
        &fairy_meta_offs,
        &fairy_meta_lens,
        &fairy_meta_blob,
    ).expect("write snapshot");

    tmp.into_temp_path()
}

#[tokio::test]
async fn health_and_route_and_reload() {
    // Build initial snapshot
    let snap_path = make_snapshot_file(3);
    let opened = navpath_core::Snapshot::open(&snap_path).unwrap();
    let (neighbors, globals, macro_lookup) = navpath_service::engine_adapter::build_neighbor_provider(&opened);
    let coord_index = Some(Arc::new(build_coord_index(&opened)));
    let snapshot = Some(Arc::new(opened));
    let state = AppState { current: Arc::new(ArcSwap::from_pointee(SnapshotState {
        path: snap_path.to_path_buf(),
        snapshot,
        neighbors: Some(Arc::new(neighbors)),
        globals: Arc::new(globals),
        macro_lookup: Arc::new(macro_lookup),
        loaded_at_unix: 123,
        snapshot_hash_hex: None,
        coord_index,
        fairy_rings: Arc::new(Vec::new()),
        node_to_fairy_ring: Arc::new(HashMap::new()),
    })) };

    let app = build_router(state.clone());

    // GET /health
    let res = app.clone().oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v.get("version").is_some());

    // POST /route
    let req_body = json!({
        "start_id": 0,
        "goal_id": 2,
        "profile": {"requirements": []},
        "options": {"return_geometry": true, "only_actions": false}
    }).to_string();
    let res = app.clone().oneshot(Request::builder()
        .method("POST")
        .uri("/route")
        .header("content-type", "application/json")
        .body(Body::from(req_body)).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["found"], true);
    assert_eq!(v["path"], json!([0,1,2]));

    // POST /admin/reload (re-write file with same contents is fine)
    // For behavior, we just assert 200 and reloaded true
    let res = app.clone().oneshot(Request::builder()
        .method("POST")
        .uri("/admin/reload")
        .body(Body::empty()).unwrap()).await.unwrap();
    assert!(res.status().is_success());
}

#[tokio::test]
async fn missing_start_coordinate_forces_global_teleport_entry() {
    let snap_path = make_snapshot_file(3);
    let opened = navpath_core::Snapshot::open(&snap_path).unwrap();
    let (neighbors, globals, macro_lookup) = navpath_service::engine_adapter::build_neighbor_provider(&opened);
    let coord_index = Some(Arc::new(build_coord_index(&opened)));
    let snapshot = Some(Arc::new(opened));
    let state = AppState { current: Arc::new(ArcSwap::from_pointee(SnapshotState {
        path: snap_path.to_path_buf(),
        snapshot,
        neighbors: Some(Arc::new(neighbors)),
        globals: Arc::new(globals),
        macro_lookup: Arc::new(macro_lookup),
        loaded_at_unix: 123,
        snapshot_hash_hex: None,
        coord_index,
        fairy_rings: Arc::new(Vec::new()),
        node_to_fairy_ring: Arc::new(HashMap::new()),
    })) };

    let app = build_router(state.clone());

    // POST /route with start coordinate not present in snapshot
    let req_body = json!({
        "start": {"wx": 2212, "wy": 4944, "plane": 1},
        "goal":  {"wx": 3202, "wy": 3200, "plane": 0},
        "profile": {"requirements": []},
        "options": {"return_geometry": false, "only_actions": true}
    }).to_string();
    let res = app.clone().oneshot(Request::builder()
        .method("POST")
        .uri("/route")
        .header("content-type", "application/json")
        .body(Body::from(req_body)).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["found"], true);

    let actions = v.get("actions").and_then(|a| a.as_array()).unwrap();
    assert!(!actions.is_empty());
    let first = &actions[0];
    assert!(first.get("type").and_then(|t| t.as_str()).is_some());
    assert_eq!(
        first.get("metadata").and_then(|m| m.get("reason")).and_then(|r| r.as_str()),
        Some("start_coordinate_not_found")
    );
}

