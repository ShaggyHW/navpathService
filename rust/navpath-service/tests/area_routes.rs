//! `POST /route_area`: cheapest route to any tile of an area.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::{body::Body, http::{Request, StatusCode}};
use http_body_util::BodyExt;
use navpath_core::snapshot::{pack_coord, write_snapshot_v8, SnapshotSections};
use navpath_service::{build_router, AppState, SnapshotState};
use serde_json::{json, Value};
use tempfile::NamedTempFile;
use tower::ServiceExt;

/// Two 27-tile columns at x=3200/3202 (y 3200..=3226) joined ONLY across the top at
/// (3201,3226); a walk-isolated cell at (3205,3200) entered through a door from
/// (3202,3200); a distant cell at (3300,3200); and a global teleport (cost 10000) landing
/// at (3200,3210). Cardinal walk edges only (300 ms per step).
fn make_snapshot() -> tempfile::TempPath {
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
    tiles.sort_by_key(|&(x, y)| pack_coord(x, y, 0));
    let index: HashMap<(i32, i32), u32> = tiles.iter().enumerate().map(|(i, &t)| (t, i as u32)).collect();
    let coords_packed: Vec<u32> = tiles.iter().map(|&(x, y)| pack_coord(x, y, 0)).collect();
    let mut walk_offsets: Vec<u32> = vec![0];
    let mut walk_dst: Vec<u32> = Vec::new();
    for &(x, y) in &tiles {
        // Ascending direction-bit order: left, bottom, right, top.
        for n in [(x - 1, y), (x, y - 1), (x + 1, y), (x, y + 1)] {
            if let Some(&v) = index.get(&n) {
                walk_dst.push(v);
            }
        }
        walk_offsets.push(walk_dst.len() as u32);
    }
    let walk_diag = vec![0u8; walk_dst.len().div_ceil(8)];
    // Components: west+east columns+connector = 0, isolated cell = 1, far cell = 2.
    let comp: Vec<u16> = tiles
        .iter()
        .map(|&t| if t == (3205, 3200) { 1 } else if t == (3300, 3200) { 2 } else { 0 })
        .collect();

    // Door (3202,3200) -> (3205,3200), plus the synthetic 0->0 carrier of the global.
    let macro_src = vec![index[&(3202, 3200)], 0];
    let macro_dst = vec![index[&(3205, 3200)], 0];
    let meta0: Vec<u8> = b"{}".to_vec();
    let meta1: Vec<u8> = json!({
        "global": [{ "dst": index[&(3200, 3210)], "cost_ms": 10000.0, "steps": [{"kind": "npc"}], "requirements": [] }]
    })
    .to_string()
    .into_bytes();
    let mut blob = meta0.clone();
    blob.extend_from_slice(&meta1);
    write_snapshot_v8(
        &path,
        &SnapshotSections {
            coords_packed: &coords_packed,
            walk_offsets: &walk_offsets,
            walk_dst: &walk_dst,
            walk_diag: &walk_diag,
            comp: &comp,
            walk_components: 3,
            macro_src: &macro_src,
            macro_dst: &macro_dst,
            macro_w: &[600.0, 0.0],
            macro_kind_first: &[1, 0],
            macro_id_first: &[7, 0],
            macro_meta_offs: &[0, meta0.len() as u32],
            macro_meta_lens: &[meta0.len() as u32, meta1.len() as u32],
            macro_meta_blob: &blob,
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

fn app_for(path: &std::path::Path, with_comp_graph: bool) -> axum::Router {
    let opened = navpath_core::Snapshot::open(path).unwrap();
    let (neighbors, neighbors_rev, globals, macro_lookup) = navpath_service::engine_adapter::build_neighbor_provider(&opened);
    let comp_graph = with_comp_graph.then(|| {
        Arc::new(navpath_service::engine_adapter::build_component_graph(&opened, &globals, &[], &macro_lookup))
    });
    let state = AppState {
        current: Arc::new(ArcSwap::from_pointee(SnapshotState {
            path: path.to_path_buf(),
            snapshot: Some(Arc::new(opened)),
            neighbors: Some(Arc::new(neighbors)),
            neighbors_rev: Some(Arc::new(neighbors_rev)),
            globals: Arc::new(globals),
            macro_lookup: Arc::new(macro_lookup),
            loaded_at_unix: 123,
            snapshot_hash_hex: None,
            route_cache: navpath_service::new_route_cache(),
            seed_shadow: navpath_service::new_seed_shadow(),
            fairy_rings: Arc::new(Vec::new()),
            node_to_fairy_ring: Arc::new(navpath_service::FxHashMap::default()),
            comp_graph,
            canonical_grid: None,
            profile_cache: navpath_service::new_profile_cache(),
            subpath_cache: navpath_service::new_subpath_cache(),
            warm_state: Default::default(),
        })),
        search_permits: navpath_service::default_search_permits(),
        metrics: Arc::new(navpath_service::Metrics::default()),
        ctx_pool: navpath_service::ContextPool::new(),
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    build_router(state)
}

async fn post(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value, String) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), text)
}

fn tile(x: i32, y: i32) -> Value {
    json!({"wx": x, "wy": y, "plane": 0})
}

fn area(x0: i32, y0: i32, x1: i32, y1: i32) -> Value {
    json!({"min_wx": x0, "min_wy": y0, "max_wx": x1, "max_wy": y1, "plane": 0})
}

#[tokio::test]
async fn ends_on_the_first_reachable_area_tile() {
    let snap = make_snapshot();
    for comp_graph in [false, true] {
        let app = app_for(&snap, comp_graph);
        for seed in [Value::Null, json!(42)] {
            // East column from the west column's foot: up 26, across 2 = 28 steps; the
            // column's foot (3202,3200) would be 54.
            let (st, v, _) = post(&app, "/route_area", json!({
                "start": tile(3200, 3200), "goal_area": area(3202, 3200, 3202, 3226), "seed": seed,
            })).await;
            assert_eq!(st, StatusCode::OK, "{v}");
            assert_eq!(v["found"], json!(true), "{v}");
            assert_eq!(v["goal_reached"], tile(3202, 3226), "{v}");
            assert_eq!(v["length_tiles"], json!(29), "{v}");
            let cost = v["cost"].as_f64().unwrap();
            if seed.is_null() {
                assert_eq!(cost, 28.0 * 300.0);
            } else {
                assert!((cost - 8400.0).abs() < 28.0 * 0.1 + 1e-3, "{cost}");
            }
            // Only the column's top tile can be entered first.
            assert_eq!(v["goal_tiles_resolved"], json!(27), "{v}");
            assert_eq!(v["goal_tiles_searched"], json!(1), "{v}");
            // The path ends on the reached tile and holds no earlier area tile.
            let path: Vec<u64> = v["path"].as_array().unwrap().iter().map(|x| x.as_u64().unwrap()).collect();
            assert_eq!(path.len(), 29);
        }
    }
}

#[tokio::test]
async fn teleport_and_door_landings_inside_the_area_count() {
    let snap = make_snapshot();
    let app = app_for(&snap, true);
    // The door lands on the isolated cell: an interior tile only a macro edge reaches.
    let (st, v, _) = post(&app, "/route_area", json!({
        "start": tile(3202, 3200), "goal_area": area(3204, 3199, 3206, 3201),
    })).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["goal_reached"], tile(3205, 3200), "{v}");
    assert_eq!(v["cost"], json!(600.0), "{v}");

    // Virtual start (off the snapshot): the only way in is the global teleport, which
    // lands INSIDE the west-column area (3200, 3205..3215) — its landing tile wins.
    let (st, v, _) = post(&app, "/route_area", json!({
        "start": {"wx": 3100, "wy": 3100, "plane": 0}, "goal_area": area(3200, 3205, 3200, 3215),
    })).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["found"], json!(true), "{v}");
    assert_eq!(v["goal_reached"], tile(3200, 3210), "{v}");
    assert_eq!(v["cost"], json!(10000.0), "{v}");
}

#[tokio::test]
async fn start_inside_the_area_is_free_and_unreachable_areas_are_not_found() {
    let snap = make_snapshot();
    for comp_graph in [false, true] {
        let app = app_for(&snap, comp_graph);
        let (_, v, _) = post(&app, "/route_area", json!({
            "start": tile(3200, 3205), "goal_area": area(3199, 3203, 3201, 3207),
        })).await;
        assert_eq!(v["found"], json!(true), "{v}");
        assert_eq!(v["cost"], json!(0.0), "{v}");
        assert_eq!(v["goal_reached"], tile(3200, 3205), "{v}");

        let (st, v, text) = post(&app, "/route_area", json!({
            "start": tile(3200, 3200), "goal_tiles": [tile(3300, 3200)],
        })).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["found"], json!(false), "{v}");
        assert!(!text.contains("goal_reached"), "{text}");
    }
}

#[tokio::test]
async fn equal_cost_ties_follow_the_goal_preference() {
    let snap = make_snapshot();
    let app = app_for(&snap, true);
    // From the connector (3201,3226), (3200,3225) and (3202,3225) are both 2 steps.
    for (pref, want) in [(tile(3202, 3224), tile(3202, 3225)), (tile(3200, 3223), tile(3200, 3225))] {
        for _ in 0..3 {
            let (_, v, _) = post(&app, "/route_area", json!({
                "start": tile(3201, 3226), "goal": pref, "goal_tiles": [tile(3200, 3225), tile(3202, 3225)],
            })).await;
            assert_eq!(v["cost"], json!(600.0), "{v}");
            assert_eq!(v["goal_reached"], want, "{v}");
        }
    }
}

#[tokio::test]
async fn bad_requests() {
    let snap = make_snapshot();
    let app = app_for(&snap, true);
    let cases = [
        (json!({"start": tile(3200, 3200)}), "missing goal_area"),
        (json!({"goal_area": area(3200, 3200, 3201, 3201)}), "missing start"),
        (json!({"start": tile(3200, 3200), "goal_area": area(3400, 3400, 3410, 3410)}), "no goal_area tile in snapshot"),
        (json!({"start": tile(3200, 3200), "goal_area": area(3210, 3200, 3200, 3210)}), "must be >="),
        (json!({"start": tile(3200, 3200), "goal_area": area(3000, 3000, 3064, 3063)}), "goal_area too large"),
    ];
    for (body, msg) in cases {
        let (st, _, text) = post(&app, "/route_area", body.clone()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}: {text}");
        assert!(text.contains(msg), "{body}: {text}");
    }
    // goal_tiles may span planes: a plane-1 tile that does not exist is just skipped.
    let (st, v, _) = post(&app, "/route_area", json!({
        "start": tile(3200, 3200), "goal_tiles": [{"wx": 3200, "wy": 3201, "plane": 1}, tile(3200, 3202)],
    })).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["goal_reached"], tile(3200, 3202), "{v}");
}

#[tokio::test]
async fn single_tile_route_is_unchanged() {
    // /route keeps its contract: no goal_reached, area fields ignored.
    let snap = make_snapshot();
    let app = app_for(&snap, true);
    let (st, v, text) = post(&app, "/route", json!({
        "start": tile(3200, 3200), "goal": tile(3202, 3200), "goal_area": area(3202, 3200, 3202, 3226),
    })).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["cost"], json!(54.0 * 300.0), "{v}");
    assert!(!text.contains("goal_reached"), "{text}");
}
