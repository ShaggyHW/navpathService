mod config;
mod errors;
mod models;
mod routes;
mod db;
mod planner;
mod requirements;
mod serialization;

use crate::config::Config;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() {
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info,navpath_service=debug,axum=info"))
        .expect("failed to init EnvFilter");
    fmt().with_env_filter(env_filter).init();

    let config = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            error!(error = %e, "failed to load configuration");
            eprintln!("failed to load configuration: {e}");
            std::process::exit(1);
        }
    };
    let config = Arc::new(config);

    let app = routes::build_router(Arc::clone(&config));

    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .expect("invalid host/port combination");
    info!(%addr, version = env!("CARGO_PKG_VERSION"), "starting navpath-service");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind address");

    if let Err(e) = axum::serve(listener, app.into_make_service()).await {
        error!(error = %e, "server error");
    }
}
