//! Payload-compatibility harness for response-construction refactors (roadmap 5.3).
//!
//! Issues a deterministic matrix of /route requests against the REAL snapshot through
//! the full axum router and canonicalizes the JSON responses (serde_json's BTreeMap
//! already sorts keys; the volatile `duration_ms` field is stripped). Two modes:
//!
//!   cargo run --release -p navpath-service --example payload_diff -- --capture
//!       write tools/payload_baseline.json from the current code
//!   cargo run --release -p navpath-service --example payload_diff
//!       re-run the matrix and fail (exit 1) on ANY semantic difference vs baseline
//!
//! Refactors that intend to change payloads must re-capture and justify the diff.

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn strip_volatile(v: &mut Value) {
    if let Some(obj) = v.as_object_mut() {
        obj.remove("duration_ms");
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Deterministic, cache-free, budget-free captures.
    std::env::set_var("NAVPATH_ROUTE_CACHE", "0");
    std::env::set_var("NAVPATH_MAX_POPS", "0");

    let capture = std::env::args().any(|a| a == "--capture");
    let baseline_path = format!("{}/../../tools/payload_baseline.json", env!("CARGO_MANIFEST_DIR"));
    let snap_path = std::env::var("SNAPSHOT_PATH")
        .unwrap_or_else(|_| format!("{}/../../graph.snapshot", env!("CARGO_MANIFEST_DIR")));
    if !std::path::Path::new(&snap_path).exists() {
        println!("payload_diff: no snapshot at {snap_path}; skipping (exit 0)");
        return;
    }

    let snap = navpath_core::Snapshot::open(&snap_path).expect("open snapshot");
    let (n, nr, g, m) = navpath_service::engine_adapter::build_neighbor_provider(&snap);
    let (fr, nfr) = navpath_service::engine_adapter::build_fairy_rings(&snap);
    let cg = navpath_service::engine_adapter::build_component_graph(&snap, &g, &fr);
    let canon = navpath_service::engine_adapter::build_canonical_grid(&snap);
    let state = navpath_service::AppState {
        current: Arc::new(arc_swap::ArcSwap::from_pointee(navpath_service::SnapshotState {
            path: snap_path.clone().into(),
            snapshot: Some(Arc::new(snap)),
            neighbors: Some(Arc::new(n)),
            neighbors_rev: Some(Arc::new(nr)),
            globals: Arc::new(g),
            macro_lookup: Arc::new(m),
            loaded_at_unix: 0,
            snapshot_hash_hex: None,
            route_cache: navpath_service::new_route_cache(),
            fairy_rings: Arc::new(fr),
            node_to_fairy_ring: Arc::new(nfr),
            comp_graph: Some(Arc::new(cg)),
            canonical_grid: canon,
            profile_cache: navpath_service::new_profile_cache(),
        })),
        search_permits: navpath_service::default_search_permits(),
        metrics: Arc::new(navpath_service::Metrics::default()),
        ctx_pool: navpath_service::ContextPool::new(),
    };
    let app = navpath_service::build_router(state);

    // Request matrix: every payload shape the response builder produces — walk moves,
    // macro/global/fairy actions, geometry, virtual-start synthetic action, surge/dive
    // rewriting, only_actions, seeded and unseeded, permissive and gated profiles.
    let quick = json!([{"key": "hasQuickTele", "value": 1}]);
    let none: Value = json!([]);
    let mut matrix: Vec<(String, Value)> = Vec::new();
    for (name, start, goal) in [
        ("readme", json!({"wx": 3259, "wy": 3101, "plane": 0}), json!({"wx": 3425, "wy": 3017, "plane": 0})),
        ("incident", json!({"wx": 2887, "wy": 3535, "plane": 0}), json!({"wx": 3563, "wy": 3408, "plane": 0})),
        ("short", json!({"wx": 3222, "wy": 3218, "plane": 0}), json!({"wx": 3230, "wy": 3220, "plane": 0})),
        ("virtual", json!({"wx": 30000, "wy": 20000, "plane": 0}), json!({"wx": 3425, "wy": 3017, "plane": 0})),
    ] {
        for (opts_name, opts) in [
            ("actions", json!({"return_geometry": false, "only_actions": true})),
            ("geom", json!({"return_geometry": true, "only_actions": false})),
            ("both", json!({"return_geometry": true, "only_actions": true})),
        ] {
            for (prof_name, reqs, qt) in [("all", &none, false), ("quick", &quick, true)] {
                let _ = qt;
                for seed in [Value::Null, json!(12345)] {
                    let seed_name = if seed.is_null() { "unseeded" } else { "seeded" };
                    let body = json!({
                        "start": start,
                        "goal": goal,
                        "profile": {"requirements": reqs},
                        "options": opts,
                        "surge": {"enabled": true, "charges": 2, "cooldown_ms": 20400.0},
                        "dive": {"enabled": true, "cooldown_ms": 20400.0},
                        "seed": seed,
                    });
                    matrix.push((format!("{name}/{opts_name}/{prof_name}/{seed_name}"), body));
                }
            }
        }
    }

    let mut results = serde_json::Map::new();
    for (name, body) in matrix {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/route")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status().as_u16();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let mut v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        strip_volatile(&mut v);
        results.insert(name, json!({"status": status, "body": v}));
    }
    let canonical = Value::Object(results);

    if capture {
        std::fs::write(&baseline_path, serde_json::to_string_pretty(&canonical).unwrap() + "\n")
            .expect("write baseline");
        println!("payload_diff: captured {} responses into {baseline_path}",
                 canonical.as_object().unwrap().len());
        return;
    }

    let baseline: Value = serde_json::from_str(
        &std::fs::read_to_string(&baseline_path).expect("read baseline — run --capture first"),
    )
    .expect("parse baseline");
    let mut failures = 0;
    for (name, expect) in baseline.as_object().unwrap() {
        match canonical.get(name) {
            Some(got) if got == expect => {}
            Some(got) => {
                failures += 1;
                eprintln!("PAYLOAD DIFF at {name}:");
                eprintln!("  expected: {}", serde_json::to_string(expect).unwrap());
                eprintln!("  got:      {}", serde_json::to_string(got).unwrap());
            }
            None => {
                failures += 1;
                eprintln!("MISSING response for {name}");
            }
        }
    }
    println!(
        "payload_diff: {} cases, {failures} difference(s)",
        baseline.as_object().unwrap().len()
    );
    std::process::exit(if failures == 0 { 0 } else { 1 });
}
