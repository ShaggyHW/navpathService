use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;

use crate::snapshot::{walk_diagonal_ms, Snapshot, WALK_CARDINAL_MS};

use super::canonical::CanonicalGrid;
use super::heuristics::{active_landmarks, LandmarkHeuristic};
use super::neighbors::{MacroFilter, NeighborProvider, WalkGraph};

/// Inputs specific to the backward half of a bidirectional search.
pub struct BidirParams<'a> {
    /// Reversed macro adjacency (same per-edge requirement data, src/dst swapped).
    pub macros_rev: &'a NeighborProvider,
    /// Per-request filter for the reversed macro slots.
    pub macro_filter_rev: Option<&'a MacroFilter>,
}

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
    /// Opt-in plateau tie-break (roadmap 3.4): 0.0 = exact ordering (default). When
    /// positive, the heap compares f at this bucket granularity (then HIGH g), so the
    /// u16-ALT equal-cost plateau — whose exact-tie collapse seed jitter otherwise
    /// destroys — is dived instead of swept. PROVABLY bounded: at goal-pop every open
    /// entry's f exceeds the goal bucket's lower edge, so served cost <= optimum (on
    /// the jittered graph) + bucket_ms. MM's stop rule reads the bucket's lower edge,
    /// which only understates the frontier minimum — the same bound holds.
    pub bucket_ms: f32,
}

/// Deterministic per-edge jitter in [0, 0.1) ms, keyed on (seed, from, to).
///
/// Public because it is part of the engine's cost contract: backward bidirectional
/// relaxations must price the forward edge identity, and external validators (the
/// golden replay re-coster) must be able to reproduce a returned path's exact cost.
#[inline]
pub fn edge_jitter(seed: u64, from: u32, to: u32) -> f32 {
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
    /// The pop budget ran out before optimality was proven. With `found == false` no
    /// path was discovered (the goal may still be reachable); with `found == true` the
    /// returned path is valid but NOT proven optimal — a truncated search must never
    /// masquerade as `Found`, or callers cache/serve unproven costs and the budget
    /// retry never fires.
    BudgetExceeded,
    /// The caller cancelled the search (deadline or client disconnect). Same
    /// found-true/false semantics as [`SearchStatus::BudgetExceeded`].
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
    /// Forward-side expansions (== `pops` for unidirectional searches).
    pub pops_f: u32,
    /// Backward-side expansions (0 for unidirectional searches). A backward share far
    /// above the forward one on a slow query is the weak-backward-bound signature
    /// (roadmap 4.2) — the signal the bidir/uni demotion policy is tuned against.
    pub pops_b: u32,
}

impl SearchResult {
    fn not_found(status: SearchStatus, pops_f: u32, pops_b: u32) -> Self {
        SearchResult {
            found: false,
            status,
            path: Vec::new(),
            cost: f32::INFINITY,
            pops: pops_f + pops_b,
            pops_f,
            pops_b,
        }
    }
}

/// Heap entry with a PRECOMPUTED packed ordering key: `k`'s unsigned order is exactly
/// the pop order — smaller f first, then (on f-ties) LARGER g (deeper entries are
/// closer to the goal, avoiding breadth-first sweeps of equal-f plateaus). Every key is
/// non-negative finite (pushes are h.is_finite()-guarded; g sums non-negative weights),
/// so IEEE `to_bits` is order-preserving. Packing once at push makes each sift
/// comparison a single integer compare — measured cheaper than the float chains on
/// plateau-heavy searches (where the f-tie chain ran constantly) AND on cache-warm
/// short searches (where per-compare packing was a measured 30-45% regression).
#[derive(Clone, Copy)]
pub struct Key {
    k: u64,
    pub g: f32,
    pub id: u32,
}

impl Key {
    #[inline(always)]
    pub fn new(f: f32, g: f32, id: u32) -> Self {
        Key { k: ((!f.to_bits() as u64) << 32) | g.to_bits() as u64, g, id }
    }

    /// Bucketed ordering key (roadmap 3.4): with `bucket_ms > 0` the high half is the
    /// f bucket index instead of the exact f bits, so entries within one bucket order
    /// by HIGH g — the plateau dive. `bucket_ms == 0` is exactly [`Key::new`].
    #[inline(always)]
    pub fn new_bucketed(f: f32, g: f32, id: u32, bucket_ms: f32) -> Self {
        let hi = if bucket_ms > 0.0 { !((f / bucket_ms) as u32) } else { !f.to_bits() };
        Key { k: ((hi as u64) << 32) | g.to_bits() as u64, g, id }
    }

    /// A LOWER bound on this entry's f: exact when unbucketed, the bucket's lower edge
    /// otherwise. Understating the frontier minimum keeps MM's stop rule conservative.
    #[inline(always)]
    pub fn f_lower(&self, bucket_ms: f32) -> f32 {
        let hi = !((self.k >> 32) as u32);
        if bucket_ms > 0.0 { hi as f32 * bucket_ms } else { f32::from_bits(hi) }
    }
}

impl PartialEq for Key { fn eq(&self, other: &Self) -> bool { self.cmp(other) == Ordering::Equal } }
impl Eq for Key {}
impl PartialOrd for Key { fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) } }
impl Ord for Key {
    #[inline(always)]
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap: Greater = popped first. The (g, id) pair is unique
        // per heap (pushes require strictly smaller g), so this total order — and hence
        // the pop sequence — is identical to the original two-float-chain comparator.
        let a = self.k.cmp(&other.k);
        if a != Ordering::Equal { return a; }
        self.id.cmp(&other.id).reverse()
    }
}

/// Best-effort prefetch of a node's search state (prefetches never fault, so the
/// unchecked pointer arithmetic is safe for any id). Used to overlap the relax loop's
/// random 16-byte NodeState loads and the next stale-check line with current work —
/// the loop is memory-bound, not branch-bound (measured; PGO showed no win).
#[inline(always)]
fn prefetch_node(nodes: &[NodeState], id: u32) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        _mm_prefetch(nodes.as_ptr().wrapping_add(id as usize) as *const i8, _MM_HINT_T0);
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (nodes, id);
    }
}

/// Canonical-pruning context for one search: engaged only when the grid exists, the
/// walk graph is the zero-copy CSR, coords are available, and the search is UNSEEDED
/// (pruning is cost-exact; jitter both breaks the uniform-cost premise of the table
/// and exists to explore the tie variety pruning removes).
#[inline]
fn canonical_ctx<'a>(
    view: &'a EngineView,
    seed: Option<u64>,
) -> Option<(&'a CanonicalGrid, &'a [u32], &'a [u32], &'a [u32])> {
    if seed.is_some() {
        return None;
    }
    let cg = view.canonical.as_deref()?;
    let coords = view.coords?;
    let WalkGraph::Csr { offsets, dst, .. } = &view.walk else {
        return None;
    };
    Some((cg, coords, offsets, dst))
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

/// All-zero bytes are a valid, semantically-empty NodeState: `gen == 0` never matches
/// the live generation (which starts at 1 and the wrap path re-fills), so `g`/`parent`
/// read as INFINITY/unset through the generation guards, and `h` is only ever read
/// after `set_g`/relax stamp the record (writing the NAN sentinel first). Allocating
/// zeroed instead of `vec![EMPTY_STATE; n]` lets the allocator hand back untouched
/// kernel zero pages: no multi-MB memset on context creation (the recurring
/// cold-thread p99 spike), and physical commit proportional to the touched search
/// corridor instead of `nodes * 16 B`.
fn zeroed_states(n: usize) -> Vec<NodeState> {
    if n == 0 {
        return Vec::new();
    }
    let layout = std::alloc::Layout::array::<NodeState>(n).expect("NodeState array layout");
    unsafe {
        let ptr = std::alloc::alloc_zeroed(layout) as *mut NodeState;
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Vec::from_raw_parts(ptr, n, n)
    }
}

pub struct SearchContext {
    pub nodes: Vec<NodeState>,
    pub generation: u32,
    pub open: BinaryHeap<Key>,
}

impl SearchContext {
    pub fn new(nodes: usize) -> Self {
        Self {
            nodes: zeroed_states(nodes),
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
    /// Packed (plane,y,x) coordinates in node-id order — parent-direction recovery for
    /// canonical pruning. Set by [`EngineView::from_snapshot`]; None disables pruning.
    pub coords: Option<&'a [u32]>,
    /// Canonical strict-domination successor grid (roadmap Phase E Stage 2a), built
    /// once per snapshot. Pruning engages only for UNSEEDED searches: it is cost-exact
    /// (every optimal path survives), while seeded jitter both breaks the uniform-cost
    /// premise of the table and exists precisely to explore tie variety.
    pub canonical: Option<Arc<CanonicalGrid>>,
}

impl<'a> EngineView<'a> {
    pub fn from_snapshot(s: &'a Snapshot) -> Self {
        let nodes = s.counts().nodes as usize;
        let macros = NeighborProvider::new(nodes, s.macro_src(), s.macro_dst(), s.macro_w());
        let lm = LandmarkHeuristic { nodes, landmarks: s.counts().landmarks as usize, tab: s.lm_tab(), quantum: s.manifest().alt_quantum_ms };
        EngineView {
            nodes,
            walk: WalkGraph::from_snapshot(s),
            macros: Arc::new(macros),
            lm,
            extra: ExtraEdges::default(),
            coords: Some(s.coords_packed()),
            canonical: None,
        }
    }

    pub fn from_parts(nodes: usize, walk_src: &'a [u32], walk_dst: &'a [u32], walk_w: &'a [f32], macro_src: &'a [u32], macro_dst: &'a [u32], macro_w: &'a [f32], lm_tab: &'a [u16], landmarks: usize) -> Self {
        let macros = NeighborProvider::new(nodes, macro_src, macro_dst, macro_w);
        let lm = LandmarkHeuristic { nodes, landmarks, tab: lm_tab, quantum: crate::snapshot::ALT_QUANTUM_MS };
        EngineView {
            nodes,
            walk: WalkGraph::from_edges(nodes, walk_src, walk_dst, walk_w),
            macros: Arc::new(macros),
            lm,
            extra: ExtraEdges::default(),
            coords: None,
            canonical: None,
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
        let bucket = params.bucket_ms;

        ctx.reset(n);

        // Pick the best landmarks for this query once, and cache the goal's landmark row
        // inside the returned selection. Every heuristic call below evaluates only these
        // active landmarks, keeping the ALT bound admissible while cutting the per-node
        // cost from all landmarks to a small constant. Multi-source searches rank by the
        // goal side only (any goal-finite subset is admissible).
        let rank_from = origin.unwrap_or(goal_id);
        let active = self.lm.select_active(rank_from, goal_id, active_landmarks());

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
                ctx.open.push(Key::new_bucketed(h_start, 0.0, start_id, bucket));
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
                        ctx.open.push(Key::new_bucketed(w_jittered + hv, w_jittered, dst, bucket));
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
                    ctx.open.push(Key::new_bucketed(g0 + hv, g0, node, bucket));
                }
            }
        }

        let mut pops: u32 = 0;
        let max_pops = params.max_pops.unwrap_or(u32::MAX);
        let mut ended: Option<SearchStatus> = None;
        // Incumbent-bound push pruning: g(goal) can be finite from pop zero (a global
        // teleport seed) and only decreases; an entry pushed with f strictly above the
        // current incumbent can never pop before the goal does (the goal's own key has
        // f = g(goal) exactly, since h(goal) == 0), so the push is dead work. Pruning
        // is pop-sequence-invariant — bit-exact for path, cost, and the pops counter.
        let mut incumbent = ctx.get_g(goal);
        let canon = canonical_ctx(self, params.seed);
        let diag_w = walk_diagonal_ms();

        while let Some(Key { g: gcur, id, .. }) = ctx.open.pop() {
            let u = id as usize;
            // Overlap the NEXT iteration's stale-check load with this expansion.
            if let Some(k) = ctx.open.peek() {
                prefetch_node(&ctx.nodes, k.id);
            }
            // Lazy-deletion: skip heap entries that were superseded by a better g.
            if gcur > ctx.get_g(u) { continue; }
            if u == goal { break; }

            pops += 1;
            if pops > max_pops { ended = Some(SearchStatus::BudgetExceeded); break; }
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

            // One fused NodeState access per relaxation: the generation check, g
            // compare, g/parent/h writes, and the h cache all touch the same 16-byte
            // record through a single borrow instead of four accessor round-trips.
            let mut relax = |v_id: u32, w: f32, ctx: &mut SearchContext| {
                let v = v_id as usize;
                // Add deterministic jitter if seed is provided
                let w_jittered = match params.seed {
                    Some(seed) => w + edge_jitter(seed, id, v_id),
                    None => w,
                };
                let ng = gcur + w_jittered;
                let gen = ctx.generation;
                let st = &mut ctx.nodes[v];
                let cur_g = if st.gen == gen { st.g } else { f32::INFINITY };
                if ng < cur_g {
                    if st.gen != gen {
                        st.gen = gen;
                        st.h = f32::NAN;
                    }
                    st.g = ng;
                    st.parent = id;
                    let hv = if st.h.is_nan() {
                        let hh = h(v_id);
                        st.h = hh;
                        hh
                    } else {
                        st.h
                    };
                    if hv.is_finite() {
                        if v == goal {
                            incumbent = ng;
                        }
                        let f = ng + hv;
                        if f <= incumbent {
                            ctx.open.push(Key::new_bucketed(f, ng, v_id, bucket));
                        }
                    }
                }
            };

            if let Some((cg, coords, offsets, dst)) = canon {
                // Canonical strict-domination pruning: relax only the successor bits
                // for the stored parent's incoming direction, resolved to CSR slots in
                // O(1); weights derive from the direction bit (0-3 cardinal).
                let parent = ctx.nodes[u].parent;
                let mask = cg.masks[u];
                let mut bits = cg.succ_bits(id, parent, coords) & mask;
                let s0 = offsets[u] as usize;
                while bits != 0 {
                    let d = bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    let slot = s0 + (mask & ((1u8 << d) - 1)).count_ones() as usize;
                    let w = if d < 4 { WALK_CARDINAL_MS } else { diag_w };
                    relax(dst[slot], w, ctx);
                }
            } else {
                // Issue all neighbor NodeState loads up front: the relax loop's
                // improving branch is data-dependent, so hardware alone cannot keep 8
                // misses in flight across mispredicts.
                for &d in self.walk.neighbor_ids(id) {
                    prefetch_node(&ctx.nodes, d);
                }
                self.walk.for_each_neighbor(id, |v_id, w| relax(v_id, w, ctx));
            }
            if self.macros.has_macro(id) {
                for (v_id, w) in self.macros.macro_neighbors(id, params.macro_filter) {
                    relax(v_id, w, ctx);
                }
            }
            for &(v_id, w) in extra_slice {
                if v_id != id {
                    relax(v_id, w, ctx);
                }
            }
        }
        if ctx.get_g(goal) == f32::INFINITY {
            return SearchResult::not_found(ended.unwrap_or(SearchStatus::NotFound), pops, 0);
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
        // A budget/cancel break can land AFTER a path to the goal was discovered but
        // BEFORE the goal popped (i.e. before its cost was proven minimal) — e.g. a
        // global teleport seeds g(goal) at pop zero. Surface the truncation in the
        // status so callers can retry / refuse to cache instead of trusting the cost.
        let status = ended.unwrap_or(SearchStatus::Found);
        SearchResult { found: true, status, path, cost: ctx.get_g(goal), pops, pops_f: pops, pops_b: 0 }
    }
}


impl<'a> EngineView<'a> {
    /// Bidirectional A* in the MM formulation (Holte et al., "Bidirectional Search That
    /// Is Guaranteed to Meet in the Middle"): each side is ordered by
    /// pr(n) = max(g(n) + h(n), 2*g(n)) with its own ADMISSIBLE bound (forward:
    /// d(.,goal) via `h_active`; backward: d(origins,.) via `h_active_rev` anchored on
    /// start+seeds), and the search stops once the best meeting cost mu <=
    /// min(prmin_f, prmin_b). The 2g term guarantees neither side expands past the
    /// midpoint, which is where bidirection wins; correctness needs only admissibility,
    /// so quantization inconsistency cannot break exactness (unlike average-potential
    /// formulations).
    ///
    /// The walk graph must be symmetric (the builder asserts this at build time);
    /// backward macro edges come from `bp.macros_rev`; backward fairy predecessors of a
    /// ring x are all other eligible rings at weight cost(x); globals never appear in
    /// the backward direction (they are forward seeds only, where g(start)=0 dominates).
    pub fn astar_bidir(
        &self,
        bp: &BidirParams,
        params: SearchParams,
        ctx_f: &mut SearchContext,
        ctx_b: &mut SearchContext,
    ) -> SearchResult {
        self.bidir_core(Some(params.start), &[], bp, params, ctx_f, ctx_b)
    }

    /// Multi-source bidirectional MM: every `(node, initial_g)` seed enters the
    /// forward frontier with parent = u32::MAX (unjittered, exactly like
    /// [`EngineView::astar_multi`]); backward bounds anchor on the seed set. Used for
    /// virtual starts, closing the last unidirectional-only production path.
    /// `extra.global` is NOT auto-seeded here; pass it as `seeds`.
    pub fn astar_bidir_multi(
        &self,
        seeds: &[(u32, f32)],
        bp: &BidirParams,
        params: SearchParams,
        ctx_f: &mut SearchContext,
        ctx_b: &mut SearchContext,
    ) -> SearchResult {
        self.bidir_core(None, seeds, bp, params, ctx_f, ctx_b)
    }

    fn bidir_core(
        &self,
        origin: Option<u32>,
        seeds: &[(u32, f32)],
        bp: &BidirParams,
        params: SearchParams,
        ctx_f: &mut SearchContext,
        ctx_b: &mut SearchContext,
    ) -> SearchResult {
        let n = self.nodes;
        let goal_id = params.goal;
        let goal = goal_id as usize;
        let bucket = params.bucket_ms;

        if origin == Some(goal_id) {
            return SearchResult { found: true, status: SearchStatus::Found, path: vec![goal_id], cost: 0.0, pops: 0, pops_f: 0, pops_b: 0 };
        }

        ctx_f.reset(n);
        ctx_b.reset(n);

        let active_f = self.lm.select_active(origin.unwrap_or(goal_id), goal_id, active_landmarks());
        let h_f = |u: u32| -> f32 { self.lm.h_active(u, &active_f) };

        // Backward bound anchors: the forward origins — start at 0 plus globals at
        // their cost, or (multi-source) the seed set itself.
        let mut anchors: Vec<(u32, f32)> = Vec::with_capacity(1 + self.extra.global.len() + seeds.len());
        if let Some(o) = origin {
            anchors.push((o, 0.0));
            for &(dst, w) in self.extra.global.iter() {
                if (dst as usize) < n {
                    anchors.push((dst, w));
                }
            }
        }
        for &(node, g0) in seeds {
            if (node as usize) < n {
                anchors.push((node, g0));
            }
        }
        let active_b = self.lm.select_active_rev(&anchors, goal_id, active_landmarks());
        let h_b = |u: u32| -> f32 { self.lm.h_active_rev(u, &active_b) };

        let mut mu = f32::INFINITY;
        let mut meet: Option<u32> = None;

        // --- forward seeding (mirrors the unidirectional path) ---
        if let Some(origin_id) = origin {
            let start = origin_id as usize;
            ctx_f.set_g(start, 0.0);
            ctx_f.set_parent(start, u32::MAX);
            let hs = ctx_f.h_cached(start, || h_f(origin_id));
            if hs.is_finite() {
                ctx_f.open.push(Key::new_bucketed(hs.max(0.0), 0.0, origin_id, bucket));
            }
            for &(dst, w) in self.extra.global.iter() {
                if dst as usize >= n { continue; }
                let w_j = match params.seed {
                    Some(seed) => w + edge_jitter(seed, origin_id, dst),
                    None => w,
                };
                if w_j < ctx_f.get_g(dst as usize) {
                    ctx_f.set_g(dst as usize, w_j);
                    ctx_f.set_parent(dst as usize, origin_id);
                    let hv = ctx_f.h_cached(dst as usize, || h_f(dst));
                    if hv.is_finite() {
                        ctx_f.open.push(Key::new_bucketed((w_j + hv).max(2.0 * w_j), w_j, dst, bucket));
                    }
                }
            }
        }
        for &(node, g0) in seeds {
            if node as usize >= n { continue; }
            if g0 < ctx_f.get_g(node as usize) {
                ctx_f.set_g(node as usize, g0);
                ctx_f.set_parent(node as usize, u32::MAX);
                let hv = ctx_f.h_cached(node as usize, || h_f(node));
                if hv.is_finite() {
                    ctx_f.open.push(Key::new_bucketed((g0 + hv).max(2.0 * g0), g0, node, bucket));
                }
            }
        }

        // --- backward seeding ---
        ctx_b.set_g(goal, 0.0);
        ctx_b.set_parent(goal, u32::MAX);
        let hg = ctx_b.h_cached(goal, || h_b(goal_id));
        if hg.is_finite() {
            ctx_b.open.push(Key::new_bucketed(hg.max(0.0), 0.0, goal_id, bucket));
        }
        // A forward seed IS a meeting candidate if it is the goal itself.
        if ctx_f.get_g(goal).is_finite() {
            mu = ctx_f.get_g(goal);
            meet = Some(goal_id);
        }

        let fairy_cost_of = |x: u32| -> Option<f32> {
            self.extra
                .fairy_dests
                .binary_search_by(|probe| probe.0.cmp(&x))
                .ok()
                .map(|i| self.extra.fairy_dests[i].1)
        };

        let mut pops: u32 = 0;
        let mut pops_f: u32 = 0;
        let mut pops_b: u32 = 0;
        let canon = canonical_ctx(self, params.seed);
        let diag_w = walk_diagonal_ms();
        let max_pops = params.max_pops.unwrap_or(u32::MAX);
        let mut ended: Option<SearchStatus> = None;

        loop {
            let tf = ctx_f.open.peek().map(|k| k.f_lower(bucket)).unwrap_or(f32::INFINITY);
            let tb = ctx_b.open.peek().map(|k| k.f_lower(bucket)).unwrap_or(f32::INFINITY);
            // MM stopping rule: pr is a lower bound on the cost of any solution still
            // passing through that side's frontier, so mu <= min proves optimality.
            if mu <= tf.min(tb) {
                break;
            }

            let forward = tf <= tb;
            let Some(Key { g: gcur, id, .. }) =
                (if forward { ctx_f.open.pop() } else { ctx_b.open.pop() })
            else {
                break;
            };
            // Overlap the chosen side's NEXT stale-check load with this expansion.
            if forward {
                if let Some(k) = ctx_f.open.peek() { prefetch_node(&ctx_f.nodes, k.id); }
            } else if let Some(k) = ctx_b.open.peek() {
                prefetch_node(&ctx_b.nodes, k.id);
            }
            let u = id as usize;
            // Lazy-deletion: an entry superseded by a better g is not an expansion, so it
            // must be skipped BEFORE the budget is charged — exactly as the unidirectional
            // loop does. Charging stale entries makes the effective budget shrink with heap
            // churn (which seed jitter inflates), so a reachable goal can hit
            // BudgetExceeded well under `max_pops` real expansions.
            let g_side = if forward { ctx_f.get_g(u) } else { ctx_b.get_g(u) };
            if gcur > g_side { continue; }

            pops += 1;
            if forward { pops_f += 1 } else { pops_b += 1 }
            if pops > max_pops { ended = Some(SearchStatus::BudgetExceeded); break; }
            if pops & 1023 == 0 {
                if let Some(c) = params.cancel {
                    if c.load(std::sync::atomic::Ordering::Relaxed) {
                        ended = Some(SearchStatus::Cancelled);
                        break;
                    }
                }
            }

            if forward {
                // ---- expand forward ----
                let fairy_slice: &[(u32, f32)] =
                    if !self.extra.fairy_sources.is_empty()
                        && self.extra.fairy_sources.binary_search(&id).is_ok()
                    { &self.extra.fairy_dests } else { &[] };

                // Fused NodeState access + push pruning: an entry whose pr exceeds the
                // (already-updated) meeting cost mu can never be expanded before the
                // MM stop rule fires — mu only decreases after the decision, so the
                // prune is pop-sequence-invariant, hence bit-exact.
                let mut relax = |v_id: u32, w: f32, ctx: &mut SearchContext, other: &SearchContext,
                             mu: &mut f32, meet: &mut Option<u32>| {
                    let v = v_id as usize;
                    let w_j = match params.seed {
                        Some(seed) => w + edge_jitter(seed, id, v_id),
                        None => w,
                    };
                    let ng = gcur + w_j;
                    let gen = ctx.generation;
                    let st = &mut ctx.nodes[v];
                    let cur_g = if st.gen == gen { st.g } else { f32::INFINITY };
                    if ng < cur_g {
                        if st.gen != gen {
                            st.gen = gen;
                            st.h = f32::NAN;
                        }
                        st.g = ng;
                        st.parent = id;
                        let og = other.get_g(v);
                        if og.is_finite() && ng + og < *mu {
                            *mu = ng + og;
                            *meet = Some(v_id);
                        }
                        let hv = if st.h.is_nan() {
                            let hh = h_f(v_id);
                            st.h = hh;
                            hh
                        } else {
                            st.h
                        };
                        if hv.is_finite() {
                            let pr = (ng + hv).max(2.0 * ng);
                            if pr <= *mu {
                                ctx.open.push(Key::new_bucketed(pr, ng, v_id, bucket));
                            }
                        }
                    }
                };
                if let Some((cg, coords, offsets, dst)) = canon {
                    let parent = ctx_f.nodes[u].parent;
                    let mask = cg.masks[u];
                    let mut bits = cg.succ_bits(id, parent, coords) & mask;
                    let s0 = offsets[u] as usize;
                    while bits != 0 {
                        let d = bits.trailing_zeros() as usize;
                        bits &= bits - 1;
                        let slot = s0 + (mask & ((1u8 << d) - 1)).count_ones() as usize;
                        let w = if d < 4 { WALK_CARDINAL_MS } else { diag_w };
                        relax(dst[slot], w, ctx_f, ctx_b, &mut mu, &mut meet);
                    }
                } else {
                    for &d in self.walk.neighbor_ids(id) {
                        prefetch_node(&ctx_f.nodes, d);
                    }
                    self.walk.for_each_neighbor(id, |v_id, w| {
                        relax(v_id, w, ctx_f, ctx_b, &mut mu, &mut meet)
                    });
                }
                if self.macros.has_macro(id) {
                    for (v_id, w) in self.macros.macro_neighbors(id, params.macro_filter) {
                        relax(v_id, w, ctx_f, ctx_b, &mut mu, &mut meet);
                    }
                }
                for &(v_id, w) in fairy_slice {
                    if v_id != id {
                        relax(v_id, w, ctx_f, ctx_b, &mut mu, &mut meet);
                    }
                }
            } else {
                // ---- expand backward (over reversed edges) ----
                // Backward fairy: predecessors of ring x are all other rings, each via
                // the forward edge y->x whose weight is cost(x).
                let fairy_pred_w: Option<f32> =
                    if !self.extra.fairy_sources.is_empty()
                        && self.extra.fairy_sources.binary_search(&id).is_ok()
                    { fairy_cost_of(id) } else { None };

                let mut relax = |y_id: u32, w: f32, ctx: &mut SearchContext, other: &SearchContext,
                             mu: &mut f32, meet: &mut Option<u32>| {
                    let y = y_id as usize;
                    // Forward edge identity is (y -> id): jitter must match the forward
                    // search's pricing of the same physical edge.
                    let w_j = match params.seed {
                        Some(seed) => w + edge_jitter(seed, y_id, id),
                        None => w,
                    };
                    let ng = gcur + w_j;
                    let gen = ctx.generation;
                    let st = &mut ctx.nodes[y];
                    let cur_g = if st.gen == gen { st.g } else { f32::INFINITY };
                    if ng < cur_g {
                        if st.gen != gen {
                            st.gen = gen;
                            st.h = f32::NAN;
                        }
                        st.g = ng;
                        st.parent = id;
                        let og = other.get_g(y);
                        if og.is_finite() && ng + og < *mu {
                            *mu = ng + og;
                            *meet = Some(y_id);
                        }
                        let hv = if st.h.is_nan() {
                            let hh = h_b(y_id);
                            st.h = hh;
                            hh
                        } else {
                            st.h
                        };
                        if hv.is_finite() {
                            let pr = (ng + hv).max(2.0 * ng);
                            if pr <= *mu {
                                ctx.open.push(Key::new_bucketed(pr, ng, y_id, bucket));
                            }
                        }
                    }
                };
                // Walk graph is symmetric (asserted at build time): forward neighbor
                // slice doubles as predecessor slice with identical weights.
                if let Some((cg, coords, offsets, dst)) = canon {
                    // Walk symmetry (builder-asserted) makes the same table valid for
                    // the reversed graph: predecessors == successors, direction taken
                    // from the backward parent exactly as forward.
                    let parent = ctx_b.nodes[u].parent;
                    let mask = cg.masks[u];
                    let mut bits = cg.succ_bits(id, parent, coords) & mask;
                    let s0 = offsets[u] as usize;
                    while bits != 0 {
                        let d = bits.trailing_zeros() as usize;
                        bits &= bits - 1;
                        let slot = s0 + (mask & ((1u8 << d) - 1)).count_ones() as usize;
                        let w = if d < 4 { WALK_CARDINAL_MS } else { diag_w };
                        relax(dst[slot], w, ctx_b, ctx_f, &mut mu, &mut meet);
                    }
                } else {
                    for &d in self.walk.neighbor_ids(id) {
                        prefetch_node(&ctx_b.nodes, d);
                    }
                    self.walk.for_each_neighbor(id, |y_id, w| {
                        relax(y_id, w, ctx_b, ctx_f, &mut mu, &mut meet)
                    });
                }
                if bp.macros_rev.has_macro(id) {
                    for (y_id, w) in bp.macros_rev.macro_neighbors(id, bp.macro_filter_rev) {
                        relax(y_id, w, ctx_b, ctx_f, &mut mu, &mut meet);
                    }
                }
                if let Some(wx) = fairy_pred_w {
                    for &(y_id, _) in self.extra.fairy_dests.iter() {
                        if y_id != id {
                            relax(y_id, wx, ctx_b, ctx_f, &mut mu, &mut meet);
                        }
                    }
                }
            }
        }

        let Some(m) = meet else {
            return SearchResult::not_found(ended.unwrap_or(SearchStatus::NotFound), pops_f, pops_b);
        };

        // Reconstruct origin/seed->meet from forward parents, meet->goal from backward
        // parents. Multi-source paths start at the winning seed (parent == u32::MAX).
        let mut path = Vec::new();
        let mut cur = m;
        loop {
            path.push(cur);
            if origin == Some(cur) { break; }
            let p = ctx_f.get_parent(cur as usize);
            if p == u32::MAX { break; }
            cur = p;
        }
        path.reverse();
        let mut cur = m;
        loop {
            let p = ctx_b.get_parent(cur as usize);
            if p == u32::MAX { break; }
            path.push(p);
            cur = p;
        }
        let cost = ctx_f.get_g(m as usize) + ctx_b.get_g(m as usize);
        // MM records mu/meet at FIRST frontier contact (or immediately, when a global
        // teleport seed IS the goal) but proves optimality only at the stop rule; a
        // budget/cancel break inside that window leaves a valid-but-unproven meeting
        // path. Surface the truncation instead of reporting Found.
        let status = ended.unwrap_or(SearchStatus::Found);
        SearchResult { found: true, status, path, cost, pops, pops_f, pops_b }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Distinct expansions `bidir_budget_charges_expansions_not_stale_pops` performs.
    const EXPECTED_BIDIR_POPS: u32 = 20;

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
        let res = view.astar(SearchParams { start: 0, goal: 2, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 }, &mut ctx);
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
        let res = view.astar(SearchParams { start: 0, goal: 3, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 }, &mut ctx);
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
        let res = view.astar(SearchParams { start: 0, goal: 3, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 }, &mut ctx);
        assert!(res.found);
        assert_eq!(res.path, vec![0, 1, 3]);
        assert!((res.cost - 1.25).abs() < 1e-6);
        // ...but a non-source node cannot hop: from 0 straight to 3 is impossible
        // without passing through the ring at 1.
        let res2 = view.astar(SearchParams { start: 2, goal: 3, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 }, &mut ctx);
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
        let params = SearchParams { start: 0, goal: 2, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 };
        let res = view.astar_multi(&[(0, 5.0), (3, 1.0)], params, &mut ctx);
        assert!(res.found);
        assert_eq!(res.path, vec![3, 2]);
        assert!((res.cost - 2.0).abs() < 1e-6);
    }

    #[test]
    fn multi_source_uses_fairy_hops() {
        // Virtual-start shape: the search enters at seed node 0 (g=2.0) and the goal 3
        // is reachable only via the fairy hop at node 1. Guards the adapter wiring bug
        // where virtual-start searches never populated the fairy extras at all.
        let walk_src = [0u32, 1];
        let walk_dst = [1u32, 2];
        let walk_w = [1.0f32, 1.0];
        let mut view = line_view(&walk_src, &walk_dst, &walk_w, &[], &[], &[], 4);
        view.extra.fairy_sources = vec![1, 3];
        view.extra.fairy_dests = vec![(1, 0.25), (3, 0.25)];
        let mut ctx = SearchContext::new(4);
        let params = SearchParams { start: 3, goal: 3, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 };
        let res = view.astar_multi(&[(0, 2.0)], params, &mut ctx);
        assert!(res.found);
        assert_eq!(res.path, vec![0, 1, 3]);
        assert!((res.cost - 3.25).abs() < 1e-6, "cost {}", res.cost);
    }

    fn bidir_run(
        view: &EngineView,
        start: u32,
        goal: u32,
        nodes: usize,
        seed: Option<u64>,
        max_pops: Option<u32>,
    ) -> SearchResult {
        // Reversed macro provider: swap src/dst of the view's macro CSR.
        let mut rs = Vec::new();
        let mut rd = Vec::new();
        let mut rw = Vec::new();
        for u in 0..nodes as u32 {
            for (v, w) in view.macros.macro_neighbors(u, None) {
                rs.push(v);
                rd.push(u);
                rw.push(w);
            }
        }
        let macros_rev = NeighborProvider::new(nodes, &rs, &rd, &rw);
        let bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: None };
        let mut cf = SearchContext::new(nodes);
        let mut cb = SearchContext::new(nodes);
        view.astar_bidir(&bp, SearchParams { start, goal, macro_filter: None, seed, max_pops, cancel: None, bucket_ms: 0.0 }, &mut cf, &mut cb)
    }

    fn bidir_of(view: &EngineView, start: u32, goal: u32, nodes: usize) -> SearchResult {
        bidir_run(view, start, goal, nodes, None, None)
    }

    #[test]
    fn bidir_matches_unidir_on_line_with_macro() {
        // Symmetric walk line 0..4 plus a directed macro shortcut 0->3.
        let mut ws = Vec::new(); let mut wd = Vec::new(); let mut ww = Vec::new();
        for (a, b) in [(0u32,1u32),(1,2),(2,3),(3,4)] {
            ws.push(a); wd.push(b); ww.push(1.0);
            ws.push(b); wd.push(a); ww.push(1.0);
        }
        let ms = [0u32]; let md = [3u32]; let mw = [1.5f32];
        let view = line_view(&ws, &wd, &ww, &ms, &md, &mw, 5);
        let mut ctx = SearchContext::new(5);
        for (s, g) in [(0u32, 4u32), (4, 0), (1, 3), (0, 3)] {
            let uni = view.astar(SearchParams { start: s, goal: g, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 }, &mut ctx);
            let bi = bidir_of(&view, s, g, 5);
            assert_eq!(uni.found, bi.found, "{s}->{g}");
            assert!((uni.cost - bi.cost).abs() < 1e-4, "{s}->{g}: uni {} vs bidir {}", uni.cost, bi.cost);
        }
        // 4->0 must NOT use the directed macro (it only exists 0->3).
        let bi = bidir_of(&view, 4, 0, 5);
        assert!((bi.cost - 4.0).abs() < 1e-4);
    }

    #[test]
    fn bidir_uses_global_seeds_and_fairy() {
        // 0 -1- 1 -1- 2 -1- 3 (symmetric), global teleport to 3 at 1.5, fairy 1<->3.
        let mut ws = Vec::new(); let mut wd = Vec::new(); let mut ww = Vec::new();
        for (a, b) in [(0u32,1u32),(1,2),(2,3)] {
            ws.push(a); wd.push(b); ww.push(1.0);
            ws.push(b); wd.push(a); ww.push(1.0);
        }
        let mut view = line_view(&ws, &wd, &ww, &[], &[], &[], 4);
        view.extra.global = vec![(3, 1.5)];
        view.extra.fairy_sources = vec![1, 3];
        view.extra.fairy_dests = vec![(1, 0.25), (3, 0.25)];
        let mut ctx = SearchContext::new(4);
        for (s, g) in [(0u32, 3u32), (0, 2), (3, 0), (2, 0)] {
            let uni = view.astar(SearchParams { start: s, goal: g, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 }, &mut ctx);
            let bi = bidir_of(&view, s, g, 4);
            assert_eq!(uni.found, bi.found, "{s}->{g}");
            assert!((uni.cost - bi.cost).abs() < 1e-4, "{s}->{g}: uni {} vs bidir {}", uni.cost, bi.cost);
        }
        // Path sanity for 0->3: teleport (1.5) beats walk (3.0) and fairy (1+0.25).
        let bi = bidir_of(&view, 0, 3, 4);
        assert!((bi.cost - 1.25).abs() < 1e-4, "cost {}", bi.cost);
        assert_eq!(bi.path, vec![0, 1, 3]);
    }

    #[test]
    fn bucketed_tiebreak_bounded_suboptimality() {
        // Diamond with near-equal paths: 0-1-3 costs 2.0, 0-2-3 costs 2.4. With a
        // 5.0 bucket both paths share a bucket, so the high-g dive may return either —
        // the served cost must stay within one bucket of the optimum (and exact mode
        // must return the optimum exactly). Checks uni and bidir.
        let (mut ws, mut wd, mut ww) = (Vec::new(), Vec::new(), Vec::new());
        for (a, b, w) in [(0u32, 1u32, 1.0f32), (1, 3, 1.0), (0, 2, 1.2), (2, 3, 1.2)] {
            ws.push(a); wd.push(b); ww.push(w);
            ws.push(b); wd.push(a); ww.push(w);
        }
        let view = line_view(&ws, &wd, &ww, &[], &[], &[], 4);
        let mut ctx = SearchContext::new(4);
        let params = |bucket: f32| SearchParams {
            start: 0, goal: 3, macro_filter: None, seed: None, max_pops: None, cancel: None,
            bucket_ms: bucket,
        };
        let exact = view.astar(params(0.0), &mut ctx);
        assert!((exact.cost - 2.0).abs() < 1e-6);
        let bucketed = view.astar(params(5.0), &mut ctx);
        assert!(bucketed.found);
        assert!(
            bucketed.cost >= exact.cost - 1e-6 && bucketed.cost <= exact.cost + 5.0 + 1e-6,
            "bucketed cost {} outside [optimum, optimum + bucket]", bucketed.cost
        );

        let macros_rev = NeighborProvider::new(4, &[], &[], &[]);
        let bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: None };
        let mut cf = SearchContext::new(4);
        let mut cb = SearchContext::new(4);
        let bi = view.astar_bidir(&bp, params(5.0), &mut cf, &mut cb);
        assert!(bi.found);
        assert!(
            bi.cost >= exact.cost - 1e-6 && bi.cost <= exact.cost + 5.0 + 1e-6,
            "bidir bucketed cost {} outside bound", bi.cost
        );
    }

    #[test]
    fn bidir_multi_matches_astar_multi() {
        // Symmetric chains: 0-1-2 and 3-2. Seeds: node 0 at g=5, node 3 at g=1; best
        // total is via 3 (1 + 1 = 2) and the path must start at the winning seed —
        // exactly the astar_multi contract, now bidirectional.
        let (mut ws, mut wd, mut ww) = (Vec::new(), Vec::new(), Vec::new());
        for (a, b) in [(0u32, 1u32), (1, 2), (3, 2)] {
            ws.push(a); wd.push(b); ww.push(1.0);
            ws.push(b); wd.push(a); ww.push(1.0);
        }
        let view = line_view(&ws, &wd, &ww, &[], &[], &[], 4);
        let seeds = [(0u32, 5.0f32), (3, 1.0)];
        let params = || SearchParams { start: 2, goal: 2, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 };

        let mut ctx = SearchContext::new(4);
        let uni = view.astar_multi(&seeds, params(), &mut ctx);

        let macros_rev = NeighborProvider::new(4, &[], &[], &[]);
        let bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: None };
        let mut cf = SearchContext::new(4);
        let mut cb = SearchContext::new(4);
        let bi = view.astar_bidir_multi(&seeds, &bp, params(), &mut cf, &mut cb);

        assert!(uni.found && bi.found);
        assert!((uni.cost - bi.cost).abs() < 1e-4, "uni {} vs bidir {}", uni.cost, bi.cost);
        assert_eq!(bi.path.first(), Some(&3), "path must start at the winning seed");
        assert_eq!(bi.path.last(), Some(&2));
        // A seed that IS the goal must win outright when cheapest.
        let bi2 = view.astar_bidir_multi(&[(2, 0.5)], &bp, params(), &mut cf, &mut cb);
        assert!(bi2.found);
        assert!((bi2.cost - 0.5).abs() < 1e-6, "cost {}", bi2.cost);
    }

    #[test]
    fn bidir_multi_uses_fairy_hops() {
        // Virtual-start shape with fairy: seed at 0 (g=2), goal 3 reachable only via
        // the fairy hop at 1 (symmetric walk 0-1-2; fairy 1<->3).
        let (mut ws, mut wd, mut ww) = (Vec::new(), Vec::new(), Vec::new());
        for (a, b) in [(0u32, 1u32), (1, 2)] {
            ws.push(a); wd.push(b); ww.push(1.0);
            ws.push(b); wd.push(a); ww.push(1.0);
        }
        let mut view = line_view(&ws, &wd, &ww, &[], &[], &[], 4);
        view.extra.fairy_sources = vec![1, 3];
        view.extra.fairy_dests = vec![(1, 0.25), (3, 0.25)];
        let macros_rev = NeighborProvider::new(4, &[], &[], &[]);
        let bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: None };
        let mut cf = SearchContext::new(4);
        let mut cb = SearchContext::new(4);
        let params = SearchParams { start: 3, goal: 3, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 };
        let res = view.astar_bidir_multi(&[(0, 2.0)], &bp, params, &mut cf, &mut cb);
        assert!(res.found);
        assert_eq!(res.path, vec![0, 1, 3]);
        assert!((res.cost - 3.25).abs() < 1e-6, "cost {}", res.cost);
    }

    #[test]
    fn bidir_budget_charges_expansions_not_stale_pops() {
        // Node 2 is first reached at g=5 (direct 0-2 edge), then improved to g=2 (via 1),
        // leaving a stale heap entry at g=5 (pr=10). The 2..20 tail keeps the meeting cost
        // mu (=20) above that pr, so MM really does pop the stale entry. A stale pop is a
        // skip, not an expansion, and must not be charged against the budget: charging it
        // shrinks the effective budget by however much heap churn a query happens to
        // produce (seed jitter multiplies exactly that), which is how a reachable goal
        // ends up reported as BudgetExceeded.
        let mut edges: Vec<(u32, u32, f32)> = vec![(0, 1, 1.0), (1, 2, 1.0), (0, 2, 5.0)];
        for a in 2..20u32 {
            edges.push((a, a + 1, 1.0));
        }
        let (mut ws, mut wd, mut ww) = (Vec::new(), Vec::new(), Vec::new());
        for (a, b, w) in edges {
            ws.push(a); wd.push(b); ww.push(w);
            ws.push(b); wd.push(a); ww.push(w);
        }
        let view = line_view(&ws, &wd, &ww, &[], &[], &[], 21);

        let res = bidir_of(&view, 0, 20, 21);
        assert!(res.found);
        assert!((res.cost - 20.0).abs() < 1e-4, "cost {}", res.cost);
        // Every node is expanded at most once per side here (h=0 is consistent), so the
        // charge is bounded by the distinct expansions the run performs. Measured on the
        // stale-free accounting; the pre-fix loop charged the stale pops on top.
        assert_eq!(res.pops, EXPECTED_BIDIR_POPS);

        // A budget of exactly that many expansions is therefore enough to find the route
        // and prove it optimal — no BudgetExceeded, no truncated path.
        let macros_rev = NeighborProvider::new(21, &[], &[], &[]);
        let bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: None };
        let mut cf = SearchContext::new(21);
        let mut cb = SearchContext::new(21);
        let budgeted = view.astar_bidir(
            &bp,
            SearchParams { start: 0, goal: 20, macro_filter: None, seed: None, max_pops: Some(EXPECTED_BIDIR_POPS), cancel: None, bucket_ms: 0.0 },
            &mut cf,
            &mut cb,
        );
        assert_eq!(budgeted.status, SearchStatus::Found);
        assert!((budgeted.cost - 20.0).abs() < 1e-4, "cost {}", budgeted.cost);
    }

    #[test]
    fn truncated_found_reports_budget_exceeded_not_found_status() {
        // Walk chain 0-1-2-3 costs 3.0; a global teleport seeds g(goal)=10.0 at pop
        // zero. A 1-pop budget truncates before the cheaper walk route is proven, so
        // the returned teleport path is valid but NOT optimal — the status must say
        // BudgetExceeded (found stays true), or callers cache the unproven cost and
        // the budget retry never fires.
        let walk_src = [0u32, 1, 2];
        let walk_dst = [1u32, 2, 3];
        let walk_w = [1.0f32, 1.0, 1.0];
        let mut view = line_view(&walk_src, &walk_dst, &walk_w, &[], &[], &[], 4);
        view.extra.global = vec![(3, 10.0)];
        let mut ctx = SearchContext::new(4);

        let truncated = view.astar(
            SearchParams { start: 0, goal: 3, macro_filter: None, seed: None, max_pops: Some(1), cancel: None, bucket_ms: 0.0 },
            &mut ctx,
        );
        assert!(truncated.found);
        assert_eq!(truncated.status, SearchStatus::BudgetExceeded);
        assert_eq!(truncated.path, vec![0, 3]);
        assert!((truncated.cost - 10.0).abs() < 1e-6, "cost {}", truncated.cost);

        // Unbudgeted, the walk route wins and the status is a genuine Found.
        let full = view.astar(
            SearchParams { start: 0, goal: 3, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 },
            &mut ctx,
        );
        assert_eq!(full.status, SearchStatus::Found);
        assert_eq!(full.path, vec![0, 1, 2, 3]);
        assert!((full.cost - 3.0).abs() < 1e-6, "cost {}", full.cost);
    }

    #[test]
    fn bidir_truncated_found_reports_budget_exceeded() {
        // Same shape, symmetric walk for bidir: the teleport seed IS the goal, so MM
        // records mu=10 before the first pop; a 1-pop budget breaks inside the
        // first-meet-to-proof window and must not report the unproven mu as Found.
        let (mut ws, mut wd, mut ww) = (Vec::new(), Vec::new(), Vec::new());
        for (a, b) in [(0u32, 1u32), (1, 2), (2, 3)] {
            ws.push(a); wd.push(b); ww.push(1.0);
            ws.push(b); wd.push(a); ww.push(1.0);
        }
        let mut view = line_view(&ws, &wd, &ww, &[], &[], &[], 4);
        view.extra.global = vec![(3, 10.0)];

        let truncated = {
            let macros_rev = NeighborProvider::new(4, &[], &[], &[]);
            let bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: None };
            let mut cf = SearchContext::new(4);
            let mut cb = SearchContext::new(4);
            view.astar_bidir(
                &bp,
                SearchParams { start: 0, goal: 3, macro_filter: None, seed: None, max_pops: Some(1), cancel: None, bucket_ms: 0.0 },
                &mut cf,
                &mut cb,
            )
        };
        assert!(truncated.found);
        assert_eq!(truncated.status, SearchStatus::BudgetExceeded);
        assert!((truncated.cost - 10.0).abs() < 1e-6, "cost {}", truncated.cost);

        let full = bidir_run(&view, 0, 3, 4, None, None);
        assert_eq!(full.status, SearchStatus::Found);
        assert!((full.cost - 3.0).abs() < 1e-6, "cost {}", full.cost);
    }

    #[test]
    fn seeded_uni_and_bidir_agree_and_are_deterministic() {
        // Symmetric diamond: 0-1-3 and 0-2-3, all edges 1.0 both ways. Base costs tie
        // at 2.0; per-seed jitter picks a unique winner, which uni and bidir must both
        // return (cost and path), deterministically across repeat runs. This pins the
        // load-bearing backward jitter identity — backward relaxations must price the
        // FORWARD edge (edge_jitter(seed, y, id)) — which no other test exercises.
        let (mut ws, mut wd, mut ww) = (Vec::new(), Vec::new(), Vec::new());
        for (a, b) in [(0u32, 1u32), (1, 3), (0, 2), (2, 3)] {
            ws.push(a); wd.push(b); ww.push(1.0);
            ws.push(b); wd.push(a); ww.push(1.0);
        }
        let view = line_view(&ws, &wd, &ww, &[], &[], &[], 4);
        let mut ctx = SearchContext::new(4);

        let mut winners: std::collections::HashSet<Vec<u32>> = std::collections::HashSet::new();
        for seed in [1u64, 2, 3, 5, 8, 13, 21, 42] {
            let top = 2.0 + edge_jitter(seed, 0, 1) + edge_jitter(seed, 1, 3);
            let bot = 2.0 + edge_jitter(seed, 0, 2) + edge_jitter(seed, 2, 3);
            let params = || SearchParams { start: 0, goal: 3, macro_filter: None, seed: Some(seed), max_pops: None, cancel: None, bucket_ms: 0.0 };
            let uni = view.astar(params(), &mut ctx);
            let uni2 = view.astar(params(), &mut ctx);
            let bi = bidir_run(&view, 0, 3, 4, Some(seed), None);
            assert!(uni.found && bi.found, "seed {seed}");
            assert_eq!(uni.cost.to_bits(), uni2.cost.to_bits(), "seed {seed}: nondeterministic cost");
            assert_eq!(uni.path, uni2.path, "seed {seed}: nondeterministic path");
            assert!(
                (uni.cost - bi.cost).abs() < 1e-4,
                "seed {seed}: uni {} vs bidir {}", uni.cost, bi.cost
            );
            if (top - bot).abs() > 1e-6 {
                let expected = if top < bot { vec![0u32, 1, 3] } else { vec![0u32, 2, 3] };
                assert_eq!(uni.path, expected, "seed {seed}: wrong jitter winner (uni)");
                assert_eq!(bi.path, expected, "seed {seed}: wrong jitter winner (bidir)");
            }
            winners.insert(uni.path.clone());
        }
        // The fixture's purpose: different seeds really do select different paths.
        assert!(winners.len() > 1, "every seed picked the same diamond path");
    }

    #[test]
    fn budget_exceeded_reports_distinct_status() {
        // Long chain, unreachable goal (node 5 disconnected).
        let walk_src = [0u32, 1, 2, 3];
        let walk_dst = [1u32, 2, 3, 4];
        let walk_w = [1.0f32, 1.0, 1.0, 1.0];
        let view = line_view(&walk_src, &walk_dst, &walk_w, &[], &[], &[], 6);
        let mut ctx = SearchContext::new(6);
        let res = view.astar(SearchParams { start: 0, goal: 5, macro_filter: None, seed: None, max_pops: Some(2), cancel: None, bucket_ms: 0.0 }, &mut ctx);
        assert!(!res.found);
        assert_eq!(res.status, SearchStatus::BudgetExceeded);
        // Without a budget the same query exhausts the heap and reports NotFound.
        let res2 = view.astar(SearchParams { start: 0, goal: 5, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 }, &mut ctx);
        assert!(!res2.found);
        assert_eq!(res2.status, SearchStatus::NotFound);
    }
}
