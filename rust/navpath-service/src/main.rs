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
    build_coord_index,
};

#[tokio::main]
async fn main() -> Result<()> {
    let subscriber = FmtSubscriber::builder().with_ansi(false).finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    let host = env_var("NAVPATH_HOST", "127.0.0.1");
    let port: u16 = env_var("NAVPATH_PORT", "8080").parse().unwrap_or(8080);
    let snapshot_path = PathBuf::from(env_var("SNAPSHOT_PATH", "./graph.snapshot"));

    let (snapshot, neighbors, globals, macro_lookup, fairy_rings, node_to_fairy_ring) = match Snapshot::open(&snapshot_path) {
        Ok(s) => {
             let (n, g, m) = navpath_service::engine_adapter::build_neighbor_provider(&s);
             let (fr, nfr) = navpath_service::engine_adapter::build_fairy_rings(&s);
             (Some(Arc::new(s)), Some(Arc::new(n)), Arc::new(g), Arc::new(m), Arc::new(fr), Arc::new(nfr))
        },
        Err(e) => {
            error!(error=?e, path=?snapshot_path, "failed to open snapshot; service will still start but /route will 503");
            (None, None, Arc::new(Vec::new()), Arc::new(std::collections::HashMap::<(u32, u32), Vec<u32>>::new()), Arc::new(Vec::new()), Arc::new(std::collections::HashMap::new()))
        }
    };

    // Provide not-ready state if snapshot failed to load
    let hash_hex = read_tail_hash_hex(&snapshot_path);
    let coord_index = snapshot.as_ref().map(|s| Arc::new(build_coord_index(s)));
    let init = SnapshotState { path: snapshot_path.clone(), snapshot, neighbors, globals, macro_lookup, loaded_at_unix: now_unix(), snapshot_hash_hex: hash_hex, coord_index, fairy_rings, node_to_fairy_ring };
    let state = AppState { current: Arc::new(ArcSwap::from_pointee(init)) };

    let app = build_router(state.clone());
    let addr: SocketAddr = format!("{}:{}", host, port).parse().unwrap();
    info!(%addr, path=?snapshot_path, "starting navpath-service");
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

// no duplicate main
