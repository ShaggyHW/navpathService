use std::{num::NonZeroUsize, path::PathBuf, sync::{atomic::{AtomicU64, Ordering}, Arc, Mutex, OnceLock}, time::{SystemTime, UNIX_EPOCH}};

use arc_swap::ArcSwap;
use axum::{routing::{get, post}, Router};
use navpath_core::engine::search::SearchContext;
use navpath_core::{SearchResult, SearchStatus, Snapshot, NeighborProvider};
/// FxHash maps for the id-keyed lookup tables probed on payload/search setup paths
/// (`macro_lookup` alone is probed once per path window): u32/u64 keys, non-adversarial,
/// so SipHash buys nothing here.
pub use rustc_hash::FxHashMap;

use crate::engine_adapter::{GlobalTeleport, FairyRing, MacroLookup, SearchContexts};

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
    /// Shared (`Arc`) with the request's [`ProfileKey`]: one allocation per request,
    /// every further key copy is a reference-count bump.
    pub mask_bits: Arc<[u64]>,
    pub quick_tele: bool,
    pub seed: Option<u64>,
}

/// Pack an eligibility mask's satisfied bits into the cache key's lossless form.
pub fn pack_mask_bits(satisfied: &[bool]) -> Arc<[u64]> {
    satisfied
        .chunks(64)
        .map(|chunk| chunk.iter().enumerate().fold(0u64, |w, (i, &b)| w | (u64::from(b) << i)))
        .collect()
}

/// Cached search outcome: the raw result, the winning virtual-entry teleport, and
/// whether the result was served from an unseeded retry of a seeded request (the
/// `degraded: "seed_dropped"` marker must survive cache hits). Response payloads
/// (actions/geometry) are rebuilt per request so one entry serves every options
/// combination. The result is shared (T3.6): the route cache, the sub-path cache and
/// every hit hold the same allocation — nothing is deep-copied on insert or on a hit.
#[derive(Clone)]
pub struct RouteCacheEntry {
    pub res: Arc<SearchResult>,
    pub virtual_entry: Option<u32>,
    pub seed_dropped: bool,
}
pub type RouteCache = Mutex<lru::LruCache<RouteCacheKey, RouteCacheEntry>>;

fn route_cache_entries() -> usize {
    std::env::var("NAVPATH_ROUTE_CACHE").ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(2048)
}

/// Route cache sized from `NAVPATH_ROUTE_CACHE` (entries; default 2048, 0 disables).
pub fn new_route_cache() -> Option<Arc<RouteCache>> {
    NonZeroUsize::new(route_cache_entries()).map(|cap| Arc::new(Mutex::new(lru::LruCache::new(cap))))
}

/// Cache seed policy (`NAVPATH_CACHE_IGNORE_SEED`, **default ON since 2026-08-06** —
/// plan v3 §3a): drop the seed from the route-cache key, so repeat traffic with
/// varying seeds — the dominant production shape, which otherwise never hits — is
/// served the cached path. Cached hits lose per-seed tie variety (jitter is
/// < 0.1 ms/edge against 300 ms edges, so only equal-cost tie selection changes —
/// the same trade the budget retry already makes). Measured on the gate that
/// roadmap 5.2 demanded (2026-07-31): 11 of 12 repeat requests became hits,
/// ~118 ms → ~0.3–0.9 ms. Set `NAVPATH_CACHE_IGNORE_SEED=0` to restore the legacy
/// per-seed keying.
pub fn cache_ignore_seed() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(std::env::var("NAVPATH_CACHE_IGNORE_SEED").ok().as_deref().map(str::trim), Some("0") | Some("false"))
    })
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
/// attribution it reports matches what the real cache could have held. None under the
/// default seed-blind policy (T3.12): route-cache keys then carry no seed, so a
/// seed-caused miss cannot happen and the index would only be maintained, never read.
pub fn new_seed_shadow() -> Option<Arc<SeedShadow>> {
    if cache_ignore_seed() {
        return None;
    }
    NonZeroUsize::new(route_cache_entries()).map(|cap| Arc::new(Mutex::new(lru::LruCache::new(cap))))
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
pub type ProfileKey = (Arc<[u64]>, bool);

/// One cached optimal path with a node -> position index, for exact sub-path reuse.
/// `res` is the same allocation the route cache holds (T3.6).
pub struct PathRecord {
    pub res: Arc<SearchResult>,
    pub pos: FxHashMap<u32, u32>,
}

impl PathRecord {
    /// Index a fresh, proven-optimal, on-graph-start result for sub-path reuse (virtual
    /// starts are excluded by the caller: their `path[0]` is a teleport landing, not a
    /// requestable start). None when the result does not qualify. Built in the search's
    /// blocking task, so the reactor only links the finished record into the cache.
    pub fn new(res: Arc<SearchResult>) -> Option<Arc<PathRecord>> {
        if !(res.found && res.status == SearchStatus::Found)
            || res.path.len() < 2
            || res.path_g.len() != res.path.len()
        {
            return None;
        }
        let mut pos = FxHashMap::with_capacity_and_hasher(res.path.len(), Default::default());
        for (i, &n) in res.path.iter().enumerate() {
            pos.entry(n).or_insert(i as u32);
        }
        Some(Arc::new(PathRecord { res, pos }))
    }
}

/// A sub-path served from a [`PathRecord`]: positions `ps..=pg` of its path. Nothing is
/// copied — the response serializes the slice straight out of the shared record.
pub struct SubpathHit {
    pub rec: Arc<PathRecord>,
    pub ps: usize,
    pub pg: usize,
}

impl SubpathHit {
    pub fn path(&self) -> &[u32] {
        &self.rec.res.path[self.ps..=self.pg]
    }

    /// Exact cost of the slice: a sub-path of a shortest path is a shortest path, and
    /// `path_g` holds the cumulative costs along it.
    pub fn cost(&self) -> f32 {
        self.rec.res.path_g[self.pg] - self.rec.res.path_g[self.ps]
    }

    /// Cumulative costs along the slice, rebased to start at 0 (diagnostics/tests; the
    /// response never carries `path_g`).
    pub fn path_g(&self) -> Vec<f32> {
        let g0 = self.rec.res.path_g[self.ps];
        self.rec.res.path_g[self.ps..=self.pg].iter().map(|g| g - g0).collect()
    }
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

/// `NAVPATH_SUBPATH_CACHE` (default 64), read once (T3.6; it was re-read per insert).
pub fn subpath_cache_paths() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("NAVPATH_SUBPATH_CACHE").ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(64)
    })
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
/// Only the position probes run under the lock; the caller reads the slice afterwards.
pub fn subpath_lookup(cache: &SubpathCache, key: &ProfileKey, sid: u32, gid: u32) -> (Option<SubpathHit>, bool) {
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
        return (Some(SubpathHit { rec: rec.clone(), ps: ps as usize, pg: pg as usize }), true);
    }
    (None, goal_known)
}

/// Remember an indexed optimal path (see [`PathRecord::new`]) as the profile's newest.
pub fn subpath_insert(cache: &SubpathCache, key: ProfileKey, rec: Arc<PathRecord>) {
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
    /// Macro edges by (src, dst) plus each edge's requirement list and metadata, decoded
    /// once at load (the payload builder no longer parses metadata per request).
    pub macro_lookup: Arc<MacroLookup>,
    pub loaded_at_unix: u64,
    pub snapshot_hash_hex: Option<String>,
    /// Per-snapshot route result cache (None = disabled). Dropped on snapshot swap.
    pub route_cache: Option<Arc<RouteCache>>,
    /// Seed-blind miss attribution for [`route_cache`](Self::route_cache); see
    /// [`SeedShadow`]. None whenever the route cache is disabled or seed-blind.
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
    /// What [`warm_snapshot`] pinned for this mapping ([`WarmState`] as u8), read by the
    /// keep-warm loop.
    pub warm_state: Arc<std::sync::atomic::AtomicU8>,
}

impl SnapshotState {
    pub fn set_warm_state(&self, w: WarmState) {
        self.warm_state.store(w as u8, Ordering::Relaxed);
    }

    pub fn warm_state(&self) -> WarmState {
        match self.warm_state.load(Ordering::Relaxed) {
            1 => WarmState::LockedHead,
            2 => WarmState::LockedAll,
            _ => WarmState::Unlocked,
        }
    }

    /// Everything derived from one opened snapshot, with fresh (empty) caches. The
    /// independent load-time builders run concurrently (T3.15): the canonical grid (plus
    /// JPS tables, the slowest) and the fairy rings each on a scoped thread while this
    /// thread parses the macro metadata; only the component graph waits for both.
    pub fn build(path: PathBuf, snapshot: Snapshot, snapshot_hash_hex: Option<String>) -> SnapshotState {
        let snap = &snapshot;
        let ((neighbors, neighbors_rev, globals, macro_lookup), (fairy_rings, node_to_fairy_ring), canonical_grid) =
            std::thread::scope(|s| {
                let canonical = std::thread::Builder::new()
                    .name("navpath-load-canon".into())
                    .spawn_scoped(s, || engine_adapter::build_canonical_grid(snap))
                    .expect("spawn canonical-grid builder");
                let fairy = std::thread::Builder::new()
                    .name("navpath-load-fairy".into())
                    .spawn_scoped(s, || engine_adapter::build_fairy_rings(snap))
                    .expect("spawn fairy-ring builder");
                let provider = engine_adapter::build_neighbor_provider(snap);
                (
                    provider,
                    fairy.join().expect("fairy-ring builder panicked"),
                    canonical.join().expect("canonical-grid builder panicked"),
                )
            });
        let comp_graph = engine_adapter::build_component_graph(snap, &globals, &fairy_rings, &macro_lookup);
        SnapshotState {
            path,
            snapshot: Some(Arc::new(snapshot)),
            neighbors: Some(Arc::new(neighbors)),
            neighbors_rev: Some(Arc::new(neighbors_rev)),
            globals: Arc::new(globals),
            macro_lookup: Arc::new(macro_lookup),
            loaded_at_unix: now_unix(),
            snapshot_hash_hex,
            route_cache: new_route_cache(),
            seed_shadow: new_seed_shadow(),
            fairy_rings: Arc::new(fairy_rings),
            node_to_fairy_ring: Arc::new(node_to_fairy_ring),
            comp_graph: Some(Arc::new(comp_graph)),
            canonical_grid,
            profile_cache: new_profile_cache(),
            subpath_cache: new_subpath_cache(),
            warm_state: Arc::new(std::sync::atomic::AtomicU8::new(WarmState::Unlocked as u8)),
        }
    }

    /// Not-ready state for a snapshot that failed to open: `/route` answers 503.
    pub fn unloaded(path: PathBuf, snapshot_hash_hex: Option<String>) -> SnapshotState {
        SnapshotState {
            path,
            snapshot: None,
            neighbors: None,
            neighbors_rev: None,
            globals: Arc::new(Vec::new()),
            macro_lookup: Arc::new(engine_adapter::MacroLookup::default()),
            loaded_at_unix: now_unix(),
            snapshot_hash_hex,
            route_cache: new_route_cache(),
            seed_shadow: new_seed_shadow(),
            fairy_rings: Arc::new(Vec::new()),
            node_to_fairy_ring: Arc::new(FxHashMap::default()),
            comp_graph: None,
            canonical_grid: None,
            profile_cache: new_profile_cache(),
            subpath_cache: new_subpath_cache(),
            warm_state: Arc::new(std::sync::atomic::AtomicU8::new(WarmState::Unlocked as u8)),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub current: Arc<ArcSwap<SnapshotState>>, // atomic swap
    /// Bounds concurrent searches (and therefore live node-sized SearchContexts and
    /// blocking-pool threads). Sized from NAVPATH_MAX_CONCURRENT_SEARCHES, default =
    /// available parallelism.
    pub search_permits: Arc<SearchPermits>,
    /// Process-lifetime counters/histograms; relaxed atomics, never on the search loop.
    pub metrics: Arc<Metrics>,
    /// Bounded checkout pool for per-search contexts (see [`ContextPool`]).
    pub ctx_pool: Arc<ContextPool>,
    /// False until the startup warm-up (snapshot populate + context pre-warm) has run;
    /// `/route` answers 503 and `/health` reports `ready: false` meanwhile, so an
    /// orchestrator never routes traffic at a cold mapping.
    pub ready: Arc<std::sync::atomic::AtomicBool>,
}

/// Page the snapshot in (default on; `NAVPATH_MMAP_POPULATE=0` disables) and optionally
/// `mlock` it (`NAVPATH_MLOCK=1`). Used at startup and before every `/admin/reload`
/// swap, so no request ever runs against a cold mapping. Returns whether the mapping
/// (or at least its non-ALT head) is locked, in which case keep-warm is unnecessary
/// for the locked part.
///
/// When the whole mapping cannot be locked (`RLIMIT_MEMLOCK` below the snapshot size —
/// 8 MB on a default desktop), the ~45 MB of per-pop hot sections (coords, walk CSR,
/// component ids, metadata) are locked instead when the limit allows: those are touched
/// on every pop, and after 18 h of uptime the live service had only 0.8 MB of them
/// resident. Raise the limit (systemd `LimitMEMLOCK=`, or `CAP_IPC_LOCK`) to lock all.
pub fn warm_snapshot(snapshot: &Snapshot) -> WarmState {
    let populate = !matches!(std::env::var("NAVPATH_MMAP_POPULATE").ok().as_deref().map(str::trim), Some("0") | Some("false"));
    let mlock = matches!(std::env::var("NAVPATH_MLOCK").ok().as_deref().map(str::trim), Some("1") | Some("true"));
    let mut locked = WarmState::Unlocked;
    if mlock {
        // mlock populates too, so a successful full lock makes the populate pass moot.
        match snapshot.lock_memory() {
            Ok(()) => {
                tracing::info!("mlock'd the whole snapshot mapping");
                return WarmState::LockedAll;
            }
            Err(e) => match snapshot.lock_head() {
                Ok(()) => {
                    tracing::warn!(error = %e, "mlock of the whole snapshot failed (raise RLIMIT_MEMLOCK); locked the non-ALT head only");
                    locked = WarmState::LockedHead;
                }
                Err(e2) => tracing::warn!(error = %e, head_error = %e2, "mlock failed (raise RLIMIT_MEMLOCK); continuing without it"),
            },
        }
    }
    if populate {
        let t = std::time::Instant::now();
        let bytes = if locked == WarmState::LockedHead { snapshot.populate_alt() } else { snapshot.populate() };
        tracing::info!(mib = bytes / (1024 * 1024), elapsed_ms = t.elapsed().as_millis() as u64, "populated snapshot mapping");
    }
    locked
}

/// What [`warm_snapshot`] managed to pin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WarmState {
    Unlocked = 0,
    LockedHead = 1,
    LockedAll = 2,
}

/// `NAVPATH_MMAP_POPULATE=0` (a deliberately cold mapping, e.g. for cold-cache
/// studies) also disables the keep-warm loop.
pub fn keep_warm_disabled_by_populate() -> bool {
    matches!(std::env::var("NAVPATH_MMAP_POPULATE").ok().as_deref().map(str::trim), Some("0") | Some("false"))
}

/// Keep-warm interval (`NAVPATH_KEEP_WARM_S`, default 60 s; 0 disables).
pub fn keep_warm_interval() -> Option<std::time::Duration> {
    let s = std::env::var("NAVPATH_KEEP_WARM_S").ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(60);
    (s > 0).then(|| std::time::Duration::from_secs(s))
}

/// Background keep-warm loop for mappings that could not be (fully) locked: the
/// startup populate only protects the first minutes on a host under memory pressure
/// (measured: 18 h in, 58 MB of the 330 MB mapping was still resident and cold rows cost
/// 50-90 µs per pop). Every interval it re-populates whatever is not locked with
/// MADV_POPULATE_READ — page-table walks when resident (milliseconds), disk reads for
/// pages the kernel evicted, off the request path. Follows `/admin/reload` swaps.
pub fn spawn_keep_warm(current: Arc<ArcSwap<SnapshotState>>, interval: std::time::Duration) {
    let _ = std::thread::Builder::new().name("navpath-keep-warm".into()).spawn(move || loop {
        std::thread::sleep(interval);
        let cur = current.load_full();
        let Some(snap) = cur.snapshot.as_ref() else { continue };
        let t = std::time::Instant::now();
        let bytes = match cur.warm_state() {
            WarmState::LockedAll => continue,
            WarmState::LockedHead => snap.populate_alt(),
            WarmState::Unlocked => snap.populate(),
        };
        tracing::debug!(mib = bytes / (1024 * 1024), elapsed_ms = t.elapsed().as_millis() as u64, "keep-warm pass");
    });
}

/// Checkout pool of node-sized search contexts, pooled ONE context at a time (T3.4).
///
/// Replaces blocking-pool `thread_local!` contexts: tokio's blocking pool grows to 512
/// threads and reaps idle ones after ~10 s, so thread-locals both pinned multi-MB state
/// on arbitrary threads (worst case threads x 2 x nodes x 16 B) and re-paid the
/// allocation on every fresh thread at low QPS — a recurring p99 spike. The search
/// semaphore bounds concurrent checkouts, so the pool never holds more contexts than
/// the concurrency limit can use at once; contexts survive snapshot swaps via
/// `SearchContext::reset`.
///
/// A lease ([`PooledContexts`]) takes a context from the pool only when the search
/// asks for it: unidirectional, JPS and virtual-start searches use one, bidirectional
/// searches two. Pooling pairs used to pin the idle half of every unidirectional
/// checkout (~19 MB each at 1.1M nodes) and drained the pre-warmed pool twice as fast.
pub struct ContextPool {
    stack: Mutex<Vec<SearchContext>>,
    /// Contexts handed out that the pool could not supply (allocated, and page-faulted,
    /// inside a request). Reported by `/stats`; a rising count means the pre-warm is
    /// smaller than the concurrent demand.
    fresh: AtomicU64,
}

impl ContextPool {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Arc<Self> {
        Arc::new(ContextPool { stack: Mutex::new(Vec::new()), fresh: AtomicU64::new(0) })
    }

    /// Pre-allocate and page in `contexts` search contexts for `nodes` nodes so no
    /// request pays the ~19 MB first-touch fault storm per context
    /// (docs/route_latency_improvements_2026-09-17.md §2.2). A bidirectional search
    /// uses two, anything else one.
    pub fn prewarm(&self, contexts: usize, nodes: usize) {
        let mut warmed = Vec::with_capacity(contexts);
        for _ in 0..contexts {
            let mut ctx = SearchContext::new(nodes);
            ctx.prefault();
            warmed.push(ctx);
        }
        if let Ok(mut s) = self.stack.lock() {
            s.extend(warmed);
        }
    }

    /// A lease that checks contexts out on first use and returns them to the pool when
    /// dropped.
    pub fn checkout(self: &Arc<Self>) -> PooledContexts {
        PooledContexts { pool: self.clone(), fwd: None, bwd: None }
    }

    /// Contexts currently idle in the pool.
    pub fn idle(&self) -> usize {
        self.stack.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// Contexts allocated inside requests because the pool was empty.
    pub fn fresh_allocations(&self) -> u64 {
        self.fresh.load(Ordering::Relaxed)
    }

    fn take(&self) -> SearchContext {
        if let Some(ctx) = self.stack.lock().ok().and_then(|mut s| s.pop()) {
            return ctx;
        }
        self.fresh.fetch_add(1, Ordering::Relaxed);
        // Sized lazily by the search's `reset(nodes)`.
        SearchContext::new(0)
    }
}

/// A checkout from [`ContextPool`]: holds at most two contexts, each taken on first use.
pub struct PooledContexts {
    pool: Arc<ContextPool>,
    fwd: Option<SearchContext>,
    bwd: Option<SearchContext>,
}

impl SearchContexts for PooledContexts {
    fn one(&mut self) -> &mut SearchContext {
        let pool = &self.pool;
        self.fwd.get_or_insert_with(|| pool.take())
    }

    fn two(&mut self) -> (&mut SearchContext, &mut SearchContext) {
        let pool = &self.pool;
        let fwd = self.fwd.get_or_insert_with(|| pool.take());
        let bwd = self.bwd.get_or_insert_with(|| pool.take());
        (fwd, bwd)
    }
}

impl Drop for PooledContexts {
    fn drop(&mut self) {
        let (fwd, bwd) = (self.fwd.take(), self.bwd.take());
        if fwd.is_none() && bwd.is_none() {
            return;
        }
        if let Ok(mut s) = self.pool.stack.lock() {
            s.extend(fwd);
            s.extend(bwd);
        }
    }
}

/// Search admission: a semaphore plus its size, so hedges can be admitted only with
/// headroom (T3.1).
pub struct SearchPermits {
    sem: Arc<tokio::sync::Semaphore>,
    total: usize,
}

impl SearchPermits {
    pub fn new(total: usize) -> Arc<Self> {
        Arc::new(SearchPermits { sem: Arc::new(tokio::sync::Semaphore::new(total)), total })
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn available(&self) -> usize {
        self.sem.available_permits()
    }

    /// A primary search permit; None = at capacity (the request gets a 503).
    pub fn try_acquire(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.sem.clone().try_acquire_owned().ok()
    }

    /// A permit for a race hedge (the second engine), granted only while more than a
    /// quarter of all permits would remain free. A hedge shares the primaries'
    /// semaphore, so without this reserve every running race held two permits and
    /// pushed later primaries into 503s; with it, hedges only ever use spare capacity.
    pub fn try_acquire_hedge(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let reserve = self.total / 4;
        if self.sem.available_permits() <= reserve {
            return None;
        }
        let permit = self.sem.clone().try_acquire_owned().ok()?;
        // Another request may have taken permits between the check and the acquire.
        if self.sem.available_permits() < reserve {
            return None; // dropping `permit` returns it
        }
        Some(permit)
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
    /// Hedged-race accounting (`NAVPATH_RACE=1`): races actually run (both engines
    /// searching), and which engine won them.
    pub race_runs: AtomicU64,
    pub race_wins_uni: AtomicU64,
    pub race_wins_bidir: AtomicU64,
    /// Race-eligible misses that ran the primary engine alone: the predictive gate
    /// (`NAVPATH_RACE_GATE`) judged the route not worth a second engine ...
    pub race_gated: AtomicU64,
    /// ... the primary finished inside the hedge delay (`NAVPATH_RACE_HEDGE_MS`) ...
    pub race_hedge_skipped: AtomicU64,
    /// ... or no permit could be spared for the hedge (admission reserve, T3.1).
    pub race_hedge_denied: AtomicU64,
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
            "race_gated": c(&self.race_gated),
            "race_hedge_skipped": c(&self.race_hedge_skipped),
            "race_hedge_denied": c(&self.race_hedge_denied),
            "pops_log2": hist(&self.pops_log2),
            "search_ms_log2": hist(&self.search_ms_log2),
            "search_us_log2": hist(&self.search_us_log2),
            "ns_per_pop_log2": hist(&self.ns_per_pop_log2),
        })
    }
}

/// Permits sized from `NAVPATH_MAX_CONCURRENT_SEARCHES` (default: available cores).
pub fn default_search_permits() -> Arc<SearchPermits> {
    let n = std::env::var("NAVPATH_MAX_CONCURRENT_SEARCHES").ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8));
    SearchPermits::new(n)
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
    let mut hex = String::with_capacity(64);
    for b in buf {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    Some(hex)
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

    fn res(path: Vec<u32>, path_g: Vec<f32>) -> SearchResult {
        let cost = *path_g.last().unwrap();
        SearchResult { found: true, status: SearchStatus::Found, path, path_g, cost, pops: 7, pops_f: 7, pops_b: 0 }
    }

    fn insert(cache: &SubpathCache, key: &ProfileKey, r: SearchResult) {
        if let Some(rec) = PathRecord::new(Arc::new(r)) {
            subpath_insert(cache, key.clone(), rec);
        }
    }

    #[test]
    fn subpath_cache_serves_exact_slices_and_reports_goal_known() {
        let cache = new_subpath_cache().expect("enabled by default");
        let key: ProfileKey = (Arc::from([0b101u64]), false);
        insert(&cache, &key, res(vec![10, 11, 12, 13], vec![0.0, 300.0, 600.0, 1024.0]));

        // suffix
        let (hit, known) = subpath_lookup(&cache, &key, 11, 13);
        let hit = hit.expect("suffix hit");
        assert!(known);
        assert_eq!(hit.path(), &[11, 12, 13]);
        assert_eq!(hit.path_g(), vec![0.0, 300.0, 724.0]);
        assert_eq!(hit.cost(), 724.0);
        // prefix and interior
        assert_eq!(subpath_lookup(&cache, &key, 10, 12).0.unwrap().cost(), 600.0);
        assert_eq!(subpath_lookup(&cache, &key, 11, 12).0.unwrap().path(), &[11, 12]);
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
        assert!(subpath_lookup(&cache, &(Arc::from([0b111u64]), false), 11, 13).0.is_none());
        // truncated / not-found results are never inserted
        let mut bad = res(vec![1, 2], vec![0.0, 300.0]);
        bad.status = SearchStatus::BudgetExceeded;
        assert!(PathRecord::new(Arc::new(bad.clone())).is_none());
        insert(&cache, &key, bad);
        assert!(subpath_lookup(&cache, &key, 1, 2).0.is_none());
    }

    #[test]
    fn pack_mask_bits_is_lossless_and_word_aligned() {
        let mut bits = vec![false; 130];
        for i in [0usize, 5, 63, 64, 127, 129] {
            bits[i] = true;
        }
        let packed = pack_mask_bits(&bits);
        assert_eq!(packed.len(), 3);
        assert_eq!(packed[0], 1 | (1 << 5) | (1 << 63));
        assert_eq!(packed[1], 1 | (1 << 63));
        assert_eq!(packed[2], 1 << 1);
        assert!(pack_mask_bits(&[]).is_empty());
    }

    #[test]
    fn pooled_lease_takes_contexts_only_on_demand() {
        let pool = ContextPool::new();
        pool.prewarm(3, 16);
        {
            let mut lease = pool.checkout();
            assert_eq!(pool.idle(), 3, "a lease takes nothing up front");
            let _ = lease.one();
            assert_eq!(pool.idle(), 2, "unidirectional searches take one context");
        }
        assert_eq!(pool.idle(), 3);
        {
            let mut lease = pool.checkout();
            let _ = lease.two();
            assert_eq!(pool.idle(), 1, "bidirectional searches take two");
            let mut other = pool.checkout();
            let _ = other.two();
            assert_eq!(pool.fresh_allocations(), 1, "the fourth context had to be allocated");
        }
        assert_eq!(pool.idle(), 4);
    }
}
