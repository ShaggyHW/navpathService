use std::collections::HashMap;
use std::sync::Arc;

use axum::{body::Body, http::{Request, StatusCode}};
use http_body_util::BodyExt;
use navpath_core::snapshot::{pack_coord, write_snapshot_v8, SnapshotSections};
use navpath_service::{build_router, AppState, SnapshotState};
use arc_swap::ArcSwap;
use serde_json::json;
use tempfile::NamedTempFile;
use tower::ServiceExt; // for `oneshot`

fn make_snapshot_file(nodes: usize) -> tempfile::TempPath {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();
    assert_eq!(nodes, 3, "fixture is a 3-node chain");
    // Nodes at (3200..3202, 3200, plane 0); packed keys ascend with x.
    let coords_packed: Vec<u32> = (0..nodes as i32).map(|i| pack_coord(3200 + i, 3200, 0)).collect();
    // simple symmetric chain 0<->1<->2 (cardinal edges) — the real walk graph is
    // symmetric and the builder asserts it; bidirectional search relies on it.
    let walk_offsets = vec![0u32, 1, 3, 4];
    let walk_dst = vec![1u32, 0, 2, 1];
    let walk_diag = vec![0u8];
    let comp = vec![0u16, 0, 0];

    // Macro edges: include a synthetic 0->0 edge whose metadata encodes global teleports.
    // Global teleports are used when the requested start coordinate is not present in the snapshot.
    let macro_src: Vec<u32> = vec![0, 0];
    let macro_dst: Vec<u32> = vec![2, 0];
    // v8 derives walk weights (300ms/step), so walking 0->1->2 costs 600; keep the
    // direct macro edge more expensive so the walk route stays optimal.
    let macro_w: Vec<f32> = vec![700.0, 0.0];
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

    write_snapshot_v8(
        &path,
        &SnapshotSections {
            coords_packed: &coords_packed,
            walk_offsets: &walk_offsets,
            walk_dst: &walk_dst,
            walk_diag: &walk_diag,
            comp: &comp,
            walk_components: 1,
            macro_src: &macro_src,
            macro_dst: &macro_dst,
            macro_w: &macro_w,
            macro_kind_first: &macro_kind_first,
            macro_id_first: &macro_id_first,
            macro_meta_offs: &macro_meta_offs,
            macro_meta_lens: &macro_meta_lens,
            macro_meta_blob: &macro_meta_blob,
            req_tags: &[],
            landmarks: &[],
            lm_tab: &[],
            fairy_nodes: &[],
            fairy_cost_ms: &[],
            fairy_meta_offs: &[],
            fairy_meta_lens: &[],
            fairy_meta_blob: &[],
        },
    ).expect("write snapshot");

    tmp.into_temp_path()
}

/// Fixture for /reachable: two 27-tile columns at x=3200/3202 (y 3200..=3226)
/// joined ONLY across the top at (3201,3226) — so their feet are 2 tiles apart but
/// the sole walk path detours 26 tiles north; a walk-isolated cell at (3205,3200)
/// connected only through a door macro edge; and a distant cell at (3300,3200).
fn make_reachable_snapshot_file() -> tempfile::TempPath {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_path_buf();

    let mut tiles: Vec<(i32, i32)> = Vec::new();
    for y in 3200..=3226 {
        tiles.push((3200, y));
        tiles.push((3202, y));
    }
    tiles.push((3201, 3226));
    tiles.push((3205, 3200));
    tiles.push((3300, 3200));
    // Snapshot node ids ascend with the packed key (plane, y, x).
    tiles.sort_by_key(|&(x, y)| pack_coord(x, y, 0));
    let index: HashMap<(i32, i32), u32> =
        tiles.iter().enumerate().map(|(i, &t)| (t, i as u32)).collect();
    let coords_packed: Vec<u32> = tiles.iter().map(|&(x, y)| pack_coord(x, y, 0)).collect();

    // Walk edges: cardinal adjacency between existing tiles (symmetric).
    let mut walk_offsets: Vec<u32> = vec![0];
    let mut walk_dst: Vec<u32> = Vec::new();
    for &(x, y) in &tiles {
        for n in [(x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)] {
            if let Some(&v) = index.get(&n) {
                walk_dst.push(v);
            }
        }
        walk_offsets.push(walk_dst.len() as u32);
    }
    let walk_diag = vec![0u8; walk_dst.len().div_ceil(8)];

    // Walk-component ids from the edges above (what the endpoint's stage-1 check reads).
    fn find(parent: &mut [u32], mut v: u32) -> u32 {
        while parent[v as usize] != v {
            parent[v as usize] = parent[parent[v as usize] as usize];
            v = parent[v as usize];
        }
        v
    }
    let mut parent: Vec<u32> = (0..tiles.len() as u32).collect();
    for u in 0..tiles.len() {
        for slot in walk_offsets[u] as usize..walk_offsets[u + 1] as usize {
            let (ru, rv) = (find(&mut parent, u as u32), find(&mut parent, walk_dst[slot]));
            if ru != rv {
                parent[ru as usize] = rv;
            }
        }
    }
    let mut comp: Vec<u16> = vec![0; tiles.len()];
    let mut roots: HashMap<u32, u16> = HashMap::new();
    for v in 0..tiles.len() as u32 {
        let r = find(&mut parent, v);
        let next = roots.len() as u16;
        comp[v as usize] = *roots.entry(r).or_insert(next);
    }

    // The isolated cell hangs off the east column through a door.
    let macro_src = vec![index[&(3202, 3200)]];
    let macro_dst = vec![index[&(3205, 3200)]];
    let meta: Vec<u8> = b"{}".to_vec();

    write_snapshot_v8(
        &path,
        &SnapshotSections {
            coords_packed: &coords_packed,
            walk_offsets: &walk_offsets,
            walk_dst: &walk_dst,
            walk_diag: &walk_diag,
            comp: &comp,
            walk_components: roots.len() as u32,
            macro_src: &macro_src,
            macro_dst: &macro_dst,
            macro_w: &[600.0],
            macro_kind_first: &[1], // door
            macro_id_first: &[7],
            macro_meta_offs: &[0],
            macro_meta_lens: &[meta.len() as u32],
            macro_meta_blob: &meta,
            req_tags: &[],
            landmarks: &[],
            lm_tab: &[],
            fairy_nodes: &[],
            fairy_cost_ms: &[],
            fairy_meta_offs: &[],
            fairy_meta_lens: &[],
            fairy_meta_blob: &[],
        },
    )
    .expect("write snapshot");

    tmp.into_temp_path()
}

#[tokio::test]
async fn reachable_is_walk_only_and_range_bounded() {
    let snap_path = make_reachable_snapshot_file();
    let opened = navpath_core::Snapshot::open(&snap_path).unwrap();
    let (neighbors, neighbors_rev, globals, macro_lookup) =
        navpath_service::engine_adapter::build_neighbor_provider(&opened);
    let snapshot = Some(Arc::new(opened));
    let req_tag_index = Arc::new(navpath_service::build_req_tag_index(snapshot.as_deref()));
    let state = AppState { current: Arc::new(ArcSwap::from_pointee(SnapshotState {
        path: snap_path.to_path_buf(),
        snapshot,
        neighbors: Some(Arc::new(neighbors)),
        neighbors_rev: Some(Arc::new(neighbors_rev)),
        globals: Arc::new(globals),
        macro_lookup: Arc::new(macro_lookup),
        req_tag_index,
        loaded_at_unix: 123,
        snapshot_hash_hex: None,
        route_cache: navpath_service::new_route_cache(),
        seed_shadow: navpath_service::new_seed_shadow(),
        fairy_rings: Arc::new(Vec::new()),
        node_to_fairy_ring: Arc::new(navpath_service::FxHashMap::default()),
        comp_graph: None,
        canonical_grid: None,
        profile_cache: navpath_service::new_profile_cache(), subpath_cache: navpath_service::new_subpath_cache(),
    })), search_permits: navpath_service::default_search_permits(), metrics: Arc::new(navpath_service::Metrics::default()), ctx_pool: navpath_service::ContextPool::new(), ready: Arc::new(std::sync::atomic::AtomicBool::new(true)) };

    let app = build_router(state);

    let check = |s: (i32, i32, i32), g: (i32, i32, i32)| {
        let app = app.clone();
        async move {
            let uri = format!(
                "/reachable?sx={}&sy={}&splane={}&gx={}&gy={}&gplane={}",
                s.0, s.1, s.2, g.0, g.1, g.2
            );
            let res = app
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        }
    };

    // Adjacent walk-connected tiles, and a 15-tile straight walk: reachable.
    let v = check((3200, 3200, 0), (3200, 3201, 0)).await;
    assert_eq!(v, json!({"reachable": true}));
    let v = check((3200, 3200, 0), (3200, 3215, 0)).await;
    assert_eq!(v, json!({"reachable": true}));

    // Same tile: trivially reachable.
    let v = check((3200, 3200, 0), (3200, 3200, 0)).await;
    assert_eq!(v, json!({"reachable": true}));

    // 5 tiles away but only connected through a door: walk-only says no.
    let v = check((3202, 3200, 0), (3205, 3200, 0)).await;
    assert_eq!(v, json!({"reachable": false, "reason": "not_connected"}));

    // 100 tiles away: outside the 20-tile range, rejected before any lookup.
    let v = check((3200, 3200, 0), (3300, 3200, 0)).await;
    assert_eq!(v, json!({"reachable": false, "reason": "out_of_range"}));

    // Cross-plane pairs can never be walk-reachable.
    let v = check((3200, 3200, 0), (3200, 3201, 1)).await;
    assert_eq!(v, json!({"reachable": false, "reason": "different_plane"}));

    // In-range coordinates that are not walkable tiles.
    let v = check((3199, 3200, 0), (3200, 3200, 0)).await;
    assert_eq!(v, json!({"reachable": false, "reason": "start_tile_not_found"}));
    let v = check((3200, 3200, 0), (3210, 3210, 0)).await;
    assert_eq!(v, json!({"reachable": false, "reason": "goal_tile_not_found"}));

    // Column feet: 2 tiles apart, same walk component, but the only walk path
    // detours 26 tiles north — outside the endpoints' 20-tile neighbourhood.
    let v = check((3200, 3200, 0), (3202, 3200, 0)).await;
    assert_eq!(v, json!({"reachable": false, "reason": "no_path_in_range"}));
}

#[tokio::test]
async fn health_and_route_and_reload() {
    // Build initial snapshot
    let snap_path = make_snapshot_file(3);
    let opened = navpath_core::Snapshot::open(&snap_path).unwrap();
    let (neighbors, neighbors_rev, globals, macro_lookup) = navpath_service::engine_adapter::build_neighbor_provider(&opened);
    let snapshot = Some(Arc::new(opened));
    let req_tag_index = Arc::new(navpath_service::build_req_tag_index(snapshot.as_deref()));
    let state = AppState { current: Arc::new(ArcSwap::from_pointee(SnapshotState {
        path: snap_path.to_path_buf(),
        snapshot,
        neighbors: Some(Arc::new(neighbors)),
        neighbors_rev: Some(Arc::new(neighbors_rev)),
        globals: Arc::new(globals),
        macro_lookup: Arc::new(macro_lookup),
        req_tag_index,
        loaded_at_unix: 123,
        snapshot_hash_hex: None,
        route_cache: navpath_service::new_route_cache(),
        seed_shadow: navpath_service::new_seed_shadow(),
        fairy_rings: Arc::new(Vec::new()),
        node_to_fairy_ring: Arc::new(navpath_service::FxHashMap::default()),
        comp_graph: None,
        canonical_grid: None,
        profile_cache: navpath_service::new_profile_cache(), subpath_cache: navpath_service::new_subpath_cache(),
    })), search_permits: navpath_service::default_search_permits(), metrics: Arc::new(navpath_service::Metrics::default()), ctx_pool: navpath_service::ContextPool::new(), ready: Arc::new(std::sync::atomic::AtomicBool::new(true)) };

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
    let (neighbors, neighbors_rev, globals, macro_lookup) = navpath_service::engine_adapter::build_neighbor_provider(&opened);
    let snapshot = Some(Arc::new(opened));
    let req_tag_index = Arc::new(navpath_service::build_req_tag_index(snapshot.as_deref()));
    let state = AppState { current: Arc::new(ArcSwap::from_pointee(SnapshotState {
        path: snap_path.to_path_buf(),
        snapshot,
        neighbors: Some(Arc::new(neighbors)),
        neighbors_rev: Some(Arc::new(neighbors_rev)),
        globals: Arc::new(globals),
        macro_lookup: Arc::new(macro_lookup),
        req_tag_index,
        loaded_at_unix: 123,
        snapshot_hash_hex: None,
        route_cache: navpath_service::new_route_cache(),
        seed_shadow: navpath_service::new_seed_shadow(),
        fairy_rings: Arc::new(Vec::new()),
        node_to_fairy_ring: Arc::new(navpath_service::FxHashMap::default()),
        comp_graph: None,
        canonical_grid: None,
        profile_cache: navpath_service::new_profile_cache(), subpath_cache: navpath_service::new_subpath_cache(),
    })), search_permits: navpath_service::default_search_permits(), metrics: Arc::new(navpath_service::Metrics::default()), ctx_pool: navpath_service::ContextPool::new(), ready: Arc::new(std::sync::atomic::AtomicBool::new(true)) };

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

