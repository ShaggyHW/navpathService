use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Result;
use arc_swap::ArcSwap;
use tokio::net::TcpListener;
use navpath_core::Snapshot;
use tracing::{error, info};
use tracing_subscriber::FmtSubscriber;

use navpath_service::{
    build_router,
    AppState,
    SnapshotState,
    env_var,
    read_tail_hash_hex,
    now_unix,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Parse the `--dump-result <path>` / `--dump-result=<path>` flag. When present it sets
/// `NAVPATH_DUMP_RESULT` so the existing (env-driven, cached) dump path in `routes.rs` picks
/// it up. The flag takes precedence over an already-set env var; absent it, the env var still
/// works. Must run before the first `/route` request, which is where the path is read & cached.
fn apply_dump_result_flag() {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let path = if arg == "--dump-result" {
            args.next()
        } else if let Some(rest) = arg.strip_prefix("--dump-result=") {
            Some(rest.to_string())
        } else {
            None
        };
        if let Some(path) = path {
            std::env::set_var("NAVPATH_DUMP_RESULT", path);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let subscriber = FmtSubscriber::builder().with_ansi(false).finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    apply_dump_result_flag();

    let host = env_var("NAVPATH_HOST", "127.0.0.1");
    let port: u16 = env_var("NAVPATH_PORT", "8080").parse().unwrap_or(8080);
    let snapshot_path = PathBuf::from(env_var("SNAPSHOT_PATH", "./graph.snapshot"));

    let (snapshot, neighbors, neighbors_rev, globals, macro_lookup, fairy_rings, node_to_fairy_ring, comp_graph, canonical_grid) = match Snapshot::open(&snapshot_path) {
        Ok(s) => {
             let (n, nr, g, m) = navpath_service::engine_adapter::build_neighbor_provider(&s);
             let (fr, nfr) = navpath_service::engine_adapter::build_fairy_rings(&s);
             let cg = navpath_service::engine_adapter::build_component_graph(&s, &g, &fr);
             let canon = navpath_service::engine_adapter::build_canonical_grid(&s);
             (Some(Arc::new(s)), Some(Arc::new(n)), Some(Arc::new(nr)), Arc::new(g), Arc::new(m), Arc::new(fr), Arc::new(nfr), Some(Arc::new(cg)), canon)
        },
        Err(e) => {
            error!(error=?e, path=?snapshot_path, "failed to open snapshot; service will still start but /route will 503");
            (None, None, None, Arc::new(Vec::new()), Arc::new(std::collections::HashMap::<(u32, u32), Vec<u32>>::new()), Arc::new(Vec::new()), Arc::new(std::collections::HashMap::new()), None, None)
        }
    };

    // Provide not-ready state if snapshot failed to load
    let hash_hex = read_tail_hash_hex(&snapshot_path);
    let init = SnapshotState { path: snapshot_path.clone(), snapshot, neighbors, neighbors_rev, globals, macro_lookup, loaded_at_unix: now_unix(), snapshot_hash_hex: hash_hex, route_cache: navpath_service::new_route_cache(), fairy_rings, node_to_fairy_ring, comp_graph, canonical_grid, profile_cache: navpath_service::new_profile_cache() };
    let state = AppState {
        current: Arc::new(ArcSwap::from_pointee(init)),
        search_permits: navpath_service::default_search_permits(),
        metrics: Arc::new(navpath_service::Metrics::default()),
        ctx_pool: navpath_service::ContextPool::new(),
    };

    let app = build_router(state.clone());
    let addr: SocketAddr = format!("{}:{}", host, port).parse().unwrap();
    if let Ok(dump) = std::env::var("NAVPATH_DUMP_RESULT") {
        if !dump.trim().is_empty() {
            info!(path = %dump, "result dumping enabled; each /route response overwrites this file");
        }
    }
    info!(%addr, path=?snapshot_path, "starting navpath-service");
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

// no duplicate main
