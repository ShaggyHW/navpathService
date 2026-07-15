use std::{collections::HashMap, num::NonZeroUsize, path::PathBuf, sync::{atomic::{AtomicU64, Ordering}, Arc, Mutex}, time::{SystemTime, UNIX_EPOCH}};

use arc_swap::ArcSwap;
use axum::{routing::{get, post}, Router};
use navpath_core::engine::search::SearchContext;
use navpath_core::{Snapshot, NeighborProvider};

use crate::engine_adapter::{GlobalTeleport, FairyRing};

pub mod routes;
pub mod engine_adapter;

/// Cache key for a route search. Everything the search result depends on is in here;
/// snapshot identity is implicit (the cache lives inside SnapshotState, so a snapshot
/// swap drops it wholesale).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RouteCacheKey {
    pub virtual_start: bool,
    pub sid: u32,
    pub gid: u32,
    /// The eligibility mask's EXACT bits (one bit per requirement tag, packed). A
    /// 64-bit digest here would let two colliding profiles share a slot and serve a
    /// route computed under the wrong eligibility — identity keys must be lossless.
    pub mask_bits: Vec<u64>,
    pub quick_tele: bool,
    pub seed: Option<u64>,
}

/// Pack an eligibility mask's satisfied bits into the cache key's lossless form.
pub fn pack_mask_bits(satisfied: &[bool]) -> Vec<u64> {
    let mut bits = vec![0u64; satisfied.len().div_ceil(64)];
    for (i, &b) in satisfied.iter().enumerate() {
        if b {
            bits[i / 64] |= 1u64 << (i % 64);
        }
    }
    bits
}

/// Cached search outcome: the raw result, the winning virtual-entry teleport, and
/// whether the result was served from an unseeded retry of a seeded request (the
/// `degraded: "seed_dropped"` marker must survive cache hits). Response payloads
/// (actions/geometry) are rebuilt per request so one entry serves every options
/// combination.
pub type RouteCacheEntry = Arc<(navpath_core::SearchResult, Option<u32>, bool)>;
pub type RouteCache = Mutex<lru::LruCache<RouteCacheKey, RouteCacheEntry>>;

/// Route cache sized from `NAVPATH_ROUTE_CACHE` (entries; default 2048, 0 disables).
pub fn new_route_cache() -> Option<Arc<RouteCache>> {
    let n = std::env::var("NAVPATH_ROUTE_CACHE").ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(2048);
    NonZeroUsize::new(n).map(|cap| Arc::new(Mutex::new(lru::LruCache::new(cap))))
}

/// Key for the per-profile artifact cache (roadmap 5.4): the eligibility mask's EXACT
/// packed bits (via [`pack_mask_bits`] — lossless, same rationale as the route-cache
/// key) plus the quick-tele flag. Snapshot identity is implicit: the cache lives in
/// [`SnapshotState`], so a snapshot swap drops it wholesale.
pub type ProfileKey = (Vec<u64>, bool);

/// Per-snapshot LRU of per-profile search artifacts (forward/reversed MacroFilters,
/// eligible globals, eligible fairy sources/dests) — pure functions of
/// (snapshot, mask bits, quick_tele) that were previously rebuilt on every
/// cache-missing request. Touched once per request, hence a plain Mutex.
pub type ProfileCache = Mutex<lru::LruCache<ProfileKey, Arc<engine_adapter::ProfileArtifacts>>>;

/// Fixed 32 entries: production traffic concentrates on a handful of profiles, and one
/// entry is a few KB (two 959-slot filters + the eligible-edge vecs).
pub fn new_profile_cache() -> Arc<ProfileCache> {
    Arc::new(Mutex::new(lru::LruCache::new(
        NonZeroUsize::new(32).expect("32 is non-zero"),
    )))
}

#[derive(Clone)]
pub struct SnapshotState {
    pub path: PathBuf,
    pub snapshot: Option<Arc<Snapshot>>, // None when not loaded
    pub neighbors: Option<Arc<NeighborProvider>>,
    /// Reversed macro adjacency for bidirectional searches.
    pub neighbors_rev: Option<Arc<NeighborProvider>>,
    pub globals: Arc<Vec<GlobalTeleport>>, // dst, cost, reqs (indices)
    pub macro_lookup: Arc<HashMap<(u32, u32), Vec<u32>>>,
    pub loaded_at_unix: u64,
    pub snapshot_hash_hex: Option<String>,
    /// Per-snapshot route result cache (None = disabled). Dropped on snapshot swap.
    pub route_cache: Option<Arc<RouteCache>>,
    // Fairy Ring data
    pub fairy_rings: Arc<Vec<FairyRing>>,
    pub node_to_fairy_ring: Arc<HashMap<u32, usize>>,
    /// Condensed special-edge graph over walk components for the exact reachability
    /// precheck (roadmap 4.1). None when no snapshot is loaded.
    pub comp_graph: Option<Arc<engine_adapter::ComponentGraph>>,
    /// Canonical strict-domination successor grid (Phase E Stage 2a), built once per
    /// snapshot when NAVPATH_CANONICAL != 0. None = full expansion.
    pub canonical_grid: Option<Arc<navpath_core::engine::canonical::CanonicalGrid>>,
    /// Per-profile artifact cache (roadmap 5.4). Dropped on snapshot swap.
    pub profile_cache: Arc<ProfileCache>,
}

#[derive(Clone)]
pub struct AppState {
    pub current: Arc<ArcSwap<SnapshotState>>, // atomic swap
    /// Bounds concurrent searches (and therefore live node-sized SearchContexts and
    /// blocking-pool threads). Sized from NAVPATH_MAX_CONCURRENT_SEARCHES, default =
    /// available parallelism.
    pub search_permits: Arc<tokio::sync::Semaphore>,
    /// Process-lifetime counters/histograms; relaxed atomics, never on the search loop.
    pub metrics: Arc<Metrics>,
    /// Bounded checkout pool for per-search context pairs (see [`ContextPool`]).
    pub ctx_pool: Arc<ContextPool>,
}

/// Checkout pool for the node-sized per-search context pair.
///
/// Replaces blocking-pool `thread_local!` contexts: tokio's blocking pool grows to 512
/// threads and reaps idle ones after ~10 s, so thread-locals both pinned multi-MB state
/// on arbitrary threads (worst case threads x 2 x nodes x 16 B) and re-paid the
/// allocation on every fresh thread at low QPS — a recurring p99 spike. The search
/// semaphore bounds concurrent checkouts, so the pool never holds more pairs than the
/// concurrency limit; contexts survive snapshot swaps via `SearchContext::reset`.
pub struct ContextPool {
    stack: Mutex<Vec<Box<(SearchContext, SearchContext)>>>,
}

impl ContextPool {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Arc<Self> {
        Arc::new(ContextPool { stack: Mutex::new(Vec::new()) })
    }

    /// Check out a context pair (fresh and empty if the pool has none spare); it
    /// returns to the pool when the guard drops.
    pub fn checkout(self: &Arc<Self>) -> PooledContexts {
        let pair = self
            .stack
            .lock()
            .ok()
            .and_then(|mut s| s.pop())
            .unwrap_or_else(|| Box::new((SearchContext::new(0), SearchContext::new(0))));
        PooledContexts { pool: self.clone(), pair: Some(pair) }
    }
}

pub struct PooledContexts {
    pool: Arc<ContextPool>,
    pair: Option<Box<(SearchContext, SearchContext)>>,
}

impl PooledContexts {
    pub fn pair(&mut self) -> &mut (SearchContext, SearchContext) {
        self.pair.as_mut().expect("context pair checked out")
    }
}

impl Drop for PooledContexts {
    fn drop(&mut self) {
        if let Some(pair) = self.pair.take() {
            if let Ok(mut s) = self.pool.stack.lock() {
                s.push(pair);
            }
        }
    }
}

/// Service counters. Everything the roadmap's tuning decisions need (cache policy,
/// retry/budget sizing, semaphore sizing, 4M capacity planning) and none of it was
/// observable before: retries were invisible, 503/504 paths emitted no signal, and the
/// route cache shipped without the hit-rate metric the audit required.
#[derive(Default)]
pub struct Metrics {
    pub requests: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_puts: AtomicU64,
    pub searches: AtomicU64,
    pub retries: AtomicU64,
    pub retry_found: AtomicU64,
    pub found: AtomicU64,
    pub not_found: AtomicU64,
    pub budget_exceeded: AtomicU64,
    pub cancelled: AtomicU64,
    pub semaphore_rejects: AtomicU64,
    pub deadline_timeouts: AtomicU64,
    /// Requests answered found=false by the component reachability precheck — each one
    /// is a budget-capped flood that never ran.
    pub precheck_rejects: AtomicU64,
    /// log2 histogram of heap pops per fresh search (bucket i>0 covers [2^(i-1), 2^i)).
    pub pops_log2: [AtomicU64; 26],
    /// log2 histogram of search wall time in ms (same bucket scheme).
    pub search_ms_log2: [AtomicU64; 18],
}

impl Metrics {
    fn log2_bucket(v: u64, len: usize) -> usize {
        if v == 0 { 0 } else { ((64 - v.leading_zeros()) as usize).min(len - 1) }
    }

    pub fn record_pops(&self, pops: u64) {
        let i = Self::log2_bucket(pops, self.pops_log2.len());
        self.pops_log2[i].fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_search_ms(&self, ms: u64) {
        let i = Self::log2_bucket(ms, self.search_ms_log2.len());
        self.search_ms_log2[i].fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot_json(&self) -> serde_json::Value {
        fn hist(buckets: &[AtomicU64]) -> Vec<serde_json::Value> {
            buckets
                .iter()
                .enumerate()
                .filter_map(|(i, c)| {
                    let count = c.load(Ordering::Relaxed);
                    if count == 0 {
                        return None;
                    }
                    let ge: u64 = if i == 0 { 0 } else { 1u64 << (i - 1) };
                    Some(serde_json::json!({"ge": ge, "count": count}))
                })
                .collect()
        }
        let c = |a: &AtomicU64| a.load(Ordering::Relaxed);
        serde_json::json!({
            "requests": c(&self.requests),
            "cache_hits": c(&self.cache_hits),
            "cache_puts": c(&self.cache_puts),
            "searches": c(&self.searches),
            "retries": c(&self.retries),
            "retry_found": c(&self.retry_found),
            "found": c(&self.found),
            "not_found": c(&self.not_found),
            "budget_exceeded": c(&self.budget_exceeded),
            "cancelled": c(&self.cancelled),
            "semaphore_rejects": c(&self.semaphore_rejects),
            "deadline_timeouts": c(&self.deadline_timeouts),
            "precheck_rejects": c(&self.precheck_rejects),
            "pops_log2": hist(&self.pops_log2),
            "search_ms_log2": hist(&self.search_ms_log2),
        })
    }
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
        .route("/stats", get(routes::stats))
        .with_state(state)
}
