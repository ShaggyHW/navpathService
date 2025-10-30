use crate::config::Config;
use crate::errors::AppError;
use crate::models::{FindPathRequest, Tile};
use crate::planner::graph::GraphInputs;
use crate::planner::hpa::{plan as hpa_plan, HpaInputs, HpaOptions};
use crate::requirements::RequirementEvaluator;
use crate::serialization::{move_action, serialize_path};
use crate::db::Db;
use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use anyhow::anyhow;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};
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
    if body.start.plane != body.end.plane && state.db_path.is_none() {
        return Err(AppError::BadRequest("cross-plane paths require database/teleports enabled".to_string()));
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
    let db_for_allowed = db_opt.clone();
    let allowed = move |x: i32, y: i32| -> bool {
        if let Some(ref db) = db_for_allowed {
            db.get_tile(x, y, plane).ok().flatten().is_some()
        } else {
            true
        }
    };

    // Run planner
    let t_algo = Instant::now();
    // Always require DB/HPA; treat missing DB as internal error (service should not start without NAVPATH_DB)
    let db = match db_opt {
        Some(db) => db,
        None => return Err(AppError::Internal(anyhow::anyhow!("missing database configuration (NAVPATH_DB)"))),
    };
    let (path_tiles, hpa_extra_actions): (Vec<Tile>, Vec<serde_json::Value>) = {
        // Gather inputs for both planes when start and end differ
        let plane_s = body.start.plane;
        let plane_e = body.end.plane;
        // Entrances
        let mut entrances = db
            .list_cluster_entrances_by_plane(plane_s)
            .map_err(|e| AppError::Internal(e.into()))?;
        if plane_e != plane_s {
            let mut more = db
                .list_cluster_entrances_by_plane(plane_e)
                .map_err(|e| AppError::Internal(e.into()))?;
            entrances.append(&mut more);
        }
        // Intra connections
        let mut intra = db
            .list_cluster_intraconnections_by_plane(plane_s)
            .map_err(|e| AppError::Internal(e.into()))?;
        if plane_e != plane_s {
            let mut more = db
                .list_cluster_intraconnections_by_plane(plane_e)
                .map_err(|e| AppError::Internal(e.into()))?;
            intra.append(&mut more);
        }
        // Inter connections
        let mut inter = db
            .list_cluster_interconnections_by_plane(plane_s)
            .map_err(|e| AppError::Internal(e.into()))?;
        if plane_e != plane_s {
            let mut more = db
                .list_cluster_interconnections_by_plane(plane_e)
                .map_err(|e| AppError::Internal(e.into()))?;
            inter.append(&mut more);
        }
        // Teleports (edges allowed to connect across the two planes)
        let teleports = db
            .list_abstract_teleport_edges_for_planes(plane_s, plane_e)
            .map_err(|e| AppError::Internal(e.into()))?;
        let teleport_requirements = db
            .list_teleport_requirements()
            .map_err(|e| AppError::Internal(e.into()))?;

        // Build cluster tiles map for all clusters referenced by entrances
        let mut cluster_tiles: std::collections::HashMap<i64, std::collections::HashSet<(i32, i32, i32)>> =
            std::collections::HashMap::new();
        for e in entrances.iter() {
            let cid = e.cluster_id;
            if !cluster_tiles.contains_key(&cid) {
                if let Ok(tiles) = db.list_cluster_tiles(cid) {
                    let mut set = std::collections::HashSet::new();
                    for t in tiles {
                        set.insert((t.x as i32, t.y as i32, t.plane as i32));
                    }
                    cluster_tiles.insert(cid, set);
                }
            }
        }

        // Build inputs and run HPA
        let graph_inputs = GraphInputs {
            entrances: &entrances,
            intra: &intra,
            inter: &inter,
            teleports: &teleports,
            teleport_requirements: &teleport_requirements,
        };
        let hpa_inputs = HpaInputs {
            graph_inputs,
            cluster_tiles,
            is_walkable: Box::new(move |x: i32, y: i32, p: i32| is_walkable(x, y, p)),
        };
        let opts = HpaOptions { start: body.start, end: body.end };
        match hpa_plan(&hpa_inputs, &evaluator, &opts) {
            Some(res) => (res.path, res.actions),
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
    // Include any non-move actions produced by HPA (e.g., teleports)
    actions.extend(hpa_extra_actions);

    let resp = if only_actions {
        serde_json::json!({ "actions": actions })
    } else {
        serde_json::json!({ "path": serialize_path(&path_tiles), "actions": actions })
    };

    if let Some(path) = &state.config.debug_result_path {
        if let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(&resp).unwrap_or_default()) {
            warn!(error = %e, file = %path.display(), "failed to write debug result file");
        }
    }

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
