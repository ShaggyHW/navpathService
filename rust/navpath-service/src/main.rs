use tracing_subscriber::{fmt, EnvFilter};

mod config;
mod provider_manager;
mod routes;

#[tokio::main]
async fn main() {
    // Structured logging
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).json().init();

    let cfg = config::Config::from_env();
    let state = routes::AppState { providers: provider_manager::ProviderManager::new(cfg.default_db.clone()) };
    let app = routes::build_router(state);
    tracing::info!(core_version=%navpath_core::version(), addr=%cfg.addr.to_string(), "starting navpath-service");
    let listener = tokio::net::TcpListener::bind(cfg.addr).await.expect("bind failed");
    axum::serve(listener, app).await.expect("server error");
}
