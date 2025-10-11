use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::{get, post}, Json, Router};
use axum::extract::Query;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, info, info_span};

use navpath_core::{astar::AStar, CostModel, PathResult, SearchOptions, Tile};
 // trait bound

use crate::provider_manager::ProviderManager;

#[derive(Clone)]
pub struct AppState {
    pub providers: ProviderManager,
}

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
    match state.providers.warm_default() {
        Ok(()) => (StatusCode::OK, Json(json!({"ready": true}))).into_response(),
        Err(e) => {
            error!(error=%e.to_string(), "readyz failed");
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ready": false, "error": e.to_string()}))).into_response()
        }
    }
}

async fn version() -> impl IntoResponse {
    let svc_version = env!("CARGO_PKG_VERSION");
    let core_version = navpath_core::version();
    (StatusCode::OK, Json(json!({"service_version": svc_version, "core_version": core_version})))
}

async fn find_path(State(state): State<AppState>, Query(params): Query<FindPathQuery>, Json(req): Json<FindPathRequest>) -> impl IntoResponse {
    let span = info_span!("find_path", db = req.db_path.as_deref().unwrap_or("<default>"));
    let _enter = span.enter();

    let start: Tile = req.start;
    let goal: Tile = req.goal;
    let mut options = req.options.unwrap_or_else(SearchOptions::default);
    // Optionally embed start_tile for lodestone gating logic parity
    options.extras.entry("start_tile".into()).or_insert(serde_json::json!([start[0], start[1], start[2]]));

    let result = state.providers.with_provider(req.db_path.as_deref(), |prov| {
        // Build cost model per request and set on provider
        let cm = CostModel::new(options.clone());
        prov.set_cost_model(cm.clone());
        // Run A*
        let astar = AStar::new(prov, &cm);
        let res = astar.find_path(start, goal, &options)?;
        Ok::<PathResult, anyhow::Error>(res)
    });

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
