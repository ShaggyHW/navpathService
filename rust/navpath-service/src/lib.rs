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
    pub coord_index: Option<Arc<HashMap<(i32,i32,i32), u32>>>,
    // Fairy Ring data
    pub fairy_rings: Arc<Vec<FairyRing>>,
    pub node_to_fairy_ring: Arc<HashMap<u32, usize>>,
}

#[derive(Clone)]
pub struct AppState {
    pub current: Arc<ArcSwap<SnapshotState>>, // atomic swap
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

pub fn build_coord_index(s: &Snapshot) -> HashMap<(i32, i32, i32), u32> {
    let n = s.counts().nodes as usize;
    let mut map = HashMap::with_capacity(n);
    let xs = s.nodes_x();
    let ys = s.nodes_y();
    let ps = s.nodes_plane();
    for i in 0..n {
        let x = xs.get(i).expect("nodes_x missing");
        let y = ys.get(i).expect("nodes_y missing");
        let p = ps.get(i).expect("nodes_plane missing");
        map.insert((x, y, p), i as u32);
    }
    map
}
