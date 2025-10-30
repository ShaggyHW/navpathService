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
