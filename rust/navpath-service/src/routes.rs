use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::{get, post}, Json, Router};
use axum::extract::Query;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, info, info_span};

use std::sync::atomic::Ordering;

use navpath_core::{astar::AStar, CostModel, PathResult, SearchOptions, Tile};
use navpath_core::db::{self, Database};
use navpath_core::graph::provider::SqliteGraphProvider;

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct FindPathRequest {
    pub start: Tile,
    pub goal: Tile,
    pub options: Option<SearchOptions>,
    pub db_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FindPathQuery {
    #[serde(default, alias = "actions_only")]
    pub only_actions: bool,
    #[serde(default)]
    pub db: Option<String>,
    #[serde(default, alias = "db_path")]
    pub db_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Healthz { pub status: &'static str }

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/version", get(version))
        .route("/find_path", post(find_path))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(Healthz { status: "ok" }))
}

async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    if !state.ready.load(Ordering::Relaxed) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ready": false}))).into_response();
    }
    (StatusCode::OK, Json(json!({"ready": true}))).into_response()
}

async fn version() -> impl IntoResponse {
    let svc_version = env!("CARGO_PKG_VERSION");
    let core_version = navpath_core::version();
    (StatusCode::OK, Json(json!({"service_version": svc_version, "core_version": core_version})))
}

async fn find_path(State(state): State<AppState>, Query(params): Query<FindPathQuery>, Json(req): Json<FindPathRequest>) -> impl IntoResponse {
    // Reject per-request DB selection
    if params.db.is_some() || params.db_path.is_some() || req.db_path.is_some() {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "ERR_DB_SELECTION_UNSUPPORTED",
            "message": "Selecting database per request is not supported"
        }))).into_response();
    }

    let span = info_span!("find_path");
    let _enter = span.enter();

    let start: Tile = req.start;
    let goal: Tile = req.goal;
    let mut options = req.options.unwrap_or_else(SearchOptions::default);
    // Optionally embed start_tile for lodestone gating logic parity
    options.extras.entry("start_tile".into()).or_insert(serde_json::json!([start[0], start[1], start[2]]));

    // Open DB read-only per request and build a provider with per-request CostModel
    let conn = match db::open::open_read_only_with_config(&state.db_path, &state.db_open_config) {
        Ok(c) => c,
        Err(e) => {
            error!(error=%e.to_string(), "db open failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response();
        }
    };
    let db = Database::from_connection(conn);
    let cm = CostModel::new(options.clone());
    let provider = SqliteGraphProvider::new(db, cm.clone());
    let astar = AStar::new(&provider, &cm);
    let result: Result<PathResult, anyhow::Error> = astar.find_path(start, goal, &options).map_err(|e| e.into());

    match result {
        Ok(res) => {
            let path_len = res.path.as_ref().map(|p| p.len()).unwrap_or(0);
            info!(reason=?res.reason, expanded=res.expanded, path_len, total_cost_ms=res.cost_ms, "find_path done");
            // Check query flag OR body options.extras.only_actions for parity
            let body_only_actions = {
                let val = options.extras.get("only_actions").or_else(|| options.extras.get("actions_only"));
                val.map(|v| {
                    v.as_bool()
                        .unwrap_or_else(|| {
                            if let Some(n) = v.as_i64() { n == 1 } else { false }
                        })
                        || v.as_str().map(|s| s.eq_ignore_ascii_case("true") || s == "1").unwrap_or(false)
                }).unwrap_or(false)
            };
            if params.only_actions || body_only_actions {
                return (StatusCode::OK, Json(res.actions)).into_response();
            }
            (StatusCode::OK, Json(res)).into_response()
        }
        Err(e) => {
            error!(error=%e.to_string(), "find_path error");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response()
        }
    }
}
