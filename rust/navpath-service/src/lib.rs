use std::{collections::HashMap, path::PathBuf, sync::Arc, time::{SystemTime, UNIX_EPOCH}};

use arc_swap::ArcSwap;
use axum::{routing::{get, post}, Router};
use navpath_core::{Snapshot, NeighborProvider};

use crate::engine_adapter::{GlobalTeleport, FairyRing};

pub mod routes;
pub mod engine_adapter;

#[derive(Clone)]
pub struct SnapshotState {
    pub path: PathBuf,
    pub snapshot: Option<Arc<Snapshot>>, // None when not loaded
    pub neighbors: Option<Arc<NeighborProvider>>,
    pub globals: Arc<Vec<GlobalTeleport>>, // dst, cost, reqs (indices)
    pub macro_lookup: Arc<HashMap<(u32, u32), Vec<u32>>>,
    pub loaded_at_unix: u64,
    pub snapshot_hash_hex: Option<String>,
    // Fairy Ring data
    pub fairy_rings: Arc<Vec<FairyRing>>,
    pub node_to_fairy_ring: Arc<HashMap<u32, usize>>,
}

#[derive(Clone)]
pub struct AppState {
    pub current: Arc<ArcSwap<SnapshotState>>, // atomic swap
    /// Bounds concurrent searches (and therefore live node-sized SearchContexts and
    /// blocking-pool threads). Sized from NAVPATH_MAX_CONCURRENT_SEARCHES, default =
    /// available parallelism.
    pub search_permits: Arc<tokio::sync::Semaphore>,
}

/// Semaphore sized from `NAVPATH_MAX_CONCURRENT_SEARCHES` (default: available cores).
pub fn default_search_permits() -> Arc<tokio::sync::Semaphore> {
    let n = std::env::var("NAVPATH_MAX_CONCURRENT_SEARCHES").ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8));
    Arc::new(tokio::sync::Semaphore::new(n))
}

pub fn env_var(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

pub fn read_tail_hash_hex(path: &PathBuf) -> Option<String> {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len < 32 { return None; }
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(32))) .ok()?;
    let mut buf = [0u8; 32];
    let _ = f.read_exact(&mut buf).ok()?;
    Some(buf.iter().map(|b| format!("{:02x}", b)).collect())
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(routes::health))
        .route("/healthz", get(routes::health))
        .route("/route", post(routes::route))
        .route("/tile/exists", get(routes::tile_exists))
        .route("/admin/reload", post(routes::reload))
        .with_state(state)
}
