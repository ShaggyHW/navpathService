use std::sync::Arc;
use std::sync::OnceLock;
use std::collections::HashMap;

use navpath_core::{EngineView, SearchParams, SearchResult, SearchStatus, Snapshot, NeighborProvider};
use navpath_core::engine::canonical::CanonicalGrid;
use navpath_core::engine::search::{ExtraEdges, SearchContext};
use navpath_core::engine::neighbors::{MacroFilter, WalkGraph};
use navpath_core::engine::search::BidirParams;

/// Empty "no path" result for early-exit paths in this module.
fn not_found_result() -> SearchResult {
    SearchResult { found: false, status: SearchStatus::NotFound, path: Vec::new(), cost: f32::INFINITY, pops: 0, pops_f: 0, pops_b: 0 }
}
use serde_json::Value as JsonValue;
use navpath_core::eligibility::{fnv1a32, EligibilityMask};
use navpath_core::engine::heuristics::{active_landmarks, LandmarkHeuristic};
use tracing::{info, warn};

/// Weak-backward demotion ratio (roadmap 4.2): a route runs bidirectional only when
/// the backward ALT bound at the goal is at least this fraction of the forward bound
/// of C*. With ~125 spread anchors (start + every eligible global) the backward
/// min-aggregates flatten map-wide on permissive profiles; MM then grinds a
/// reverse-Dijkstra ball of cost-radius ~C*/2 where a strong-h forward search runs a
/// corridor. Both engines are exact, so this only chooses which one runs.
/// `NAVPATH_BIDIR_MIN_HB_RATIO`, default 0.5; `0` disables demotion (always bidir).
fn bidir_min_hb_ratio() -> f32 {
    static R: OnceLock<f32> = OnceLock::new();
    *R.get_or_init(|| {
        std::env::var("NAVPATH_BIDIR_MIN_HB_RATIO").ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .unwrap_or(0.5)
    })
}

/// Routes with a forward bound under this many ms skip the demotion analysis: they
/// search fast under either engine, and the analysis itself costs ~125 heuristic
/// evaluations plus one reverse selection.
const BIDIR_POLICY_MIN_H_MS: f32 = 20_000.0;

/// Opt-in plateau tie-break bucket (roadmap 3.4), `NAVPATH_TIEBREAK_BUCKET_MS`
/// (default 0 = off; 128 = 2x the ALT quantum is the recommended operating point).
/// Bounded-suboptimal by construction: served cost <= optimum + bucket. By default it
/// applies to SEEDED searches only — their contract already tolerates jitter-scale
/// cost wiggle of the same order, and seeds are what disable the exact-tie plateau
/// collapse (both production budget incidents). `NAVPATH_TIEBREAK_UNSEEDED=1` extends
/// it to unseeded traffic; gate that on a `replay --bucket-ms=<B>` corpus run.
fn tiebreak_bucket_ms() -> f32 {
    static B: OnceLock<f32> = OnceLock::new();
    *B.get_or_init(|| {
        std::env::var("NAVPATH_TIEBREAK_BUCKET_MS").ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|b| b.is_finite() && *b > 0.0)
            .unwrap_or(0.0)
    })
}

fn tiebreak_unseeded() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        matches!(std::env::var("NAVPATH_TIEBREAK_UNSEEDED").ok().as_deref().map(str::trim), Some("1") | Some("true"))
    })
}

/// Effective tie-break bucket for one search attempt.
fn bucket_for(seed: Option<u64>) -> f32 {
    let b = tiebreak_bucket_ms();
    if b > 0.0 && (seed.is_some() || tiebreak_unseeded()) { b } else { 0.0 }
}

/// Build the canonical pruning grid at snapshot load (roadmap Phase E Stage 2a),
/// unless disabled with NAVPATH_CANONICAL=0. Fail-soft: a snapshot violating a
/// canonical precondition (pre-invariant CSR order, cheap adjacent macro edge)
/// disables pruning with a warning instead of failing the load. Pruning is
/// cost-exact — strictly-dominated successors only — and engages for unseeded
/// searches (including every budget-retry rung) automatically.
pub fn build_canonical_grid(snapshot: &Snapshot) -> Option<Arc<CanonicalGrid>> {
    let enabled = !matches!(
        std::env::var("NAVPATH_CANONICAL").ok().as_deref().map(str::trim),
        Some("0") | Some("false")
    );
    if !enabled {
        return None;
    }
    let t = std::time::Instant::now();
    match CanonicalGrid::build(
        snapshot.counts().nodes as usize,
        snapshot.coords_packed(),
        snapshot.walk_offsets(),
        snapshot.walk_dst(),
        snapshot.macro_src(),
        snapshot.macro_dst(),
        snapshot.macro_w(),
    ) {
        Ok(g) => {
            info!(elapsed_ms = t.elapsed().as_millis() as u64, "built canonical pruning grid");
            Some(Arc::new(g))
        }
        Err(e) => {
            warn!(error = %e, "canonical pruning disabled: snapshot violates a precondition");
            None
        }
    }
}

/// Whether normal routes use the bidirectional search (NAVPATH_BIDIR, default on;
/// set 0 to fall back to unidirectional).
fn bidir_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(std::env::var("NAVPATH_BIDIR").ok().as_deref().map(str::trim), Some("0") | Some("false"))
    })
}

/// Whether per-request requirement diagnostics are enabled, controlled by the
/// `NAVPATH_DEBUG_REQS` env var (`1`/`true`). Cached once so the hot path never
/// performs an env lookup.
fn req_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("NAVPATH_DEBUG_REQS").ok().as_deref().map(str::trim),
            Some("1") | Some("true") | Some("TRUE")
        )
    })
}

/// Parse a pop-budget env override once: `None` = unset (use the scale-aware default),
/// `Some(None)` = explicitly disabled (0), `Some(Some(n))` = absolute cap.
fn budget_env(cache: &'static OnceLock<Option<Option<u32>>>, var: &str) -> Option<Option<u32>> {
    *cache.get_or_init(|| {
        std::env::var(var)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map(|n| if n == 0 { None } else { Some(n) })
    })
}

/// First-attempt pop budget. `NAVPATH_MAX_POPS` overrides absolutely (0 disables);
/// otherwise `max(1.5M, nodes/2)` — scale-aware (roadmap 4.5), a deliberate no-op at
/// today's 1.12M nodes. The fixed 1.5M was calibrated to this map ("hard legit queries
/// measured ~600k pops; floods cap at a few hundred ms"); at 4M the flood logic
/// inverts — a gated-goal flood stops exhausting the heap under budget and starts
/// returning BudgetExceeded, which would make the retry re-flood — so the default must
/// grow with the snapshot. (The component precheck removes those floods outright; this
/// keeps hard REACHABLE routes from a third found=false incident as the map grows.)
fn default_max_pops(nodes: usize) -> Option<u32> {
    static ENV: OnceLock<Option<Option<u32>>> = OnceLock::new();
    match budget_env(&ENV, "NAVPATH_MAX_POPS") {
        Some(v) => v,
        None => Some(1_500_000u32.max((nodes / 2).min(u32::MAX as usize) as u32)),
    }
}

/// Retry-rung pop budget: `NAVPATH_RETRY_MAX_POPS` overrides (0 disables the retry);
/// default 4x the first attempt — at the 4M scale-point (first = N/2) that is 2N,
/// enough for a full bidirectional sweep of both frontiers.
fn retry_max_pops(nodes: usize) -> Option<u32> {
    static ENV: OnceLock<Option<Option<u32>>> = OnceLock::new();
    match budget_env(&ENV, "NAVPATH_RETRY_MAX_POPS") {
        Some(v) => v,
        None => default_max_pops(nodes).map(|p| p.saturating_mul(4)),
    }
}

/// Search result plus retry telemetry, so the service can log/count what actually
/// happened instead of inferring it from a bare [`SearchResult`].
pub struct SearchOutcome {
    pub res: SearchResult,
    /// Whether the budget-exceeded retry ladder ran.
    pub retried: bool,
    /// Heap-pop counts of [first attempt, retry, unseeded fallback] (0 = did not run).
    pub attempts_pops: [u32; 3],
    /// The served result came from an UNSEEDED search of a seeded request (the last
    /// rung of the retry ladder). Surfaced to clients as `degraded: "seed_dropped"` —
    /// previously this contract rewrite was silent.
    pub seed_dropped: bool,
}

/// Budget-retry ladder (roadmap 1.5). A `BudgetExceeded` first attempt earns:
///   1. a retry with the SAME seed at the escalated cap — jitter-inflated pop counts
///      usually fit 4x, and the client's path-variety contract survives;
///   2. only if that also gives up: an unseeded retry (jitter exists to vary
///      otherwise-equal paths; a real route is worth more than that variety), with the
///      served result marked `seed_dropped`.
/// Every rung shares the request deadline/cancel flag, which remains the real ceiling.
fn retry_ladder(
    first: SearchResult,
    seed: Option<u64>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    nodes: usize,
    mut search: impl FnMut(Option<u64>, Option<u32>) -> SearchResult,
) -> SearchOutcome {
    let Some(retry_pops) = budget_retry_pops(&first, seed, cancel, nodes) else {
        return SearchOutcome { attempts_pops: [first.pops, 0, 0], retried: false, seed_dropped: false, res: first };
    };
    warn!(
        pops = first.pops, found = first.found, retry_pops, seeded = seed.is_some(),
        "search exhausted its pop budget; retrying with an escalated budget"
    );
    let second = search(seed, Some(retry_pops));
    let mut attempts_pops = [first.pops, second.pops, 0];
    if seed.is_none() || second.status == SearchStatus::Found {
        return SearchOutcome { attempts_pops, retried: true, seed_dropped: false, res: better_of(first, second) };
    }
    let best2 = better_of(first, second);
    if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
        return SearchOutcome { attempts_pops, retried: true, seed_dropped: false, res: best2 };
    }
    warn!(retry_pops, "seeded retry also exhausted its budget; dropping the seed");
    let third = search(None, Some(retry_pops));
    attempts_pops[2] = third.pops;
    if third.status == SearchStatus::Found || (third.found && (!best2.found || third.cost < best2.cost)) {
        return SearchOutcome { attempts_pops, retried: true, seed_dropped: true, res: third };
    }
    if !best2.found {
        // No rung found a path; the deeper search's verdict (e.g. a heap-exhausting
        // NotFound) is the most truthful one.
        return SearchOutcome { attempts_pops, retried: true, seed_dropped: false, res: better_of(best2, third) };
    }
    SearchOutcome { attempts_pops, retried: true, seed_dropped: false, res: best2 }
}

/// Choose which of (first attempt, budget retry) to serve. A proven result always wins;
/// otherwise a discovered path — even a truncated one — beats no path, and between two
/// truncated paths the cheaper wins. The retry is unseeded and proves optimality on the
/// base graph, so a `Found` retry costs no more than any truncated path (jitter only
/// ever adds to edge weights).
fn better_of(first: SearchResult, retry: SearchResult) -> SearchResult {
    if retry.status == SearchStatus::Found {
        return retry;
    }
    match (first.found, retry.found) {
        (true, true) => if retry.cost < first.cost { retry } else { first },
        (true, false) => first,
        _ => retry,
    }
}

/// Budget for retrying a search that gave up, or `None` to keep the result as-is.
///
/// `BudgetExceeded` means "gave up", NOT "no path": with `found=false` the goal was
/// never reached in time, and with `found=true` the returned path was discovered but
/// not proven optimal — both are answers the caller cannot distinguish from the real
/// thing. The retry drops the request `seed` and
/// raises the cap, which attacks both known causes: jitter breaks exact f-value ties and
/// so disables the high-g collapse of the ALT quantization plateau (measured: seeds alone
/// pushed a real route from ~400k pops to over budget), and heavily gated profiles leave
/// the ALT bound loose enough that the frontier balloons. Jitter only exists to vary
/// otherwise-equal paths, so trading that variety for an actual route is the right call.
///
/// Retrying is skipped when it could not search any further than the attempt that just
/// failed, and when the request is already dead (deadline fired or client gone) — the
/// deadline, not the pop count, remains the real ceiling on both attempts.
fn budget_retry_pops(
    res: &SearchResult,
    seed: Option<u64>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    nodes: usize,
) -> Option<u32> {
    if !matches!(res.status, SearchStatus::BudgetExceeded) {
        return None;
    }
    if let Some(c) = cancel {
        if c.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
    }
    let retry = retry_max_pops(nodes)?;
    // An unseeded search that already had at least this budget would repeat itself.
    if seed.is_none() && retry <= default_max_pops(nodes).unwrap_or(u32::MAX) {
        return None;
    }
    Some(retry)
}

/// Condensed special-edge graph over walk components (roadmap 4.1).
///
/// Eligibility never gates walk edges — only macro/global/fairy — so "can this goal be
/// reached at all under this profile" is decided EXACTLY on the ~491-component
/// condensation: walk connectivity inside a component is free, and the per-request
/// question is whether eligible special edges link the start's component set to the
/// goal's. This turns every impossible/gated-goal request from a budget-capped
/// ~1.5M-pop flood (holding a search permit and a context pair for hundreds of ms —
/// seconds at 4M, doubled by the retry) into a microsecond rejection.
pub struct ComponentGraph {
    pub components: usize,
    /// Directed (src_comp, dst_comp, requirement tag idxs) per macro edge; edges whose
    /// endpoints share a component are reachability no-ops and are dropped at build.
    pub macro_edges: Vec<(u16, u16, Vec<usize>)>,
    /// (component, requirement tag idxs) per fairy ring. Eligible rings form a clique:
    /// reaching any eligible ring's component reaches all of them.
    pub fairy: Vec<(u16, Vec<usize>)>,
    /// (component, requirement tag idxs) per global teleport destination. Globals are
    /// available from the origin, so eligible entries seed the reachable set directly.
    pub globals: Vec<(u16, Vec<usize>)>,
}

/// Requirement-id -> tag-index map from the snapshot's req_tags section.
fn req_id_to_tag_idx(snapshot: &Snapshot) -> HashMap<u32, usize> {
    let req_words: &[u32] = snapshot.req_tags();
    let mut map = HashMap::new();
    let mut i = 0;
    while i + 3 < req_words.len() {
        map.insert(req_words[i], i / 4);
        i += 4;
    }
    map
}

pub fn build_component_graph(
    snapshot: &Snapshot,
    globals: &[GlobalTeleport],
    fairy_rings: &[FairyRing],
) -> ComponentGraph {
    let comp = snapshot.comp_ids();
    let components = snapshot.counts().walk_components as usize;
    let id_to_idx = req_id_to_tag_idx(snapshot);
    let msrc = snapshot.macro_src();
    let mdst = snapshot.macro_dst();
    let mut macro_edges = Vec::new();
    for idx in 0..msrc.len() {
        let (s, d) = (msrc[idx], mdst[idx]);
        if s == 0 && d == 0 {
            continue; // synthetic global-metadata carrier, not a real edge
        }
        if s as usize >= comp.len() || d as usize >= comp.len() {
            continue;
        }
        let (cs, cd) = (comp[s as usize], comp[d as usize]);
        if cs == cd {
            continue;
        }
        // Same fail-closed requirement parsing as the search setup: unknown ids map to
        // usize::MAX, which no mask satisfies.
        let mut reqs = Vec::new();
        if let Some(bytes) = snapshot.macro_meta_at(idx) {
            if let Ok(val) = serde_json::from_slice::<JsonValue>(bytes) {
                if let Some(arr) = val.get("requirements").and_then(|v| v.as_array()) {
                    for ridv in arr {
                        if let Some(rid) = ridv.as_u64() {
                            reqs.push(id_to_idx.get(&(rid as u32)).copied().unwrap_or(usize::MAX));
                        }
                    }
                }
            }
        }
        macro_edges.push((cs, cd, reqs));
    }
    let fairy = fairy_rings
        .iter()
        .filter(|r| (r.node as usize) < comp.len())
        .map(|r| (comp[r.node as usize], r.req_tag_idxs.clone()))
        .collect();
    let globals = globals
        .iter()
        .filter(|g| (g.dst as usize) < comp.len())
        .map(|g| (comp[g.dst as usize], g.reqs.clone()))
        .collect();
    ComponentGraph { components, macro_edges, fairy, globals }
}

/// Exact per-request reachability decision over [`ComponentGraph`]. `start_comp` is
/// `None` for virtual starts (the origin enters the world only through eligible
/// globals). Sound and complete: a path exists iff the goal's component is reachable
/// from the seeded set through eligible special edges, because walk edges are never
/// requirement-gated.
pub fn goal_reachable(
    cg: &ComponentGraph,
    mask: &EligibilityMask,
    start_comp: Option<u16>,
    goal_comp: u16,
) -> bool {
    let n = cg.components.max(goal_comp as usize + 1);
    let mut reached = vec![false; n];
    if let Some(sc) = start_comp {
        if (sc as usize) < n {
            reached[sc as usize] = true;
        }
    }
    for (c, reqs) in &cg.globals {
        if reqs.iter().all(|&i| mask.is_satisfied(i)) {
            reached[*c as usize] = true;
        }
    }
    let edges: Vec<(u16, u16)> = cg
        .macro_edges
        .iter()
        .filter(|(_, _, reqs)| reqs.iter().all(|&i| mask.is_satisfied(i)))
        .map(|&(s, d, _)| (s, d))
        .collect();
    let ring_comps: Vec<u16> = cg
        .fairy
        .iter()
        .filter(|(_, reqs)| reqs.iter().all(|&i| mask.is_satisfied(i)))
        .map(|&(c, _)| c)
        .collect();
    let mut fairy_joined = false;
    loop {
        if reached[goal_comp as usize] {
            return true;
        }
        let mut changed = false;
        for &(s, d) in &edges {
            if reached[s as usize] && !reached[d as usize] {
                reached[d as usize] = true;
                changed = true;
            }
        }
        if !fairy_joined && ring_comps.iter().any(|&c| reached[c as usize]) {
            for &c in &ring_comps {
                if !reached[c as usize] {
                    reached[c as usize] = true;
                    changed = true;
                }
            }
            fairy_joined = true;
        }
        if !changed {
            return reached[goal_comp as usize];
        }
    }
}

#[derive(Clone, Debug)]
pub struct GlobalTeleport {
    pub dst: u32,
    pub cost: f32,
    pub reqs: Vec<usize>,
    pub kind_first: u32,
    /// The teleport's parsed metadata entry from the snapshot's "global" array, cached
    /// at load time so /route never re-parses the ~113KB JSON blob per request.
    pub meta: Arc<JsonValue>,
}

/// Runtime representation of a Fairy Ring node
#[derive(Clone, Debug)]
pub struct FairyRing {
    pub node: u32,
    pub object_id: u64,
    pub x: i32,
    pub y: i32,
    pub plane: i32,
    pub cost_ms: f32,
    pub code: String,
    pub action: Option<String>,
    pub req_tag_idxs: Vec<usize>, // usize::MAX for fail-closed unknown requirements
}

/// Sort extra edges by (dst id, then weight) — the ordering the search engine relies on
/// when merging these edges with the static neighbor stream.
fn sort_extra_edges(edges: &mut [(u32, f32)]) {
    edges.sort_unstable_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });
}

/// Per-profile search artifacts (roadmap 5.4): everything a search rebuilds per
/// request that is actually a pure function of (snapshot, exact eligibility mask bits,
/// quick_tele) — the forward/reversed MacroFilters (959-slot scans over per-edge
/// requirement lists), the eligible global teleports, and the eligible fairy ring
/// sources/destinations. Built once per profile via [`build_profile_artifacts`] and
/// cached in the per-snapshot LRU ([`crate::SnapshotState::profile_cache`]); the
/// adapter entry points consume it instead of rebuilding.
pub struct ProfileArtifacts {
    /// Forward macro-edge eligibility/effective weights.
    pub macro_filter: MacroFilter,
    /// Same filter over the reversed macro adjacency (None when no reversed provider
    /// exists; bidirectional search is then unavailable, exactly as before).
    pub macro_filter_rev: Option<MacroFilter>,
    /// Eligible global teleports (quick-tele effective costs), sorted by (dst, w).
    pub eligible_globals: Vec<(u32, f32)>,
    /// Eligible fairy ring source nodes, sorted + deduped.
    pub fairy_sources: Vec<u32>,
    /// Eligible fairy ring destinations, sorted by (dst, w).
    pub fairy_dests: Vec<(u32, f32)>,
}

/// Build one profile's artifacts, byte-for-byte identical to what the per-request code
/// used to construct inline (same iteration order, same sorts).
pub fn build_profile_artifacts(
    neighbors: &NeighborProvider,
    neighbors_rev: Option<&NeighborProvider>,
    globals: &[GlobalTeleport],
    fairy_rings: &[FairyRing],
    mask: &EligibilityMask,
    quick_tele: bool,
) -> ProfileArtifacts {
    // Eligible global teleports.
    let mut eligible_globals: Vec<(u32, f32)> = Vec::new();
    for g in globals {
        let mut allowed = true;
        for &idx in &g.reqs {
            if !mask.is_satisfied(idx) {
                allowed = false;
                break;
            }
        }
        if allowed {
            let mut cost = g.cost;
            if quick_tele && g.kind_first == 2 {
                cost = 2400.0;
            }
            eligible_globals.push((g.dst, cost));
        }
    }
    sort_extra_edges(&mut eligible_globals);

    // Eligible fairy ring destinations (rings whose requirements are satisfied). For
    // each eligible source ring, the engine can teleport to any other eligible ring.
    // Both collections are sorted: the engine binary-searches sources per pop and
    // merges the shared destination slice in dst order (skipping the self-hop).
    let mut fairy_dests: Vec<(u32, f32)> = Vec::new();
    let mut fairy_sources: Vec<u32> = Vec::new();
    for ring in fairy_rings {
        let mut allowed = true;
        for &idx in &ring.req_tag_idxs {
            if !mask.is_satisfied(idx) {
                allowed = false;
                break;
            }
        }
        if allowed {
            fairy_sources.push(ring.node);
            fairy_dests.push((ring.node, ring.cost_ms));
        }
    }
    fairy_sources.sort_unstable();
    fairy_sources.dedup();
    sort_extra_edges(&mut fairy_dests);

    ProfileArtifacts {
        macro_filter: neighbors.macro_filter(mask, quick_tele),
        macro_filter_rev: neighbors_rev.map(|rev| rev.macro_filter(mask, quick_tele)),
        eligible_globals,
        fairy_sources,
        fairy_dests,
    }
}

fn kind_code(kind: &str) -> u32 {
    match kind {
        "door" => 1,
        "lodestone" => 2,
        "npc" => 3,
        "object" => 4,
        "item" => 5,
        "ifslot" => 6,
        _ => 0,
    }
}

pub fn build_neighbor_provider(snapshot: &Snapshot) -> (NeighborProvider, NeighborProvider, Vec<GlobalTeleport>, HashMap<(u32, u32), Vec<u32>>) {
    // 1. Build map of req_id -> tag_index
    let req_words: &[u32] = snapshot.req_tags();
    let mut id_to_idx = std::collections::HashMap::new();
    let mut i = 0;
    while i + 3 < req_words.len() {
        let req_id = req_words[i];
        id_to_idx.insert(req_id, i / 4);
        i += 4;
    }

    // 2. Iterate macro edges and parse requirements
    let msrc = snapshot.macro_src();
    let len = msrc.len();
    let mut macro_reqs: Vec<Vec<usize>> = Vec::with_capacity(len);
    let mut globals: Vec<GlobalTeleport> = Vec::new();
    let mut macro_lookup: HashMap<(u32, u32), Vec<u32>> = HashMap::with_capacity(len);
    
    let msrc_vec: &[u32] = msrc;
    let mdst_vec: &[u32] = snapshot.macro_dst();
    let mw_vec: &[f32] = snapshot.macro_w();
    let mkind_vec: &[u32] = snapshot.macro_kind_first();

    let mut missing_req_ids: u64 = 0;

    for idx in 0..len {
        let mut reqs = Vec::new();
        let mut is_global = false;

        if let Some(bytes) = snapshot.macro_meta_at(idx) {
            if let Ok(val) = serde_json::from_slice::<JsonValue>(bytes) {
                // Check for global def
                if msrc_vec[idx] == 0 && mdst_vec[idx] == 0 {
                    is_global = true;
                    if let Some(arr) = val.get("global").and_then(|v| v.as_array()) {
                         for g in arr {
                             let dst = g.get("dst").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                             let cost = g.get("cost_ms").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                             let mut g_reqs = Vec::new();
                             let kind_first = g
                                 .get("steps")
                                 .and_then(|v| v.as_array())
                                 .and_then(|a| a.first())
                                 .and_then(|s| s.get("kind"))
                                 .and_then(|v| v.as_str())
                                 .map(kind_code)
                                 .unwrap_or(0);
                             if let Some(r_arr) = g.get("requirements").and_then(|v| v.as_array()) {
                                 for ridv in r_arr {
                                     if let Some(rid) = ridv.as_u64() {
                                         if let Some(&tag_idx) = id_to_idx.get(&(rid as u32)) {
                                             g_reqs.push(tag_idx);
                                         } else {
                                             // Fail-closed: unknown requirement id means the edge can never be satisfied
                                             g_reqs.push(usize::MAX);
                                             missing_req_ids += 1;
                                         }
                                     }
                                 }
                             }
                             if dst != 0 {
                                 globals.push(GlobalTeleport { dst, cost, reqs: g_reqs, kind_first, meta: Arc::new(g.clone()) });
                             }
                         }
                    }
                }

                if !is_global {
                    if let Some(arr) = val.get("requirements").and_then(|v| v.as_array()) {
                        for ridv in arr {
                            if let Some(rid) = ridv.as_u64() {
                                if let Some(&tag_idx) = id_to_idx.get(&(rid as u32)) {
                                    reqs.push(tag_idx);
                                } else {
                                    // Fail-closed: unknown requirement id means the edge can never be satisfied
                                    reqs.push(usize::MAX);
                                    missing_req_ids += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        macro_reqs.push(reqs);
        macro_lookup
            .entry((msrc_vec[idx], mdst_vec[idx]))
            .or_insert_with(Vec::new)
            .push(idx as u32);
    }

    if missing_req_ids > 0 {
        warn!(missing_req_ids, "snapshot macro metadata referenced unknown requirement ids (will be treated as unsatisfied)");
    }

    // 3. Build the macro-edge provider (~1k edges; the walk grid is served zero-copy
    // from the snapshot's CSR sections and never rebuilt on the heap).
    let nodes = snapshot.counts().nodes as usize;
    let provider = NeighborProvider::new_with_reqs(
        nodes,
        msrc_vec, mdst_vec, mw_vec,
        mkind_vec,
        &macro_reqs,
    );
    // Reversed macro adjacency for the backward half of bidirectional searches (same
    // per-edge requirement data, src/dst swapped).
    let provider_rev = NeighborProvider::new_with_reqs(
        nodes,
        mdst_vec, msrc_vec, mw_vec,
        mkind_vec,
        &macro_reqs,
    );

    (provider, provider_rev, globals, macro_lookup)
}

/// Build fairy ring runtime data from snapshot.
/// Returns: (Vec<FairyRing>, HashMap<node_id, ring_index>)
pub fn build_fairy_rings(snapshot: &Snapshot) -> (Vec<FairyRing>, HashMap<u32, usize>) {
    // Build req_id -> tag_index map
    let req_words: &[u32] = snapshot.req_tags();
    let mut id_to_idx: HashMap<u32, usize> = HashMap::new();
    let mut i = 0;
    while i + 3 < req_words.len() {
        let req_id = req_words[i];
        id_to_idx.insert(req_id, i / 4);
        i += 4;
    }

    let fairy_count = snapshot.counts().fairy_rings as usize;
    let mut rings: Vec<FairyRing> = Vec::with_capacity(fairy_count);
    let mut node_to_ring: HashMap<u32, usize> = HashMap::with_capacity(fairy_count);
    let mut missing_req_ids: u64 = 0;

    let nodes = snapshot.fairy_nodes();
    let costs = snapshot.fairy_cost_ms();

    for idx in 0..fairy_count {
        let node = nodes.get(idx).copied().unwrap_or(0);
        let cost_ms = costs.get(idx).copied().unwrap_or(0.0);

        // Parse metadata JSON
        let (object_id, x, y, plane, code, action, req_tag_idxs) =
            if let Some(bytes) = snapshot.fairy_meta_at(idx) {
                if let Ok(val) = serde_json::from_slice::<JsonValue>(bytes) {
                    let object_id = val.get("object_id").and_then(|v| v.as_u64()).unwrap_or(0);
                    let x = val.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let y = val.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let plane = val.get("plane").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let code = val.get("code").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let action = val.get("action").and_then(|v| v.as_str()).map(|s| s.to_string());

                    // Parse requirements and map to tag indices
                    let mut reqs = Vec::new();
                    if let Some(arr) = val.get("requirements").and_then(|v| v.as_array()) {
                        for ridv in arr {
                            if let Some(rid) = ridv.as_u64() {
                                if let Some(&tag_idx) = id_to_idx.get(&(rid as u32)) {
                                    reqs.push(tag_idx);
                                } else {
                                    // Fail-closed: unknown requirement id
                                    reqs.push(usize::MAX);
                                    missing_req_ids += 1;
                                }
                            }
                        }
                    }

                    (object_id, x, y, plane, code, action, reqs)
                } else {
                    (0, 0, 0, 0, String::new(), None, Vec::new())
                }
            } else {
                (0, 0, 0, 0, String::new(), None, Vec::new())
            };

        node_to_ring.insert(node, idx);
        rings.push(FairyRing {
            node,
            object_id,
            x,
            y,
            plane,
            cost_ms,
            code,
            action,
            req_tag_idxs,
        });
    }

    if missing_req_ids > 0 {
        warn!(missing_req_ids, "fairy ring metadata referenced unknown requirement ids (will be treated as unsatisfied)");
    }

    info!(fairy_ring_count = rings.len(), "loaded fairy rings from snapshot");

    (rings, node_to_ring)
}

pub fn run_route_with_requirements_and_fairy_rings(
    snapshot: Arc<Snapshot>,
    neighbors: Arc<NeighborProvider>,
    neighbors_rev: Option<Arc<NeighborProvider>>,
    start_id: u32,
    goal_id: u32,
    mask: &EligibilityMask,
    seed: Option<u64>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    // Per-profile artifacts (filters, eligible globals/fairy sets), resolved or built
    // by the caller — pure functions of (snapshot, mask bits, quick_tele), see
    // [`build_profile_artifacts`].
    artifacts: &ProfileArtifacts,
    // Canonical pruning grid (None = full expansion); engages for unseeded rungs only.
    canonical: Option<Arc<CanonicalGrid>>,
    // Pooled per-search state (forward, backward); reset per search, checked out for
    // the duration of this call only.
    ctxs: &mut (SearchContext, SearchContext),
) -> SearchOutcome {
    // Per-request requirement diagnostics, gated behind NAVPATH_DEBUG_REQS=1 so the hot
    // path skips this scan/logging by default. Enable when debugging requirement matching.
    if req_debug_enabled() {
        let req_words: &[u32] = snapshot.req_tags();
        // Diagnostics: show computed satisfaction for requirement id 78 (expected key=hasGamesNeck, value=1)
        let mut i = 0usize;
        while i + 3 < req_words.len() {
            if req_words[i] == 78 {
                let tag_idx = i / 4;
                let key_id = req_words[i + 1];
                let opbits = req_words[i + 2];
                let rhs_val = req_words[i + 3];
                let expected_key_id = fnv1a32("hasgamesneck");
                let key_matches = key_id == expected_key_id;
                info!(tag_idx, key_id, expected_key_id, key_matches, opbits, rhs_val, satisfied = mask.is_satisfied(tag_idx), "req_id 78 evaluation");
                break;
            }
            i += 4;
        }
    }

    let nodes = snapshot.counts().nodes as usize;
    let snap_ref: &Snapshot = &snapshot;
    let lm = LandmarkHeuristic {
        nodes,
        landmarks: snap_ref.counts().landmarks as usize,
        tab: snap_ref.lm_tab(),
        quantum: snap_ref.manifest().alt_quantum_ms,
    };

    let mut view = EngineView {
        nodes,
        walk: WalkGraph::from_snapshot(snap_ref),
        macros: neighbors,
        lm,
        extra: ExtraEdges::default(),
        coords: Some(snap_ref.coords_packed()),
        canonical,
    };

    // Per-profile artifacts: eligible globals (available from every node; the engine
    // relaxes them once from the start, so they never enter per-pop neighbor merges),
    // fairy sources/dests, and the folded macro filters — all pre-sorted exactly as the
    // engine expects (see build_profile_artifacts).
    view.extra.global = artifacts.eligible_globals.clone();
    view.extra.fairy_sources = artifacts.fairy_sources.clone();
    view.extra.fairy_dests = artifacts.fairy_dests.clone();

    let macro_filter = &artifacts.macro_filter;

    // Weak-backward demotion (roadmap 4.2): compare the two lower bounds of C* the
    // engines will steer by. lb_fwd = min(h(start), min over seeds (w + h(dst))) is
    // the forward search's effective bound; hb_goal is the backward bound anchored on
    // the same origin set. If the backward bound is provably loose relative to the
    // forward one, unidirectional wins — skip bidir for this request.
    let bidir_ok = bidir_enabled() && neighbors_rev.is_some() && {
        let ratio = bidir_min_hb_ratio();
        if ratio <= 0.0 {
            true
        } else {
            let active_f = view.lm.select_active(start_id, goal_id, active_landmarks());
            let h_start = view.lm.h_active(start_id, &active_f);
            if h_start < BIDIR_POLICY_MIN_H_MS {
                true
            } else {
                let mut lb_fwd = h_start;
                for &(dst, w) in view.extra.global.iter() {
                    lb_fwd = lb_fwd.min(w + view.lm.h_active(dst, &active_f));
                }
                let mut anchors: Vec<(u32, f32)> = Vec::with_capacity(1 + view.extra.global.len());
                anchors.push((start_id, 0.0));
                anchors.extend(view.extra.global.iter().copied());
                let active_b = view.lm.select_active_rev(&anchors, goal_id, active_landmarks());
                let hb_goal = view.lm.h_active_rev(goal_id, &active_b);
                !lb_fwd.is_finite() || hb_goal >= ratio * lb_fwd
            }
        }
    };

    // Reversed adjacency + the profile's pre-built reversed filter, so the budget retry
    // below re-runs only the search, not the setup.
    let bidir = if bidir_ok {
        match (neighbors_rev.as_ref(), artifacts.macro_filter_rev.as_ref()) {
            (Some(rev), Some(filter_rev)) => Some((rev.clone(), filter_rev)),
            _ => None,
        }
    } else {
        None
    };

    let search = |seed: Option<u64>, max_pops: Option<u32>, ctxs: &mut (SearchContext, SearchContext)| -> SearchResult {
        let params = SearchParams { start: start_id, goal: goal_id, macro_filter: Some(macro_filter), seed, max_pops, cancel, bucket_ms: bucket_for(seed) };
        if let Some((rev, macro_filter_rev)) = bidir.as_ref() {
            let bp = BidirParams { macros_rev: rev, macro_filter_rev: Some(macro_filter_rev) };
            let (cf, cb) = (&mut ctxs.0, &mut ctxs.1);
            return view.astar_bidir(&bp, params, cf, cb);
        }
        view.astar(params, &mut ctxs.0)
    };

    let res = search(seed, default_max_pops(nodes), ctxs);
    retry_ladder(res, seed, cancel, nodes, |s, m| search(s, m, ctxs))
}

pub fn run_route_with_requirements_virtual_start(
    snapshot: Arc<Snapshot>,
    neighbors: Arc<NeighborProvider>,
    neighbors_rev: Option<Arc<NeighborProvider>>,
    goal_id: u32,
    seed: Option<u64>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    // Per-profile artifacts (see [`build_profile_artifacts`]). Fairy rings are wired
    // exactly as on the normal path: a virtual start enters the world through a
    // teleport but walks the same graph afterwards — omitting fairy hops here silently
    // lengthened (or failed) every off-graph-start route whose optimum was
    // teleport-entry -> walk -> fairy hop. The ALT tables bake the full fairy clique,
    // so admissibility is unaffected.
    artifacts: &ProfileArtifacts,
    canonical: Option<Arc<CanonicalGrid>>,
    ctxs: &mut (SearchContext, SearchContext),
) -> (SearchOutcome, Option<u32>) {
    let eligible_globals: &[(u32, f32)] = &artifacts.eligible_globals;
    if eligible_globals.is_empty() {
        return (
            SearchOutcome { res: not_found_result(), retried: false, attempts_pops: [0, 0, 0], seed_dropped: false },
            None,
        );
    }

    let nodes = snapshot.counts().nodes as usize;
    let snap_ref: &Snapshot = &snapshot;
    let lm = LandmarkHeuristic {
        nodes,
        landmarks: snap_ref.counts().landmarks as usize,
        tab: snap_ref.lm_tab(),
        quantum: snap_ref.manifest().alt_quantum_ms,
    };
    let mut view = EngineView {
        nodes,
        walk: WalkGraph::from_snapshot(snap_ref),
        macros: neighbors,
        lm,
        extra: ExtraEdges::default(),
        coords: Some(snap_ref.coords_packed()),
        canonical,
    };
    view.extra.fairy_sources = artifacts.fairy_sources.clone();
    view.extra.fairy_dests = artifacts.fairy_dests.clone();

    let macro_filter = &artifacts.macro_filter;

    // Weak-backward demotion, virtual-start flavor (roadmap 4.2/4.4): anchors are the
    // seed set itself, and lb_fwd = min over seeds (g0 + h(dst)).
    let bidir_ok = bidir_enabled() && neighbors_rev.is_some() && {
        let ratio = bidir_min_hb_ratio();
        if ratio <= 0.0 {
            true
        } else {
            let active_f = view.lm.select_active(goal_id, goal_id, active_landmarks());
            let mut lb_fwd = f32::INFINITY;
            for &(dst, w) in eligible_globals {
                lb_fwd = lb_fwd.min(w + view.lm.h_active(dst, &active_f));
            }
            if lb_fwd < BIDIR_POLICY_MIN_H_MS {
                true
            } else {
                let active_b = view.lm.select_active_rev(eligible_globals, goal_id, active_landmarks());
                let hb_goal = view.lm.h_active_rev(goal_id, &active_b);
                !lb_fwd.is_finite() || hb_goal >= ratio * lb_fwd
            }
        }
    };
    let bidir = if bidir_ok {
        match (neighbors_rev.as_ref(), artifacts.macro_filter_rev.as_ref()) {
            (Some(rev), Some(filter_rev)) => Some((rev.clone(), filter_rev)),
            _ => None,
        }
    } else {
        None
    };

    // One multi-source search replaces one full A* per eligible teleport: every teleport
    // destination is seeded at g = its cost, and the winning entry is path[0]. The engine
    // leaves `extra.global` unused in multi-source mode, and this out-of-graph start has
    // no mid-route teleports by construction (a second teleport at any node u would cost
    // g(u) + c >= c, dominated by seeding it directly).
    let search = |seed: Option<u64>, max_pops: Option<u32>, ctxs: &mut (SearchContext, SearchContext)| -> SearchResult {
        let params = SearchParams { start: goal_id, goal: goal_id, macro_filter: Some(macro_filter), seed, max_pops, cancel, bucket_ms: bucket_for(seed) };
        if let Some((rev, macro_filter_rev)) = bidir.as_ref() {
            let bp = BidirParams { macros_rev: rev, macro_filter_rev: Some(macro_filter_rev) };
            let (cf, cb) = (&mut ctxs.0, &mut ctxs.1);
            return view.astar_bidir_multi(eligible_globals, &bp, params, cf, cb);
        }
        view.astar_multi(eligible_globals, params, &mut ctxs.0)
    };

    let res = search(seed, default_max_pops(nodes), ctxs);
    let outcome = retry_ladder(res, seed, cancel, nodes, |s, m| search(s, m, ctxs));

    let entry = if outcome.res.found { outcome.res.path.first().copied() } else { None };
    (outcome, entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(found: bool, status: SearchStatus, cost: f32) -> SearchResult {
        SearchResult {
            found,
            status,
            path: if found { vec![0, 1] } else { Vec::new() },
            cost,
            pops: 0,
            pops_f: 0,
            pops_b: 0,
        }
    }

    fn mask_of(bits: &[bool]) -> EligibilityMask {
        EligibilityMask { satisfied: bits.to_vec() }
    }

    #[test]
    fn goal_reachable_macro_chain_and_gating() {
        // Components 0 -> 1 (req tag 0) -> 2 (req tag 1); goal in comp 2.
        let cg = ComponentGraph {
            components: 3,
            macro_edges: vec![(0, 1, vec![0]), (1, 2, vec![1])],
            fairy: vec![],
            globals: vec![],
        };
        assert!(goal_reachable(&cg, &mask_of(&[true, true]), Some(0), 2));
        assert!(!goal_reachable(&cg, &mask_of(&[true, false]), Some(0), 2));
        assert!(goal_reachable(&cg, &mask_of(&[true, false]), Some(0), 1));
        // Same component is always reachable (walk edges are never gated).
        assert!(goal_reachable(&cg, &mask_of(&[false, false]), Some(2), 2));
        // Macro edges are DIRECTED: comp 2 cannot get back to 0.
        assert!(!goal_reachable(&cg, &mask_of(&[true, true]), Some(2), 0));
    }

    #[test]
    fn goal_reachable_fairy_clique_and_globals() {
        // Rings in comps 1 and 3 (ring in 3 gated by tag 0); global teleport into comp 1.
        let cg = ComponentGraph {
            components: 4,
            macro_edges: vec![],
            fairy: vec![(1, vec![]), (3, vec![0])],
            globals: vec![(1, vec![])],
        };
        // Virtual start (no on-graph component): global seeds comp 1; eligible fairy
        // clique joins comp 3 only when its ring's requirement holds.
        assert!(goal_reachable(&cg, &mask_of(&[true]), None, 3));
        assert!(!goal_reachable(&cg, &mask_of(&[false]), None, 3));
        assert!(goal_reachable(&cg, &mask_of(&[false]), None, 1));
        // No eligible entry at all: virtual start reaches nothing.
        let cg2 = ComponentGraph { components: 2, macro_edges: vec![], fairy: vec![], globals: vec![(1, vec![0])] };
        assert!(!goal_reachable(&cg2, &mask_of(&[false]), None, 1));
    }

    #[test]
    fn better_of_prefers_proven_then_found_then_cheaper() {
        // A proven (Found) retry always wins over a truncated first attempt.
        let r = better_of(res(true, SearchStatus::BudgetExceeded, 10.0), res(true, SearchStatus::Found, 3.0));
        assert_eq!(r.status, SearchStatus::Found);
        assert!((r.cost - 3.0).abs() < 1e-6);
        // A truncated-found first attempt beats a retry that found nothing.
        let r = better_of(
            res(true, SearchStatus::BudgetExceeded, 10.0),
            res(false, SearchStatus::BudgetExceeded, f32::INFINITY),
        );
        assert!(r.found);
        assert!((r.cost - 10.0).abs() < 1e-6);
        // Two truncated paths: the cheaper one wins.
        let r = better_of(
            res(true, SearchStatus::BudgetExceeded, 10.0),
            res(true, SearchStatus::BudgetExceeded, 8.0),
        );
        assert!((r.cost - 8.0).abs() < 1e-6);
        // Neither found: the retry searched further; its status is authoritative.
        let r = better_of(
            res(false, SearchStatus::BudgetExceeded, f32::INFINITY),
            res(false, SearchStatus::NotFound, f32::INFINITY),
        );
        assert_eq!(r.status, SearchStatus::NotFound);
    }
}
