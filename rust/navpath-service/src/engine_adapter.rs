use std::sync::Arc;
use std::sync::OnceLock;
use std::cell::RefCell;
use std::collections::HashMap;

use navpath_core::{EngineView, SearchParams, SearchResult, SearchStatus, Snapshot, NeighborProvider};
use navpath_core::engine::search::{ExtraEdges, SearchContext};
use navpath_core::engine::neighbors::WalkGraph;

/// Empty "no path" result for early-exit paths in this module.
fn not_found_result() -> SearchResult {
    SearchResult { found: false, status: SearchStatus::NotFound, path: Vec::new(), cost: f32::INFINITY, pops: 0 }
}
use serde_json::Value as JsonValue;
use navpath_core::eligibility::{fnv1a32, EligibilityMask};
use navpath_core::engine::heuristics::LandmarkHeuristic;
use tracing::{info, warn};

thread_local! {
    static SEARCH_CONTEXT: RefCell<SearchContext> = RefCell::new(SearchContext::new(0));
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

/// Pop budget for a single search, from `NAVPATH_MAX_POPS` (0 disables). Defaults to
/// 500k pops — enough for any legitimate route, while capping unreachable-goal floods.
/// Cached once so the hot path never performs an env lookup.
fn default_max_pops() -> Option<u32> {
    static MAX_POPS: OnceLock<Option<u32>> = OnceLock::new();
    *MAX_POPS.get_or_init(|| {
        match std::env::var("NAVPATH_MAX_POPS").ok().and_then(|v| v.trim().parse::<u32>().ok()) {
            Some(0) => None,
            Some(n) => Some(n),
            None => Some(500_000),
        }
    })
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

pub fn build_neighbor_provider(snapshot: &Snapshot) -> (NeighborProvider, Vec<GlobalTeleport>, HashMap<(u32, u32), Vec<u32>>) {
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

    (provider, globals, macro_lookup)
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
    globals: Arc<Vec<GlobalTeleport>>,
    start_id: u32,
    goal_id: u32,
    mask: &EligibilityMask,
    quick_tele: bool,
    seed: Option<u64>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    fairy_rings: &[FairyRing],
    // Kept for API symmetry with the caller; eligible fairy sources/dests are derived
    // from `fairy_rings` directly, so the node->ring map is not needed here.
    _node_to_fairy_ring: &HashMap<u32, usize>,
) -> SearchResult {
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

    // Eligible global teleports
    let mut eligible_globals: Vec<(u32, f32)> = Vec::new();
    for g in globals.iter() {
        let mut allowed = true;
        for &idx in &g.reqs {
            if !mask.is_satisfied(idx) { allowed = false; break; }
        }
        if allowed {
            let mut cost = g.cost;
            if quick_tele && g.kind_first == 2 {
                cost = 2400.0;
            }
            eligible_globals.push((g.dst, cost));
        }
    }

    // Pre-compute eligible fairy ring destinations (rings whose requirements are satisfied)
    // For each eligible source ring, we can teleport to any other eligible ring. Both
    // collections are sorted: the engine binary-searches sources per pop and merges the
    // shared destination slice in dst order (skipping the self-hop).
    let mut eligible_fairy_dests: Vec<(u32, f32)> = Vec::new();
    let mut eligible_fairy_sources: Vec<u32> = Vec::new();
    for ring in fairy_rings {
        let mut allowed = true;
        for &idx in &ring.req_tag_idxs {
            if !mask.is_satisfied(idx) {
                allowed = false;
                break;
            }
        }
        if allowed {
            eligible_fairy_sources.push(ring.node);
            eligible_fairy_dests.push((ring.node, ring.cost_ms));
        }
    }
    eligible_fairy_sources.sort_unstable();
    eligible_fairy_sources.dedup();
    sort_extra_edges(&mut eligible_fairy_dests);

    let nodes = snapshot.counts().nodes as usize;
    let snap_ref: &Snapshot = &snapshot;
    let lm = LandmarkHeuristic {
        nodes,
        landmarks: snap_ref.counts().landmarks as usize,
        tab: snap_ref.lm_tab(),
    };

    let mut view = EngineView {
        nodes,
        walk: WalkGraph::from_snapshot(snap_ref),
        macros: neighbors,
        lm,
        extra: ExtraEdges::default(),
    };

    // Global teleports are available from every node; the engine relaxes them once from
    // the start node, so they never enter per-pop neighbor merges.
    sort_extra_edges(&mut eligible_globals);
    view.extra.global = eligible_globals;
    view.extra.fairy_sources = eligible_fairy_sources;
    view.extra.fairy_dests = eligible_fairy_dests;

    let macro_filter = view.macros.macro_filter(mask, quick_tele);

    SEARCH_CONTEXT.with(|ctx_cell| {
        let mut ctx = ctx_cell.borrow_mut();
        view.astar(SearchParams { start: start_id, goal: goal_id, macro_filter: Some(&macro_filter), seed, max_pops: default_max_pops(), cancel }, &mut ctx)
    })
}

pub fn run_route_with_requirements_virtual_start(
    snapshot: Arc<Snapshot>,
    neighbors: Arc<NeighborProvider>,
    globals: Arc<Vec<GlobalTeleport>>,
    goal_id: u32,
    mask: &EligibilityMask,
    quick_tele: bool,
    seed: Option<u64>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> (SearchResult, Option<u32>) {
    let mut eligible_globals: Vec<(u32, f32)> = Vec::new();
    for g in globals.iter() {
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

    let nodes = snapshot.counts().nodes as usize;
    let snap_ref: &Snapshot = &snapshot;
    let lm = LandmarkHeuristic {
        nodes,
        landmarks: snap_ref.counts().landmarks as usize,
        tab: snap_ref.lm_tab(),
    };
    let view = EngineView {
        nodes,
        walk: WalkGraph::from_snapshot(snap_ref),
        macros: neighbors,
        lm,
        extra: ExtraEdges::default(),
    };

    if eligible_globals.is_empty() {
        return (not_found_result(), None);
    }
    sort_extra_edges(&mut eligible_globals);
    let macro_filter = view.macros.macro_filter(mask, quick_tele);

    // One multi-source search replaces one full A* per eligible teleport: every teleport
    // destination is seeded at g = its cost, and the winning entry is path[0]. The engine
    // leaves `extra.global` unused in multi-source mode, and this out-of-graph start has
    // no mid-route teleports by construction (a second teleport at any node u would cost
    // g(u) + c >= c, dominated by seeding it directly).
    SEARCH_CONTEXT.with(|ctx_cell| {
        let mut ctx = ctx_cell.borrow_mut();
        let res = view.astar_multi(
            &eligible_globals,
            SearchParams { start: goal_id, goal: goal_id, macro_filter: Some(&macro_filter), seed, max_pops: default_max_pops(), cancel },
            &mut ctx,
        );
        if res.found {
            let entry = res.path.first().copied();
            (res, entry)
        } else {
            (res, None)
        }
    })
}
