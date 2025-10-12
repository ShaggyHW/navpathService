use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::{get, post}, Json, Router};
use axum::extract::Query;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, info, info_span};

use std::sync::atomic::Ordering;

use navpath_core::{astar::AStar, CostModel, PathResult, SearchOptions, Tile, ActionStep, Rect};
use navpath_core::db::{self, Database};
use navpath_core::graph::provider::SqliteGraphProvider;
use navpath_core::graph::navmesh_provider::NavmeshGraphProvider;
use navpath_core::funnel::string_pull;

use crate::state::AppState;
use crate::config::{JpsMode, ProviderMode};

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

    // Apply deployment-time JPS mode (service-wide). Off forces JPS disabled regardless of request.
    match state.jps_mode {
        JpsMode::Off => {
            options.extras.insert("jps_enabled".into(), serde_json::Value::from(false));
        }
        JpsMode::Auto => { /* leave request/default behavior */ }
    }

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
    let result: Result<PathResult, anyhow::Error> = match state.provider_mode {
        ProviderMode::Sqlite => {
            let provider = SqliteGraphProvider::new(db, cm.clone());
            let astar = AStar::new(&provider, &cm);
            astar.find_path(start, goal, &options).map_err(|e| e.into())
        }
        ProviderMode::Navmesh => {
            // Map incoming tile centers to navmesh cells
            let provider = NavmeshGraphProvider::new(db, cm.clone());
            let start_xy = [start[0] as f64 + 0.5, start[1] as f64 + 0.5];
            let goal_xy = [goal[0] as f64 + 0.5, goal[1] as f64 + 0.5];
            let s_cell = match provider.map_point_to_tile(start_xy[0], start_xy[1], start[2]) {
                Ok(Some(t)) => t,
                _ => {
                    return (StatusCode::BAD_REQUEST, Json(json!({
                        "error": "ERR_START_NOT_ON_NAVMESH",
                        "message": "Start tile center does not map to any navmesh cell"
                    }))).into_response();
                }
            };
            let g_cell = match provider.map_point_to_tile(goal_xy[0], goal_xy[1], goal[2]) {
                Ok(Some(t)) => t,
                _ => {
                    return (StatusCode::BAD_REQUEST, Json(json!({
                        "error": "ERR_GOAL_NOT_ON_NAVMESH",
                        "message": "Goal tile center does not map to any navmesh cell"
                    }))).into_response();
                }
            };
            let astar = AStar::new(&provider, &cm);
            match astar.find_path(s_cell, g_cell, &options).map_err(|e| e.into()) {
                Ok(mut res) => {
                    // Derive world-space waypoints from portal metadata
                    let mut portals: Vec<([f64; 2], [f64; 2])> = Vec::new();
                    for a in &res.actions {
                        if a.type_ == "move" {
                            if let Some(m) = a.metadata.as_ref() {
                                if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
                                    m.get("x1").and_then(|v| v.as_f64()),
                                    m.get("y1").and_then(|v| v.as_f64()),
                                    m.get("x2").and_then(|v| v.as_f64()),
                                    m.get("y2").and_then(|v| v.as_f64()),
                                ) {
                                    portals.push(([x1, y1], [x2, y2]));
                                }
                            }
                        }
                    }
                    let waypoints = string_pull(start_xy, &portals, goal_xy, 1e-9);
                    // Append synthetic action step carrying waypoints so clients can render a world path without schema change
                    let synth = ActionStep {
                        type_: "waypoints".into(),
                        from_rect: Rect { min: start, max: start },
                        to_rect: Rect { min: goal, max: goal },
                        cost_ms: 0,
                        node: None,
                        metadata: Some(json!({ "waypoints": waypoints })),
                    };
                    res.actions.push(synth);
                    Ok(res)
                }
                Err(e) => Err(e),
            }
        }
    };

    match result {
        Ok(mut res) => {
            for a in res.actions.iter_mut() {
                if a.type_ != "move" && a.type_ != "waypoints" {
                    if let Some(m) = a.metadata.take() {
                        if let serde_json::Value::Object(map) = m {
                            if let Some(db_row) = map.get("db_row").cloned() {
                                a.metadata = Some(json!({ "db_row": db_row }));
                            } else {
                                a.metadata = None;
                            }
                        } else {
                            a.metadata = None;
                        }
                    }
                }
            }
            let path_len = res.path.as_ref().map(|p| p.len()).unwrap_or(0);
            // Best-effort derived enabled flag for logging. Defaults to true in core when not specified.
            let jps_enabled_log = match state.jps_mode {
                JpsMode::Off => false,
                JpsMode::Auto => options.extras.get("jps_enabled").and_then(|v| v.as_bool()).unwrap_or(true),
            };
            info!(reason=?res.reason, expanded=res.expanded, path_len, total_cost_ms=res.cost_ms, jps_mode=?state.jps_mode, provider_mode=?state.provider_mode, jps_enabled=jps_enabled_log, "find_path done");
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
