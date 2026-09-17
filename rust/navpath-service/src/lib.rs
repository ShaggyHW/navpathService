use std::{num::NonZeroUsize, path::PathBuf, sync::{atomic::{AtomicU64, Ordering}, Arc, Mutex}, time::{SystemTime, UNIX_EPOCH}};

use arc_swap::ArcSwap;
use axum::{routing::{get, post}, Router};
use navpath_core::engine::search::SearchContext;
use navpath_core::{Snapshot, NeighborProvider};
/// FxHash maps for the id-keyed lookup tables probed on payload/search setup paths
/// (`macro_lookup` alone is probed once per path window): u32/u64 keys, non-adversarial,
/// so SipHash buys nothing here.
pub use rustc_hash::FxHashMap;

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

/// Seed-blind shadow index over the route-cache key space: the same [`RouteCacheKey`]
/// with `seed` cleared, holding no value. Its only job is to ATTRIBUTE misses. When an
/// exact-key lookup misses but the seed-blind key is present, the request's seed — not
/// its endpoints or its profile — is what caused the miss, and the log line / `/stats`
/// say so instead of reporting a bare `cache_hit=false`.
///
/// This is the measurement roadmap 5.2 makes a prerequisite for the
/// `NAVPATH_CACHE_IGNORE_SEED` policy decision: `cache_miss_seed` is exactly the number
/// of requests that policy would convert into hits. Only seeded requests touch it (for
/// an unseeded request the exact key IS the seed-blind key, so a miss is cold by
/// definition), and entries are one key each.
pub type SeedShadow = Mutex<lru::LruCache<RouteCacheKey, ()>>;

/// Shadow index sized like the route cache (same `NAVPATH_ROUTE_CACHE` budget) so the
/// attribution it reports matches what the real cache could have held.
pub fn new_seed_shadow() -> Option<Arc<SeedShadow>> {
    let n = std::env::var("NAVPATH_ROUTE_CACHE").ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(2048);
    NonZeroUsize::new(n).map(|cap| Arc::new(Mutex::new(lru::LruCache::new(cap))))
}

/// Why a request was not served from the route cache. Logged per request as
/// `cache=<str>`, so a zero hit rate points at its own cause.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    /// Served from the route cache; no search ran.
    Hit,
    /// The exact key missed, but the same endpoints + profile are cached under a
    /// different seed. `NAVPATH_CACHE_IGNORE_SEED=1` would have made this a hit.
    MissSeed,
    /// This (endpoints, profile) combination has not been seen (or was evicted).
    MissCold,
    /// `NAVPATH_ROUTE_CACHE=0` — caching is switched off.
    Disabled,
    /// The exact key missed but both endpoints lie on a cached optimal path for the
    /// same profile; the sub-path was served (exact, no search).
    Subpath,
}

impl CacheOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheOutcome::Hit => "hit",
            CacheOutcome::MissSeed => "miss_seed",
            CacheOutcome::MissCold => "miss_cold",
            CacheOutcome::Disabled => "off",
            CacheOutcome::Subpath => "subpath",
        }
    }
}

/// Key for the per-profile artifact cache (roadmap 5.4): the eligibility mask's EXACT
/// packed bits (via [`pack_mask_bits`] — lossless, same rationale as the route-cache
/// key) plus the quick-tele flag. Snapshot identity is implicit: the cache lives in
/// [`SnapshotState`], so a snapshot swap drops it wholesale.
pub type ProfileKey = (Vec<u64>, bool);

/// One cached optimal path with a node -> position index, for exact sub-path reuse.
pub struct PathRecord {
    pub res: navpath_core::SearchResult,
    pub pos: FxHashMap<u32, u32>,
}

/// Exact sub-path reuse for re-plans (docs/route_latency_improvements_2026-09-17.md
/// §2.4). Bots re-request the same goal as they walk; the exact route-cache key misses
/// every time the start moves. A sub-path of a shortest path is a shortest path in the
/// same graph, and the graph is identical for the same (mask bits, quick_tele) profile
/// — including origin-only global teleports: if the cached optimum walks S..S'..G' then
/// d(S',G') <= d(S,S') + d(S',G') <= c + d(dst,G') for any teleport (c, dst), so no
/// teleport from S' beats the sub-path. Per profile the newest N paths are kept
/// (`NAVPATH_SUBPATH_CACHE`, default 64; 0 disables); lookup is N hash probes.
pub type SubpathCache = Mutex<lru::LruCache<ProfileKey, std::collections::VecDeque<Arc<PathRecord>>>>;

pub fn subpath_cache_paths() -> usize {
    std::env::var("NAVPATH_SUBPATH_CACHE").ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(64)
}

pub fn new_subpath_cache() -> Option<Arc<SubpathCache>> {
    if subpath_cache_paths() == 0 {
        return None;
    }
    // `NAVPATH_ROUTE_CACHE=0` means "no caching" (the payload/replay harnesses rely on
    // it for deterministic captures); follow it unless the sub-path size is set explicitly.
    let route_cache_off = std::env::var("NAVPATH_ROUTE_CACHE").ok().and_then(|v| v.trim().parse::<usize>().ok()) == Some(0);
    if route_cache_off && std::env::var("NAVPATH_SUBPATH_CACHE").is_err() {
        return None;
    }
    Some(Arc::new(Mutex::new(lru::LruCache::new(NonZeroUsize::new(32).expect("32 is non-zero")))))
}

/// Returns the served sub-path (if any) and whether ANY cached path for this profile
/// contains the goal — the latter counts how often a near-start variant would pay.
pub fn subpath_lookup(cache: &SubpathCache, key: &ProfileKey, sid: u32, gid: u32) -> (Option<navpath_core::SearchResult>, bool) {
    let mut goal_known = false;
    let Ok(mut c) = cache.lock() else { return (None, false) };
    let Some(paths) = c.get(key) else { return (None, false) };
    for rec in paths.iter() {
        let Some(&pg) = rec.pos.get(&gid) else { continue };
        goal_known = true;
        let Some(&ps) = rec.pos.get(&sid) else { continue };
        if ps > pg {
            continue;
        }
        let (ps, pg) = (ps as usize, pg as usize);
        let g0 = rec.res.path_g[ps];
        let path = rec.res.path[ps..=pg].to_vec();
        let path_g: Vec<f32> = rec.res.path_g[ps..=pg].iter().map(|g| g - g0).collect();
        let cost = rec.res.path_g[pg] - g0;
        return (
            Some(navpath_core::SearchResult {
                found: true,
                status: navpath_core::SearchStatus::Found,
                path,
                path_g,
                cost,
                pops: 0,
                pops_f: 0,
                pops_b: 0,
            }),
            true,
        );
    }
    (None, goal_known)
}

/// Remember a fresh, proven-optimal, on-graph-start result (virtual starts excluded:
/// their `path[0]` is a teleport landing, not a requestable start).
pub fn subpath_insert(cache: &SubpathCache, key: ProfileKey, res: &navpath_core::SearchResult) {
    if !(res.found && res.status == navpath_core::SearchStatus::Found)
        || res.path.len() < 2
        || res.path_g.len() != res.path.len()
    {
        return;
    }
    let mut pos = FxHashMap::with_capacity_and_hasher(res.path.len(), Default::default());
    for (i, &n) in res.path.iter().enumerate() {
        pos.entry(n).or_insert(i as u32);
    }
    let rec = Arc::new(PathRecord { res: res.clone(), pos });
    let cap = subpath_cache_paths();
    if let Ok(mut c) = cache.lock() {
        let paths = c.get_or_insert_mut(key, std::collections::VecDeque::new);
        paths.push_front(rec);
        paths.truncate(cap);
    }
}

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

/// Requirement id -> tag index from the snapshot's 4-word `req_tags` records.
/// Empty when no snapshot is loaded.
pub fn build_req_tag_index(snapshot: Option<&Snapshot>) -> FxHashMap<u32, usize> {
    let mut map = FxHashMap::default();
    if let Some(s) = snapshot {
        let req_words: &[u32] = s.req_tags();
        let mut i = 0usize;
        while i + 3 < req_words.len() {
            map.insert(req_words[i], i / 4);
            i += 4;
        }
    }
    map
}

#[derive(Clone)]
pub struct SnapshotState {
    pub path: PathBuf,
    pub snapshot: Option<Arc<Snapshot>>, // None when not loaded
    pub neighbors: Option<Arc<NeighborProvider>>,
    /// Reversed macro adjacency for bidirectional searches.
    pub neighbors_rev: Option<Arc<NeighborProvider>>,
    pub globals: Arc<Vec<GlobalTeleport>>, // dst, cost, reqs (indices)
    pub macro_lookup: Arc<FxHashMap<(u32, u32), Vec<u32>>>,
    /// Requirement id -> tag index, derived from the snapshot's `req_tags` section once
    /// at load (previously rebuilt on every payload).
    pub req_tag_index: Arc<FxHashMap<u32, usize>>,
    pub loaded_at_unix: u64,
    pub snapshot_hash_hex: Option<String>,
    /// Per-snapshot route result cache (None = disabled). Dropped on snapshot swap.
    pub route_cache: Option<Arc<RouteCache>>,
    /// Seed-blind miss attribution for [`route_cache`](Self::route_cache); see
    /// [`SeedShadow`]. None whenever the route cache is disabled.
    pub seed_shadow: Option<Arc<SeedShadow>>,
    // Fairy Ring data
    pub fairy_rings: Arc<Vec<FairyRing>>,
    pub node_to_fairy_ring: Arc<FxHashMap<u32, usize>>,
    /// Condensed special-edge graph over walk components for the exact reachability
    /// precheck (roadmap 4.1). None when no snapshot is loaded.
    pub comp_graph: Option<Arc<engine_adapter::ComponentGraph>>,
    /// Canonical strict-domination successor grid (Phase E Stage 2a), built once per
    /// snapshot when NAVPATH_CANONICAL != 0. None = full expansion.
    pub canonical_grid: Option<Arc<navpath_core::engine::canonical::CanonicalGrid>>,
    /// Per-profile artifact cache (roadmap 5.4). Dropped on snapshot swap.
    pub profile_cache: Arc<ProfileCache>,
    pub subpath_cache: Option<Arc<SubpathCache>>,
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
    /// False until the startup warm-up (snapshot populate + context pre-warm) has run;
    /// `/route` answers 503 and `/health` reports `ready: false` meanwhile, so an
    /// orchestrator never routes traffic at a cold mapping.
    pub ready: Arc<std::sync::atomic::AtomicBool>,
}

/// Page the snapshot in (default on; `NAVPATH_MMAP_POPULATE=0` disables) and optionally
/// `mlock` it (`NAVPATH_MLOCK=1`). Used at startup and before every `/admin/reload`
/// swap, so no request ever runs against a cold mapping.
pub fn warm_snapshot(snapshot: &Snapshot) {
    let populate = !matches!(std::env::var("NAVPATH_MMAP_POPULATE").ok().as_deref().map(str::trim), Some("0") | Some("false"));
    if populate {
        let t = std::time::Instant::now();
        let bytes = snapshot.populate();
        tracing::info!(mib = bytes / (1024 * 1024), elapsed_ms = t.elapsed().as_millis() as u64, "populated snapshot mapping");
    }
    if matches!(std::env::var("NAVPATH_MLOCK").ok().as_deref().map(str::trim), Some("1") | Some("true")) {
        match snapshot.lock_memory() {
            Ok(()) => tracing::info!("mlock'd snapshot mapping"),
            Err(e) => tracing::warn!(error = %e, "mlock failed (raise RLIMIT_MEMLOCK); continuing without it"),
        }
    }
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
    /// Pre-allocate and page in `pairs` context pairs for `nodes` nodes so no request
    /// pays the ~36 MB first-touch fault storm (docs/route_latency_improvements_2026-09-17.md
    /// §2.2). Sized to the search permits (x2 with the engine race on) at startup.
    pub fn prewarm(&self, pairs: usize, nodes: usize) {
        let mut warmed = Vec::with_capacity(pairs);
        for _ in 0..pairs {
            let mut pair = Box::new((SearchContext::new(nodes), SearchContext::new(nodes)));
            pair.0.prefault();
            pair.1.prefault();
            warmed.push(pair);
        }
        if let Ok(mut s) = self.stack.lock() {
            s.extend(warmed);
        }
    }

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
    /// Misses caused by the request's seed alone: the same endpoints + profile were
    /// cached under a different seed. This is exactly how many requests
    /// `NAVPATH_CACHE_IGNORE_SEED=1` would convert into hits (roadmap 5.2).
    pub cache_miss_seed: AtomicU64,
    /// Misses on an (endpoints, profile) combination not currently cached — the
    /// irreducible kind. A client that never repeats a start/goal pair sees only these.
    pub cache_miss_cold: AtomicU64,
    /// Sub-path cache (see [`SubpathCache`]): hits served, and misses whose goal was on
    /// some cached path for the profile but whose start was not (the near-start signal).
    pub cache_subpath_hits: AtomicU64,
    pub cache_miss_goal_known: AtomicU64,
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
    /// Hedged-race accounting (`NAVPATH_RACE=1`): races started, and which engine won.
    pub race_runs: AtomicU64,
    pub race_wins_uni: AtomicU64,
    pub race_wins_bidir: AtomicU64,
    /// log2 histogram of heap pops per fresh search (bucket i>0 covers [2^(i-1), 2^i)).
    pub pops_log2: [AtomicU64; 26],
    /// log2 histogram of search wall time in ms (same bucket scheme).
    pub search_ms_log2: [AtomicU64; 18],
    /// Search wall time in microseconds, log2 buckets (the ms histogram above puts most
    /// routes in bucket 0; this one resolves them).
    pub search_us_log2: [AtomicU64; 28],
    /// Nanoseconds per heap pop, log2 buckets: the memory-behaviour signal. Warm searches
    /// sit at 128-256 ns; a cold page cache shows up as 32-128 µs.
    pub ns_per_pop_log2: [AtomicU64; 22],
}

impl Metrics {
    fn log2_bucket(v: u64, len: usize) -> usize {
        if v == 0 { 0 } else { ((64 - v.leading_zeros()) as usize).min(len - 1) }
    }

    pub fn record_pops(&self, pops: u64) {
        let i = Self::log2_bucket(pops, self.pops_log2.len());
        self.pops_log2[i].fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_search_us(&self, us: u64, pops: u64) {
        let i = Self::log2_bucket(us, self.search_us_log2.len());
        self.search_us_log2[i].fetch_add(1, Ordering::Relaxed);
        if pops > 0 {
            let ns = us.saturating_mul(1000) / pops;
            let j = Self::log2_bucket(ns, self.ns_per_pop_log2.len());
            self.ns_per_pop_log2[j].fetch_add(1, Ordering::Relaxed);
        }
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
            "cache_miss_seed": c(&self.cache_miss_seed),
            "cache_miss_cold": c(&self.cache_miss_cold),
            "cache_subpath_hits": c(&self.cache_subpath_hits),
            "cache_miss_goal_known": c(&self.cache_miss_goal_known),
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
            "race_runs": c(&self.race_runs),
            "race_wins_uni": c(&self.race_wins_uni),
            "race_wins_bidir": c(&self.race_wins_bidir),
            "pops_log2": hist(&self.pops_log2),
            "search_ms_log2": hist(&self.search_ms_log2),
            "search_us_log2": hist(&self.search_us_log2),
            "ns_per_pop_log2": hist(&self.ns_per_pop_log2),
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
        .route("/reachable", get(routes::reachable))
        .route("/admin/reload", post(routes::reload))
        .route("/stats", get(routes::stats))
        .with_state(state)
}

#[cfg(test)]
mod subpath_tests {
    use super::*;
    use navpath_core::{SearchResult, SearchStatus};

    fn res(path: Vec<u32>, path_g: Vec<f32>) -> SearchResult {
        let cost = *path_g.last().unwrap();
        SearchResult { found: true, status: SearchStatus::Found, path, path_g, cost, pops: 7, pops_f: 7, pops_b: 0 }
    }

    #[test]
    fn subpath_cache_serves_exact_slices_and_reports_goal_known() {
        let cache = new_subpath_cache().expect("enabled by default");
        let key: ProfileKey = (vec![0b101], false);
        subpath_insert(&cache, key.clone(), &res(vec![10, 11, 12, 13], vec![0.0, 300.0, 600.0, 1024.0]));

        // suffix
        let (hit, known) = subpath_lookup(&cache, &key, 11, 13);
        let hit = hit.expect("suffix hit");
        assert!(known);
        assert_eq!(hit.path, vec![11, 12, 13]);
        assert_eq!(hit.path_g, vec![0.0, 300.0, 724.0]);
        assert_eq!(hit.cost, 724.0);
        assert_eq!(hit.pops, 0);
        // prefix and interior
        assert_eq!(subpath_lookup(&cache, &key, 10, 12).0.unwrap().cost, 600.0);
        assert_eq!(subpath_lookup(&cache, &key, 11, 12).0.unwrap().path, vec![11, 12]);
        // wrong direction: goal known, no hit
        let (hit, known) = subpath_lookup(&cache, &key, 13, 11);
        assert!(hit.is_none() && known);
        // start off-path: goal known (the near-start signal), no hit
        let (hit, known) = subpath_lookup(&cache, &key, 99, 12);
        assert!(hit.is_none() && known);
        // goal off-path
        let (hit, known) = subpath_lookup(&cache, &key, 10, 99);
        assert!(hit.is_none() && !known);
        // other profile sees nothing
        assert!(subpath_lookup(&cache, &(vec![0b111], false), 11, 13).0.is_none());
        // truncated / not-found results are never inserted
        let mut bad = res(vec![1, 2], vec![0.0, 300.0]);
        bad.status = SearchStatus::BudgetExceeded;
        subpath_insert(&cache, key.clone(), &bad);
        assert!(subpath_lookup(&cache, &key, 1, 2).0.is_none());
    }
}
