use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;

use crate::snapshot::Snapshot;

use super::heuristics::{LandmarkHeuristic, ACTIVE_LANDMARKS};
use super::neighbors::{MacroFilter, NeighborProvider, WalkGraph};

pub struct SearchParams<'a> {
    pub start: u32,
    pub goal: u32,
    /// Per-request macro-edge eligibility/effective weights (None = all macro edges at
    /// their stored weights). Built once via [`NeighborProvider::macro_filter`].
    pub macro_filter: Option<&'a MacroFilter>,
    /// Optional seed for path randomization. If Some, adds deterministic jitter to edge weights.
    pub seed: Option<u64>,
    /// Optional cap on heap pops. When exceeded the search stops and reports
    /// [`SearchStatus::BudgetExceeded`] so callers can distinguish "gave up" from "no path".
    pub max_pops: Option<u32>,
    /// Cooperative cancellation, checked every 1024 pops. Set it from a request deadline
    /// or client-disconnect guard; a cancelled search reports [`SearchStatus::Cancelled`].
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
}

/// Simple hash function for deterministic jitter based on edge and seed
#[inline]
fn edge_jitter(seed: u64, from: u32, to: u32) -> f32 {
    // FNV-1a inspired hash combining seed, from, and to
    let mut h = seed;
    h ^= from as u64;
    h = h.wrapping_mul(0x100000001b3);
    h ^= to as u64;
    h = h.wrapping_mul(0x100000001b3);
    // Convert to small jitter in range [0, 0.1)
    ((h & 0xFFFF) as f32) / 655360.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchStatus {
    Found,
    NotFound,
    /// The pop budget ran out before the goal was settled and no path was discovered.
    BudgetExceeded,
    /// The caller cancelled the search (deadline or client disconnect).
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub found: bool,
    pub status: SearchStatus,
    pub path: Vec<u32>,
    pub cost: f32,
    /// Number of heap pops the search performed (diagnostics / budget tuning).
    pub pops: u32,
}

impl SearchResult {
    fn not_found(status: SearchStatus, pops: u32) -> Self {
        SearchResult { found: false, status, path: Vec::new(), cost: f32::INFINITY, pops }
    }
}

#[derive(Clone, Copy)]
pub struct Key { pub f: f32, pub g: f32, pub id: u32 }

impl PartialEq for Key { fn eq(&self, other: &Self) -> bool { self.f == other.f && self.g == other.g && self.id == other.id } }
impl Eq for Key {}
impl PartialOrd for Key { fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) } }
impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        // Min-heap on f (BinaryHeap is a max-heap, so f compares reversed). On f-ties,
        // prefer HIGH g: deeper entries are closer to the goal, which avoids sweeping
        // whole equal-f plateaus breadth-first on uniform-cost grids.
        let a = self.f.partial_cmp(&other.f).unwrap_or(Ordering::Equal).reverse();
        if a != Ordering::Equal { return a; }
        let b = self.g.partial_cmp(&other.g).unwrap_or(Ordering::Equal);
        if b != Ordering::Equal { return b; }
        self.id.cmp(&other.id).reverse()
    }
}

/// Per-node search state, packed so one relaxation touches a single 16-byte record
/// (four records per cache line) instead of four parallel arrays. `h` caches the ALT
/// heuristic for the node, valid for the same `gen` — a grid node can be relaxed from
/// up to 8 predecessors, but pays the landmark gather only once per query.
#[derive(Clone, Copy)]
pub struct NodeState {
    pub g: f32,
    pub h: f32,
    pub parent: u32,
    pub gen: u32,
}

const EMPTY_STATE: NodeState = NodeState { g: f32::INFINITY, h: 0.0, parent: u32::MAX, gen: 0 };

pub struct SearchContext {
    pub nodes: Vec<NodeState>,
    pub generation: u32,
    pub open: BinaryHeap<Key>,
}

impl SearchContext {
    pub fn new(nodes: usize) -> Self {
        Self {
            nodes: vec![EMPTY_STATE; nodes],
            generation: 1,
            open: BinaryHeap::with_capacity(1024),
        }
    }

    pub fn reset(&mut self, nodes: usize) {
        if self.nodes.len() != nodes {
            *self = Self::new(nodes);
        } else {
            self.generation = self.generation.wrapping_add(1);
            if self.generation == 0 {
                 self.nodes.fill(EMPTY_STATE);
                 self.generation = 1;
            }
            self.open.clear();
        }
    }

    #[inline(always)]
    pub fn get_g(&self, u: usize) -> f32 {
        let st = self.nodes[u];
        if st.gen == self.generation { st.g } else { f32::INFINITY }
    }

    #[inline(always)]
    pub fn set_g(&mut self, u: usize, val: f32) {
        let gen = self.generation;
        let st = &mut self.nodes[u];
        if st.gen != gen {
            st.gen = gen;
            st.h = f32::NAN; // h not yet computed this query
        }
        st.g = val;
    }

    #[inline(always)]
    pub fn set_parent(&mut self, u: usize, p: u32) {
        // Assume set_g was called first to init generation
        self.nodes[u].parent = p;
    }

    #[inline(always)]
    pub fn get_parent(&self, u: usize) -> u32 {
        let st = self.nodes[u];
        if st.gen == self.generation { st.parent } else { u32::MAX }
    }

    /// Cached heuristic for a node already stamped by `set_g` this query; computes and
    /// stores it on first use (NAN marks "not yet computed").
    #[inline(always)]
    fn h_cached(&mut self, u: usize, compute: impl FnOnce() -> f32) -> f32 {
        let st = &mut self.nodes[u];
        if st.h.is_nan() {
            st.h = compute();
        }
        st.h
    }
}

/// Extra edges injected into the search beyond the static walk/macro graph.
///
/// - `global` edges are available from *every* node (e.g. global teleports). Their cost is
///   source-independent, so using one from any node u costs `g(u) + c >= g(start) + c`:
///   the search relaxes them exactly once from the start node and never merges them into
///   per-pop neighbor streams.
/// - Fairy-ring hops are location-specific: nodes listed in `fairy_sources` (sorted) can
///   hop to every entry in `fairy_dests` (sorted by dst id; the self-hop is skipped during
///   the merge). Both slices are shared for the whole query — no per-pop allocation.
#[derive(Default)]
pub struct ExtraEdges {
    pub global: Vec<(u32, f32)>,
    /// Sorted node ids that have fairy-ring hops available.
    pub fairy_sources: Vec<u32>,
    /// Sorted (dst, cost) fairy destinations shared by all sources.
    pub fairy_dests: Vec<(u32, f32)>,
}

pub struct EngineView<'a> {
    pub nodes: usize,
    pub walk: WalkGraph<'a>,
    pub macros: Arc<NeighborProvider>,
    pub lm: LandmarkHeuristic<'a>,
    pub extra: ExtraEdges,
}

impl<'a> EngineView<'a> {
    pub fn from_snapshot(s: &'a Snapshot) -> Self {
        let nodes = s.counts().nodes as usize;
        let macros = NeighborProvider::new(nodes, s.macro_src(), s.macro_dst(), s.macro_w());
        let lm = LandmarkHeuristic { nodes, landmarks: s.counts().landmarks as usize, tab: s.lm_tab() };
        EngineView {
            nodes,
            walk: WalkGraph::from_snapshot(s),
            macros: Arc::new(macros),
            lm,
            extra: ExtraEdges::default(),
        }
    }

    pub fn from_parts(nodes: usize, walk_src: &'a [u32], walk_dst: &'a [u32], walk_w: &'a [f32], macro_src: &'a [u32], macro_dst: &'a [u32], macro_w: &'a [f32], lm_tab: &'a [u16], landmarks: usize) -> Self {
        let macros = NeighborProvider::new(nodes, macro_src, macro_dst, macro_w);
        let lm = LandmarkHeuristic { nodes, landmarks, tab: lm_tab };
        EngineView {
            nodes,
            walk: WalkGraph::from_edges(nodes, walk_src, walk_dst, walk_w),
            macros: Arc::new(macros),
            lm,
            extra: ExtraEdges::default(),
        }
    }

    pub fn astar(&self, params: SearchParams, ctx: &mut SearchContext) -> SearchResult {
        self.search_core(Some(params.start), &[], params.goal, &params, ctx)
    }

    /// Multi-source search with no on-graph origin: every `(node, initial_g)` seed enters
    /// the open list with parent = u32::MAX, and the returned path starts at the winning
    /// seed (`path[0]`). Used for virtual starts, where an out-of-graph origin can enter
    /// the world through any eligible global teleport — one search replaces one full
    /// search per teleport. `extra.global` is NOT auto-seeded here; pass it as `seeds`.
    pub fn astar_multi(&self, seeds: &[(u32, f32)], params: SearchParams, ctx: &mut SearchContext) -> SearchResult {
        self.search_core(None, seeds, params.goal, &params, ctx)
    }

    fn search_core(
        &self,
        origin: Option<u32>,
        seeds: &[(u32, f32)],
        goal_id: u32,
        params: &SearchParams,
        ctx: &mut SearchContext,
    ) -> SearchResult {
        let n = self.nodes;
        let goal = goal_id as usize;

        ctx.reset(n);

        // Pick the best landmarks for this query once, and cache the goal's landmark row
        // inside the returned selection. Every heuristic call below evaluates only these
        // active landmarks, keeping the ALT bound admissible while cutting the per-node
        // cost from all landmarks to a small constant. Multi-source searches rank by the
        // goal side only (any goal-finite subset is admissible).
        let rank_from = origin.unwrap_or(goal_id);
        let active = self.lm.select_active(rank_from, goal_id, ACTIVE_LANDMARKS);

        // Admissibility invariant: the ALT tables must be built over a SUPERSET of the
        // edges this search relaxes from non-origin nodes. The builder therefore includes
        // the full fairy-ring clique (per-request eligibility only removes fairy edges,
        // which keeps the bound valid) and floors quick-tele-adjustable lodestone macro
        // weights. Globals need no table coverage: they are relaxed only as origin seeds
        // below, so no other node can use them mid-search.
        //
        // h(u) == INFINITY proves u cannot reach the goal through any edge the search may
        // relax, so such nodes are never pushed. This also prevents an f=INF heap
        // plateau, where tie-breaking degrades into DFS-like mass reopenings.
        let h = |u: u32| -> f32 { self.lm.h_active(u, &active) };

        if let Some(start_id) = origin {
            let start = start_id as usize;
            ctx.set_g(start, 0.0);
            ctx.set_parent(start, u32::MAX);
            let h_start = ctx.h_cached(start, || h(start_id));
            if h_start.is_finite() {
                ctx.open.push(Key { f: h_start, g: 0.0, id: start_id });
            }

            // Global teleports cost the same from every node, so taking one at the start
            // (g = 0) dominates taking it anywhere later. Relax them all exactly once
            // here instead of merging them into every pop's neighbor stream.
            for &(dst, w) in self.extra.global.iter() {
                if dst as usize >= n { continue; }
                let w_jittered = match params.seed {
                    Some(seed) => w + edge_jitter(seed, start_id, dst),
                    None => w,
                };
                if w_jittered < ctx.get_g(dst as usize) {
                    ctx.set_g(dst as usize, w_jittered);
                    ctx.set_parent(dst as usize, start_id);
                    let hv = ctx.h_cached(dst as usize, || h(dst));
                    if hv.is_finite() {
                        ctx.open.push(Key { f: w_jittered + hv, g: w_jittered, id: dst });
                    }
                }
            }
        }

        for &(node, g0) in seeds {
            if node as usize >= n { continue; }
            if g0 < ctx.get_g(node as usize) {
                ctx.set_g(node as usize, g0);
                ctx.set_parent(node as usize, u32::MAX);
                let hv = ctx.h_cached(node as usize, || h(node));
                if hv.is_finite() {
                    ctx.open.push(Key { f: g0 + hv, g: g0, id: node });
                }
            }
        }

        let mut pops: u32 = 0;
        let mut ended: Option<SearchStatus> = None;

        while let Some(Key { f: _, g: gcur, id }) = ctx.open.pop() {
            let u = id as usize;
            // Lazy-deletion: skip heap entries that were superseded by a better g.
            if gcur > ctx.get_g(u) { continue; }
            if u == goal { break; }

            pops += 1;
            if let Some(max) = params.max_pops {
                if pops > max { ended = Some(SearchStatus::BudgetExceeded); break; }
            }
            if pops & 1023 == 0 {
                if let Some(c) = params.cancel {
                    if c.load(std::sync::atomic::Ordering::Relaxed) {
                        ended = Some(SearchStatus::Cancelled);
                        break;
                    }
                }
            }

            // Fairy hops exist for only a handful of nodes; membership is a binary search
            // over a small sorted slice, and hits borrow the shared destination slice.
            let extra_slice: &[(u32, f32)] =
                if !self.extra.fairy_sources.is_empty()
                    && self.extra.fairy_sources.binary_search(&id).is_ok()
                {
                    &self.extra.fairy_dests
                } else {
                    &[]
                };

            let relax = |v_id: u32, w: f32, ctx: &mut SearchContext| {
                let v = v_id as usize;
                // Add deterministic jitter if seed is provided
                let w_jittered = match params.seed {
                    Some(seed) => w + edge_jitter(seed, id, v_id),
                    None => w,
                };
                let ng = gcur + w_jittered;
                if ng < ctx.get_g(v) {
                    ctx.set_g(v, ng);
                    ctx.set_parent(v, id);
                    let hv = ctx.h_cached(v, || h(v_id));
                    if hv.is_finite() {
                        ctx.open.push(Key { f: ng + hv, g: ng, id: v_id });
                    }
                }
            };

            for (v_id, w) in self.walk.neighbors(id) {
                relax(v_id, w, ctx);
            }
            for (v_id, w) in self.macros.macro_neighbors(id, params.macro_filter) {
                relax(v_id, w, ctx);
            }
            for &(v_id, w) in extra_slice {
                if v_id != id {
                    relax(v_id, w, ctx);
                }
            }
        }
        if ctx.get_g(goal) == f32::INFINITY {
            return SearchResult::not_found(ended.unwrap_or(SearchStatus::NotFound), pops);
        }
        let mut path = Vec::new();
        let mut cur = goal_id;
        loop {
            if let Some(start_id) = origin {
                if cur == start_id {
                    path.push(cur);
                    break;
                }
            }
            path.push(cur);
            let p = ctx.get_parent(cur as usize);
            if p == u32::MAX { break; }
            cur = p;
        }
        path.reverse();
        SearchResult { found: true, status: SearchStatus::Found, path, cost: ctx.get_g(goal), pops }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_view<'a>(
        walk_src: &'a [u32], walk_dst: &'a [u32], walk_w: &'a [f32],
        macro_src: &'a [u32], macro_dst: &'a [u32], macro_w: &'a [f32],
        nodes: usize,
    ) -> EngineView<'a> {
        EngineView::from_parts(
            nodes,
            walk_src, walk_dst, walk_w,
            macro_src, macro_dst, macro_w,
            &[], 0,
        )
    }

    #[test]
    fn astar_simple_line_prefers_walk_edges() {
        let walk_src = [0u32, 1u32];
        let walk_dst = [1u32, 2u32];
        let walk_w = [1.0f32, 1.0f32];
        let macro_src = [0u32];
        let macro_dst = [2u32];
        let macro_w = [3.5f32];
        let view = line_view(&walk_src, &walk_dst, &walk_w, &macro_src, &macro_dst, &macro_w, 3);
        let mut ctx = SearchContext::new(3);
        let res = view.astar(SearchParams { start: 0, goal: 2, macro_filter: None, seed: None, max_pops: None, cancel: None }, &mut ctx);
        assert!(res.found);
        assert_eq!(res.status, SearchStatus::Found);
        assert_eq!(res.path, vec![0,1,2]);
        assert!((res.cost - 2.0).abs() < 1e-6);
    }

    #[test]
    fn global_teleports_relaxed_from_start() {
        // 0 -1-> 1 -1-> 2 -1-> 3, plus a global teleport to node 3 costing 1.5.
        // The teleport must win even though it is never merged mid-search.
        let walk_src = [0u32, 1, 2];
        let walk_dst = [1u32, 2, 3];
        let walk_w = [1.0f32, 1.0, 1.0];
        let mut view = line_view(&walk_src, &walk_dst, &walk_w, &[], &[], &[], 4);
        view.extra.global = vec![(3, 1.5)];
        let mut ctx = SearchContext::new(4);
        let res = view.astar(SearchParams { start: 0, goal: 3, macro_filter: None, seed: None, max_pops: None, cancel: None }, &mut ctx);
        assert!(res.found);
        assert_eq!(res.path, vec![0, 3]);
        assert!((res.cost - 1.5).abs() < 1e-6);
    }

    #[test]
    fn fairy_hops_only_from_source_nodes() {
        // 0 -1-> 1 -1-> 2; node 1 is a fairy source and node 3 a fairy destination.
        let walk_src = [0u32, 1];
        let walk_dst = [1u32, 2];
        let walk_w = [1.0f32, 1.0];
        let mut view = line_view(&walk_src, &walk_dst, &walk_w, &[], &[], &[], 4);
        view.extra.fairy_sources = vec![1, 3];
        view.extra.fairy_dests = vec![(1, 0.25), (3, 0.25)];
        let mut ctx = SearchContext::new(4);
        let res = view.astar(SearchParams { start: 0, goal: 3, macro_filter: None, seed: None, max_pops: None, cancel: None }, &mut ctx);
        assert!(res.found);
        assert_eq!(res.path, vec![0, 1, 3]);
        assert!((res.cost - 1.25).abs() < 1e-6);
        // ...but a non-source node cannot hop: from 0 straight to 3 is impossible
        // without passing through the ring at 1.
        let res2 = view.astar(SearchParams { start: 2, goal: 3, macro_filter: None, seed: None, max_pops: None, cancel: None }, &mut ctx);
        assert!(!res2.found);
        assert_eq!(res2.status, SearchStatus::NotFound);
    }

    #[test]
    fn multi_source_picks_cheapest_entry() {
        // Two chains: 0 -1-> 1 -1-> 2 (goal), and 3 -1-> 2. Seeds: node 0 at g=5, node 3
        // at g=1. Best total is via 3 (1 + 1 = 2); path must start at the winning seed.
        let walk_src = [0u32, 1, 3];
        let walk_dst = [1u32, 2, 2];
        let walk_w = [1.0f32, 1.0, 1.0];
        let view = line_view(&walk_src, &walk_dst, &walk_w, &[], &[], &[], 4);
        let mut ctx = SearchContext::new(4);
        let params = SearchParams { start: 0, goal: 2, macro_filter: None, seed: None, max_pops: None, cancel: None };
        let res = view.astar_multi(&[(0, 5.0), (3, 1.0)], params, &mut ctx);
        assert!(res.found);
        assert_eq!(res.path, vec![3, 2]);
        assert!((res.cost - 2.0).abs() < 1e-6);
    }

    #[test]
    fn budget_exceeded_reports_distinct_status() {
        // Long chain, unreachable goal (node 5 disconnected).
        let walk_src = [0u32, 1, 2, 3];
        let walk_dst = [1u32, 2, 3, 4];
        let walk_w = [1.0f32, 1.0, 1.0, 1.0];
        let view = line_view(&walk_src, &walk_dst, &walk_w, &[], &[], &[], 6);
        let mut ctx = SearchContext::new(6);
        let res = view.astar(SearchParams { start: 0, goal: 5, macro_filter: None, seed: None, max_pops: Some(2), cancel: None }, &mut ctx);
        assert!(!res.found);
        assert_eq!(res.status, SearchStatus::BudgetExceeded);
        // Without a budget the same query exhausts the heap and reports NotFound.
        let res2 = view.astar(SearchParams { start: 0, goal: 5, macro_filter: None, seed: None, max_pops: None, cancel: None }, &mut ctx);
        assert!(!res2.found);
        assert_eq!(res2.status, SearchStatus::NotFound);
    }
}
