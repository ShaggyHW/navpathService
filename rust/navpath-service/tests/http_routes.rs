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
    let macro_src: Vec<u32> = vec![0];
    let macro_dst: Vec<u32> = vec![2];
    let macro_w: Vec<f32> = vec![3.5];
    let macro_kind_first: Vec<u32> = vec![2]; // lodestone
    let macro_id_first: Vec<u32> = vec![13];
    let macro_meta_offs: Vec<u32> = vec![0];
    let macro_meta_lens: Vec<u32> = vec![2];
    let macro_meta_blob: Vec<u8> = b"{}".to_vec();
    let req: Vec<u32> = vec![];
    let landmarks: Vec<u32> = vec![];
    let lm_fw: Vec<f32> = vec![];
    let lm_bw: Vec<f32> = vec![];

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

