use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use navpath_service::config::Config;
use navpath_service::routes::build_router;
use serde_json::json;
use std::sync::Arc;
use tempfile::{tempdir, TempDir};
use tower::util::ServiceExt;
use http_body_util::BodyExt; // for collect()

fn build_test_db() -> (TempDir, std::path::PathBuf) {
    let dir = tempdir().expect("temp dir");
    let path = dir.path().join("test.db");
    let conn = rusqlite::Connection::open(&path).expect("open rw db");
    conn.execute(
        "CREATE TABLE tiles (
            x INTEGER, y INTEGER, plane INTEGER,
            flag INTEGER, blocked INTEGER, walk_mask INTEGER, blocked_mask INTEGER, walk_data TEXT
        )",
        [],
    )
    .unwrap();

    // Create a small 3x1 corridor at y=0, plane=0: (0,0)->(1,0)->(2,0)
    for x in 0..=2 {
        conn.execute(
            "INSERT INTO tiles (x,y,plane,flag,blocked,walk_mask,blocked_mask,walk_data) VALUES (?1,0,0,0,0,0,0,NULL)",
            rusqlite::params![x],
        )
        .unwrap();
    }
    (dir, path)
}

fn build_router_with_db(db_path: std::path::PathBuf) -> Router {
    let cfg = Config {
        host: "127.0.0.1".into(),
        port: 0,
        db_path: Some(db_path),
        move_cost_ms: Some(200),
        debug_result_path: None,
    };
    build_router(Arc::new(cfg))
}

fn build_router_without_db() -> Router {
    let cfg = Config {
        host: "127.0.0.1".into(),
        port: 0,
        db_path: None,
        move_cost_ms: Some(200),
        debug_result_path: None,
    };
    build_router(Arc::new(cfg))
}

#[tokio::test]
async fn readyz_true_with_valid_db() {
    let (dir, db_path) = build_test_db();
    let _dir_guard = dir;
    let app = build_router_with_db(db_path);

    let req: Request<Body> = Request::builder().uri("/readyz").body(Body::empty()).unwrap();
    let res = app
        .clone()
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["ready"].is_boolean());
}

#[tokio::test]
async fn find_path_returns_path_and_actions() {
    let (dir, db_path) = build_test_db();
    let _dir_guard = dir;
    let app = build_router_with_db(db_path);

    let req_body = json!({
        "start": {"x":0, "y":0, "plane":0},
        "end": {"x":2, "y":0, "plane":0},
        "requirements": [ {"key":"token", "value":"yes"} ]
    });

    let req: Request<Body> = Request::builder()
        .method("POST")
        .uri("/find_path")
        .header("content-type", "application/json")
        .body(Body::from(req_body.to_string()))
        .unwrap();
    let res = app
        .clone()
        .oneshot(req)
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Validate shape and determinism
    assert!(v.get("path").is_some());
    assert_eq!(v["path"], json!([[0,0,0],[1,0,0],[2,0,0]]));
    assert!(v.get("actions").is_some());
    let actions = v["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0]["type"], json!("move"));
    assert_eq!(actions[0]["from"]["min"], json!([0,0,0]));
    assert_eq!(actions[0]["to"]["min"], json!([1,0,0]));
}

#[tokio::test]
async fn find_path_only_actions_flag() {
    let (dir, db_path) = build_test_db();
    let _dir_guard = dir;
    let app = build_router_with_db(db_path);

    let req_body = json!({
        "start": {"x":0, "y":0, "plane":0},
        "end": {"x":2, "y":0, "plane":0}
    });

    let req: Request<Body> = Request::builder()
        .method("POST")
        .uri("/find_path?only_actions=true")
        .header("content-type", "application/json")
        .body(Body::from(req_body.to_string()))
        .unwrap();
    let res = app
        .clone()
        .oneshot(req)
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.get("path").is_none());
    assert!(v.get("actions").is_some());
}

#[tokio::test]
async fn find_path_unreachable_returns_400() {
    // Build DB but block the middle tile
    let dir = tempdir().expect("temp dir");
    let path = dir.path().join("blocked.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "CREATE TABLE tiles (
            x INTEGER, y INTEGER, plane INTEGER,
            flag INTEGER, blocked INTEGER, walk_mask INTEGER, blocked_mask INTEGER, walk_data TEXT
        )",
        [],
    )
    .unwrap();
    // start, middle (blocked), end
    conn.execute(
        "INSERT INTO tiles (x,y,plane,flag,blocked,walk_mask,blocked_mask,walk_data) VALUES (0,0,0,0,0,0,0,NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO tiles (x,y,plane,flag,blocked,walk_mask,blocked_mask,walk_data) VALUES (1,0,0,0,1,0,0,NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO tiles (x,y,plane,flag,blocked,walk_mask,blocked_mask,walk_data) VALUES (2,0,0,0,0,0,0,NULL)",
        [],
    )
    .unwrap();

    let app = build_router_with_db(path.clone());
    let _dir_guard = dir;

    let req_body = json!({
        "start": {"x":0, "y":0, "plane":0},
        "end": {"x":2, "y":0, "plane":0}
    });

    let req: Request<Body> = Request::builder()
        .method("POST")
        .uri("/find_path")
        .header("content-type", "application/json")
        .body(Body::from(req_body.to_string()))
        .unwrap();
    let res = app
        .oneshot(req)
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], json!("bad_request"));
}

#[tokio::test]
async fn find_path_missing_db_returns_500() {
    let app = build_router_without_db();

    let req_body = json!({
        "start": {"x":0, "y":0, "plane":0},
        "end": {"x":1, "y":0, "plane":0}
    });

    let req: Request<Body> = Request::builder()
        .method("POST")
        .uri("/find_path")
        .header("content-type", "application/json")
        .body(Body::from(req_body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();

    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], json!("internal"));
}

fn build_cluster_db_same_cluster() -> (TempDir, std::path::PathBuf) {
    let dir = tempdir().expect("temp dir");
    let path = dir.path().join("cluster_same.db");
    let conn = rusqlite::Connection::open(&path).expect("open rw db");
    conn.execute(
        "CREATE TABLE tiles (
            x INTEGER, y INTEGER, plane INTEGER,
            flag INTEGER, blocked INTEGER, walk_mask INTEGER, blocked_mask INTEGER, walk_data TEXT
        )",
        [],
    ).unwrap();
    conn.execute(
        "CREATE TABLE cluster_tiles (cluster_id INTEGER, x INTEGER, y INTEGER, plane INTEGER)",
        [],
    ).unwrap();
    // 3-tile corridor and cluster membership for cluster 1
    for x in 0..=2 {
        conn.execute(
            "INSERT INTO tiles (x,y,plane,flag,blocked,walk_mask,blocked_mask,walk_data) VALUES (?1,0,0,0,0,0,0,NULL)",
            rusqlite::params![x],
        ).unwrap();
        conn.execute(
            "INSERT INTO cluster_tiles (cluster_id,x,y,plane) VALUES (1,?1,0,0)",
            rusqlite::params![x],
        ).unwrap();
    }
    (dir, path)
}

#[tokio::test]
async fn find_path_same_cluster_micro_path() {
    let (dir, db_path) = build_cluster_db_same_cluster();
    let _dir_guard = dir;
    let app = build_router_with_db(db_path);

    let req_body = json!({
        "start": {"x":0, "y":0, "plane":0},
        "end": {"x":2, "y":0, "plane":0}
    });

    let req: Request<Body> = Request::builder()
        .method("POST")
        .uri("/find_path")
        .header("content-type", "application/json")
        .body(Body::from(req_body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["path"], json!([[0,0,0],[1,0,0],[2,0,0]]));
    let actions = v["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 2);
    assert!(actions.iter().all(|a| a["type"] == json!("move")));
}

fn build_cluster_db_cross_cluster_teleport() -> (TempDir, std::path::PathBuf) {
    let dir = tempdir().expect("temp dir");
    let path = dir.path().join("cluster_tp.db");
    let conn = rusqlite::Connection::open(&path).expect("open rw db");
    // Tables
    conn.execute(
        "CREATE TABLE tiles (x INTEGER, y INTEGER, plane INTEGER, flag INTEGER, blocked INTEGER, walk_mask INTEGER, blocked_mask INTEGER, walk_data TEXT)",
        [],
    ).unwrap();
    conn.execute("CREATE TABLE cluster_tiles (cluster_id INTEGER, x INTEGER, y INTEGER, plane INTEGER)", []).unwrap();
    conn.execute("CREATE TABLE cluster_entrances (entrance_id INTEGER, cluster_id INTEGER, x INTEGER, y INTEGER, plane INTEGER, neighbor_dir TEXT, teleport_edge_id INTEGER)", []).unwrap();
    conn.execute("CREATE TABLE cluster_intraconnections (entrance_from INTEGER, entrance_to INTEGER, cost INTEGER, path_blob BLOB)", []).unwrap();
    conn.execute("CREATE TABLE cluster_interconnections (entrance_from INTEGER, entrance_to INTEGER, cost INTEGER)", []).unwrap();
    conn.execute("CREATE TABLE abstract_teleport_edges (edge_id INTEGER, kind TEXT, node_id INTEGER, src_x INTEGER, src_y INTEGER, src_plane INTEGER, dst_x INTEGER, dst_y INTEGER, dst_plane INTEGER, cost INTEGER, requirement_id INTEGER, src_entrance INTEGER, dst_entrance INTEGER)", []).unwrap();
    conn.execute("CREATE TABLE teleports_requirements (id INTEGER, metaInfo TEXT, key TEXT, value TEXT, comparison TEXT)", []).unwrap();

    // Tiles: start at (-1,0,0), entrance1 at (0,0,0), entrance2 at (2,0,0), end at (3,0,0)
    for x in -1..=3 {
        conn.execute(
            "INSERT INTO tiles (x,y,plane,flag,blocked,walk_mask,blocked_mask,walk_data) VALUES (?1,0,0,0,0,0,0,NULL)",
            rusqlite::params![x],
        ).unwrap();
    }
    // Cluster 100: tiles -1..=0, Cluster 200: tiles 2..=3
    for x in -1..=0 {
        conn.execute("INSERT INTO cluster_tiles (cluster_id,x,y,plane) VALUES (100,?1,0,0)", rusqlite::params![x]).unwrap();
    }
    for x in 2..=3 {
        conn.execute("INSERT INTO cluster_tiles (cluster_id,x,y,plane) VALUES (200,?1,0,0)", rusqlite::params![x]).unwrap();
    }
    // Entrances
    conn.execute("INSERT INTO cluster_entrances (entrance_id,cluster_id,x,y,plane,neighbor_dir,teleport_edge_id) VALUES (1,100,0,0,0,'',NULL)", []).unwrap();
    conn.execute("INSERT INTO cluster_entrances (entrance_id,cluster_id,x,y,plane,neighbor_dir,teleport_edge_id) VALUES (2,200,2,0,0,'',NULL)", []).unwrap();

    // Teleport edge 1->2 with requirement 42
    conn.execute(
        "INSERT INTO abstract_teleport_edges (edge_id,kind,node_id,src_x,src_y,src_plane,dst_x,dst_y,dst_plane,cost,requirement_id,src_entrance,dst_entrance)
         VALUES (7,'item',123,NULL,NULL,NULL,2,0,0,5,42,1,2)",
        [],
    ).unwrap();
    conn.execute(
        "INSERT INTO teleports_requirements (id, metaInfo, key, value, comparison) VALUES (42, NULL, 'level', '10', '>=')",
        [],
    ).unwrap();

    (dir, path)
}

#[tokio::test]
async fn find_path_cross_cluster_via_teleport_includes_action() {
    let (dir, db_path) = build_cluster_db_cross_cluster_teleport();
    let _dir_guard = dir;
    let app = build_router_with_db(db_path);

    let req_body = json!({
        "start": {"x":-1, "y":0, "plane":0},
        "end": {"x":3, "y":0, "plane":0},
        "requirements": [ {"key": "level", "value": 50} ]
    });

    let req: Request<Body> = Request::builder()
        .method("POST")
        .uri("/find_path")
        .header("content-type", "application/json")
        .body(Body::from(req_body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // Path should stitch start->e1 micro, teleport to e2, then e2->end micro
    assert_eq!(v["path"], json!([[-1,0,0],[0,0,0],[2,0,0],[3,0,0]]));

    // Actions contain move actions and a teleport action with correct ids
    let actions = v["actions"].as_array().unwrap();
    assert!(actions.iter().any(|a| a["type"] == json!("teleport") && a["edge_id"] == json!(7) && a["requirement_id"] == json!(42)));
}
