use crate::config::Config;
use crate::errors::AppError;
use crate::models::{FindPathRequest, Tile};
use crate::planner::micro_astar::find_path_4dir;
use crate::requirements::RequirementEvaluator;
use crate::serialization::{move_action, serialize_path};
use crate::db::Db;
use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info};
use tokio::task;

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    db_path: Option<PathBuf>,
}

pub fn build_router(config: Arc<Config>) -> Router {
    let state = AppState { config: Arc::clone(&config), db_path: config.db_path.clone() };

    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/version", get(version))
        .route("/find_path", post(find_path))
        .with_state(state)
}

#[derive(Serialize)]
struct Healthz {
    status: &'static str,
}

async fn healthz() -> Json<Healthz> {
    Json(Healthz { status: "ok" })
}

#[derive(Serialize)]
struct Readyz {
    ready: bool,
}

#[tracing::instrument(skip(state))]
async fn readyz(State(state): State<AppState>) -> Json<Readyz> {
    let t0 = Instant::now();
    let ready = if let Some(db_path) = &state.db_path {
        let db_path = db_path.clone();
        match task::spawn_blocking(move || {
            match Db::open_read_only(&db_path) {
                Ok(db) => {
                    db.list_clusters(1).is_ok()
                }
                Err(_) => false,
            }
        })
        .await
        {
            Ok(ok) => ok,
            Err(_) => false,
        }
    } else {
        false
    };
    let dt = t0.elapsed();
    debug!(ready, took_ms = %dt.as_millis(), "readyz probe completed");
    Json(Readyz { ready })
}

#[derive(Serialize)]
struct Version<'a> {
    version: &'a str,
}

async fn version() -> Json<Version<'static>> {
    Json(Version { version: env!("CARGO_PKG_VERSION") })
}

#[derive(Deserialize, Default)]
struct FindPathQuery {
    #[serde(default)]
    only_actions: Option<String>,
}

fn parse_only_actions(flag: &Option<String>) -> bool {
    match flag.as_deref() {
        Some("1") | Some("true") | Some("TRUE") | Some("True") => true,
        _ => false,
    }
}

async fn find_path(
    State(state): State<AppState>,
    Query(q): Query<FindPathQuery>,
    Json(body): Json<FindPathRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let t_total = Instant::now();
    // Validate tiles are integers by type system; ensure same plane
    if body.start.plane != body.end.plane {
        return Err(AppError::BadRequest("start and end must be on the same plane".to_string()));
    }

    // Build requirement evaluator (ignore extra fields by design)
    let evaluator = RequirementEvaluator::new(body.requirements.as_slice());
    let _ = evaluator; // reserved for future teleport gating in full HPA*

    // If DB is configured, open per-request read-only connection to check walkability
    let db_opt: Option<std::sync::Arc<Db>> = match &state.db_path {
        Some(p) => Db::open_read_only(p).ok().map(std::sync::Arc::new),
        None => None,
    };
    let db_for_walk = db_opt.clone();
    let is_walkable = move |x: i32, y: i32, p: i32| -> bool {
        if let Some(ref db) = db_for_walk {
            db.get_tile(x, y, p)
                .ok()
                .flatten()
                .map(|t| t.blocked == 0)
                .unwrap_or(true)
        } else {
            true
        }
    };

    // Allowed area: if DB is configured, constrain to tiles that exist in DB; else unconstrained
    let plane = body.start.plane;
    let db_for_allowed = db_opt;
    let allowed = move |x: i32, y: i32| -> bool {
        if let Some(ref db) = db_for_allowed {
            db.get_tile(x, y, plane).ok().flatten().is_some()
        } else {
            true
        }
    };

    // Fast path: identical tiles
    let t_algo = Instant::now();
    let path_tiles: Vec<Tile> = if body.start == body.end {
        vec![body.start]
    } else {
        match find_path_4dir(body.start, body.end, allowed, |x, y| is_walkable(x, y, body.start.plane)) {
            Some(p) => p,
            None => return Err(AppError::BadRequest("no path found".to_string())),
        }
    };
    let algo_ms = t_algo.elapsed().as_millis();

    // Serialize response
    let only_actions = parse_only_actions(&q.only_actions);
    let move_cost = state.config.move_cost_ms.unwrap_or(200) as i64;
    let mut actions = Vec::new();
    for w in path_tiles.windows(2) {
        let a = w[0];
        let b = w[1];
        actions.push(move_action(a, b, move_cost));
    }

    let resp = if only_actions {
        serde_json::json!({ "actions": actions })
    } else {
        serde_json::json!({ "path": serialize_path(&path_tiles), "actions": actions })
    };

    let total_ms = t_total.elapsed().as_millis();
    info!(
        start = %format!("{}:{}:{}", body.start.x, body.start.y, body.start.plane),
        end = %format!("{}:{}:{}", body.end.x, body.end.y, body.end.plane),
        path_len = path_tiles.len(),
        actions = actions.len(),
        algo_ms = %algo_ms,
        total_ms = %total_ms,
        "find_path completed"
    );

    Ok(Json(resp))
}
