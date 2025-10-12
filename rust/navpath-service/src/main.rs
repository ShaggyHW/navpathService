use std::sync::atomic::Ordering;

use tracing_subscriber::{fmt, EnvFilter};

mod config;
mod routes;
mod state;

#[tokio::main]
async fn main() {
    // Structured logging
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).json().init();

    // Resolve DB and load open config (log here; do not bind port yet)
    let db_path = match config::resolve_db_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fatal: {}", e);
            std::process::exit(1);
        }
    };
    let db_open_cfg = config::load_db_open_config();
    let jps_mode = config::jps_mode_from_env();
    tracing::info!(db_path=%db_path.display(), db_open_config=?db_open_cfg, jps_mode=?jps_mode, "db configuration resolved");

    // Open DB read-only with config to validate it can be opened
    if let Err(e) = navpath_core::db::open::open_read_only_with_config(&db_path, &db_open_cfg) {
        eprintln!("fatal: failed to open database: {}", e);
        std::process::exit(1);
    }

    // Build service state
    let app_state = state::AppState::new(db_path.clone(), db_open_cfg.clone(), jps_mode);
    app_state.ready.store(true, Ordering::Relaxed);

    // Build routes with AppState and start server
    let cfg = config::Config::from_env();
    let app = routes::build_router(app_state.clone());
    tracing::info!(core_version=%navpath_core::version(), addr=%cfg.addr.to_string(), "starting navpath-service");
    let listener = tokio::net::TcpListener::bind(cfg.addr).await.expect("bind failed");
    axum::serve(listener, app).await.expect("server error");
}
