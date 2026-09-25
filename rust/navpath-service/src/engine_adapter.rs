use std::sync::Arc;
use std::sync::OnceLock;
use rustc_hash::FxHashMap;

use navpath_core::{EngineView, SearchParams, SearchResult, SearchStatus, Snapshot, NeighborProvider};
use navpath_core::engine::canonical::CanonicalGrid;
use navpath_core::engine::search::{ExtraEdges, SearchContext};
use navpath_core::engine::neighbors::{MacroFilter, WalkGraph};
use navpath_core::engine::search::BidirParams;

/// Empty "no path" result for early-exit paths in this module.
fn not_found_result() -> SearchResult {
    SearchResult { found: false, status: SearchStatus::NotFound, path: Vec::new(), path_g: Vec::new(), cost: f32::INFINITY, pops: 0, pops_f: 0, pops_b: 0 }
}
use serde_json::Value as JsonValue;
use navpath_core::eligibility::{fnv1a32, EligibilityMask};
use navpath_core::engine::heuristics::{active_landmarks, LandmarkHeuristic, RevAnchorBase};
use tracing::{info, warn};

/// Search-context source for the adapter entry points (T3.4). A unidirectional search
/// (plain, JPS, or multi-source virtual start) needs ONE node-sized context; only a
/// bidirectional search needs two. The service implements this on its pooled lease
/// ([`crate::PooledContexts`]), which checks each context out of the pool on first use
/// — so a unidirectional search never pins an idle second context. Harnesses pass a
/// plain `(SearchContext, SearchContext)` pair.
pub trait SearchContexts {
    /// The forward (or only) context.
    fn one(&mut self) -> &mut SearchContext;
    /// Forward and backward contexts for a bidirectional search.
    fn two(&mut self) -> (&mut SearchContext, &mut SearchContext);
}

impl SearchContexts for (SearchContext, SearchContext) {
    fn one(&mut self) -> &mut SearchContext {
        &mut self.0
    }
    fn two(&mut self) -> (&mut SearchContext, &mut SearchContext) {
        (&mut self.0, &mut self.1)
    }
}

/// Weak-backward demotion ratio (roadmap 4.2): a route runs bidirectional only when
/// the backward ALT bound at the goal is at least this fraction of the forward bound
/// of C*. With ~125 spread anchors (start + every eligible global) the backward
/// min-aggregates flatten map-wide on permissive profiles; MM then grinds a
/// reverse-Dijkstra ball of cost-radius ~C*/2 where a strong-h forward search runs a
/// corridor. Both engines are exact, so this only chooses which one runs.
/// `NAVPATH_BIDIR_MIN_HB_RATIO`, default 0 (always bidir) since 2026-09-17: the 0.5
/// demotion measured 1.3-3x slower on long walk routes over 300 random pairs
/// (docs/route_latency_improvements_2026-09-17.md §1.3). Set e.g. 0.5 to re-enable.
fn bidir_min_hb_ratio() -> f32 {
    static R: OnceLock<f32> = OnceLock::new();
    *R.get_or_init(|| {
        std::env::var("NAVPATH_BIDIR_MIN_HB_RATIO").ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .unwrap_or(0.0)
    })
}

/// Routes with a forward bound under this many ms skip the demotion analysis: they
/// search fast under either engine, and the analysis itself costs ~125 heuristic
/// evaluations plus one reverse selection.
const BIDIR_POLICY_MIN_H_MS: f32 = 20_000.0;

/// Opt-in plateau tie-break bucket (roadmap 3.4), `NAVPATH_TIEBREAK_BUCKET_MS`
/// (default 0 = off).
///
/// MEASURED HARMFUL on the deployed 64-landmark snapshot (2026-07-31, golden corpus,
/// median of 3, server-side `pops`, seeded requests — the only class it applies to by
/// default). The roadmap's "recommended operating point" of 128 makes seeded searches
/// 2-20x WORSE, on BOTH engines:
///   bidir:  readme_seeded_pair 130k -> 952k pops, quick_tele_route 41k -> 231k,
///           virtual_start_all 100k -> 305k, incident_pair_all 34k -> 68k
///   uni:    identical regressions (so this is not the MM stop rule reading
///           `Key::f_lower`'s bucket lower edge — that only compounds it)
/// The only routes that improve are ones already under 100 pops (cross_plane_up 86 -> 57,
/// short_lumbridge 30 -> 15), i.e. microseconds in absolute terms. Diving a 128 ms bucket
/// by high g commits the frontier to whichever corridor happens to be deepest, and on
/// full-width ALT bounds (roadmap 3.1, the current default) that guess is worse than the
/// exact ordering it replaces — the roadmap's own note that 3.1 "already took most of the
/// plateau" is the reason. Leave at 0; re-measure per snapshot before ever raising it.
///
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
    let jps = navpath_core::engine::search::jps_enabled();
    // The jump-point tie-pruned table (8 B/node plus fill time) is only built when JPS
    // can use it.
    match CanonicalGrid::build_opts(
        snapshot.counts().nodes as usize,
        snapshot.coords_packed(),
        snapshot.walk_offsets(),
        snapshot.walk_dst(),
        snapshot.macro_src(),
        snapshot.macro_dst(),
        snapshot.macro_w(),
        jps,
    ) {
        Ok(mut g) => {
            // Fairy rings carry non-grid edges too: jumps must stop on them.
            g.add_stop_nodes(snapshot.fairy_nodes());
            if jps {
                g.build_jump_tables(snapshot.walk_offsets(), snapshot.walk_dst(), snapshot.coords_packed());
            }
            info!(elapsed_ms = t.elapsed().as_millis() as u64, jps = navpath_core::engine::search::jps_enabled(), "built canonical pruning grid");
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
pub(crate) fn bidir_enabled() -> bool {
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
    /// Which engine produced `res`: "uni", "bidir", or "cache".
    pub engine: &'static str,
}

/// Which engine a route runs on. `Policy` is the shipped selection (bidirectional unless
/// `NAVPATH_BIDIR=0` or the `NAVPATH_BIDIR_MIN_HB_RATIO` demotion fires); `Uni` /
/// `Bidir` force one side for the hedged race (`NAVPATH_RACE=1`), where both engines
/// run concurrently and the first stable result wins. Both engines are exact, so the
/// choice never changes the served cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineChoice {
    Policy,
    Uni,
    Bidir,
}

/// Forward ALT bounds that predict which engine a route favours (T3.2b), computed the
/// way the weak-backward policy below computes them.
///
/// - Bidirectional search loses 3-5x on teleport-dominated routes: its backward bound
///   is anchored on the start plus every eligible global landing, so it is weak
///   everywhere near a landing and the backward side floods
///   (docs/route_latency_improvements §1.2). A route is teleport-dominated when some
///   eligible teleport's bound `w + h(dst)` undercuts the walk bound `h(start)`.
/// - The unidirectional engine (JPS in production) wins almost everything else, except
///   heuristic-blind routes: with `h = 0` (goal outside landmark coverage) it degrades to
///   a Dijkstra flood that bidirectional search halves by meeting in the middle.
///
/// Measured with `examples/race_sweep` (2026-09-25; 300-400 LCG pairs each, JPS on and
/// off, seeded, all-eligible and gated profiles): with a bidir primary, racing only
/// [`worth_racing`](Self::worth_racing) routes left latency unchanged (sum and p99) and
/// cut race CPU 4-16%; with a JPS primary, racing only blind routes (2-3% of pairs)
/// kept p99/max identical and the latency sum within 0-2% while cutting CPU 20-49%.
#[derive(Clone, Copy, Debug)]
pub struct RaceHint {
    /// `h(start)`; `INFINITY` for a virtual start (no on-graph origin to walk from).
    pub h_start: f32,
    /// `min over eligible globals (w + h(dst))`; `INFINITY` when none is eligible.
    pub h_teleport: f32,
    /// The forward heuristic is zero where the search begins: at the start, or (virtual
    /// start) at every eligible teleport landing.
    pub blind: bool,
}

impl RaceHint {
    pub fn teleport_dominated(&self) -> bool {
        self.h_teleport < self.h_start
    }

    /// Whether the route is worth hedging with the second engine, given which engine
    /// runs first. Bidirectional primary: teleport-dominated (where uni wins 3-5x), or
    /// long enough (`h(start)` at least [`BIDIR_POLICY_MIN_H_MS`]) that a slow pick
    /// costs milliseconds. Unidirectional primary: heuristic-blind routes only.
    pub fn worth_racing(&self, uni_primary: bool) -> bool {
        if uni_primary {
            self.blind
        } else {
            self.teleport_dominated() || self.h_start >= BIDIR_POLICY_MIN_H_MS
        }
    }
}

/// The snapshot's landmark heuristic (only the race hint builds one outside the
/// adapter entry points).
fn landmark_heuristic(snap: &Snapshot) -> LandmarkHeuristic<'_> {
    LandmarkHeuristic::from_snapshot(snap)
}

/// [`RaceHint`] for one request: one landmark selection plus ~125 forward heuristic
/// rows (the start and every eligible global landing) — microseconds.
pub fn race_hint(snapshot: &Snapshot, artifacts: &ProfileArtifacts, start: Option<u32>, goal: u32) -> RaceHint {
    let lm = landmark_heuristic(snapshot);
    let active = lm.select_active(start.unwrap_or(goal), goal, active_landmarks());
    let h_start = match start {
        Some(s) => lm.h_active(s, &active),
        None => f32::INFINITY,
    };
    let mut h_teleport = f32::INFINITY;
    let mut h_landing_max = 0.0f32;
    for &(dst, w) in artifacts.eligible_globals.iter() {
        let h = lm.h_active(dst, &active);
        h_teleport = h_teleport.min(w + h);
        h_landing_max = h_landing_max.max(h);
    }
    let blind = match start {
        Some(_) => h_start <= 0.0,
        None => h_landing_max <= 0.0,
    };
    RaceHint { h_start, h_teleport, blind }
}

/// Budget-retry ladder (roadmap 1.5). A `BudgetExceeded` first attempt earns:
///   1. a retry with the SAME seed at the escalated cap — jitter-inflated pop counts
///      usually fit 4x, and the client's path-variety contract survives;
///   2. only if that also gives up: an unseeded retry (jitter exists to vary
///      otherwise-equal paths; a real route is worth more than that variety), with the
///      served result marked `seed_dropped`.
/// Every rung shares the request deadline/cancel flag, which remains the real ceiling.
///
/// Rung 1 CONTINUES the stopped search (`search(.., resume = true)`, see
/// `EngineView::astar_resume`) instead of re-running it from scratch: the search is
/// deterministic, so a fresh rerun used to replay the first `default_max_pops` pops
/// identically before doing anything new (a 1.6M-pop route paid 3.1M). The
/// continuation is bit-identical to a fresh run with the larger budget, and its pop
/// counters are cumulative; `attempts_pops[1]` reports only the new work. Rung 2 changes
/// the pricing (seed dropped), so it always starts fresh.
fn retry_ladder(
    first: SearchResult,
    seed: Option<u64>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    nodes: usize,
    engine: &'static str,
    mut search: impl FnMut(Option<u64>, Option<u32>, bool) -> SearchResult,
) -> SearchOutcome {
    let Some(retry_pops) = budget_retry_pops(&first, seed, cancel, nodes) else {
        return SearchOutcome { attempts_pops: [first.pops, 0, 0], retried: false, seed_dropped: false, engine, res: first };
    };
    warn!(
        pops = first.pops, found = first.found, retry_pops, seeded = seed.is_some(),
        "search exhausted its pop budget; continuing with an escalated budget"
    );
    let second = search(seed, Some(retry_pops), true);
    let mut attempts_pops = [first.pops, second.pops.saturating_sub(first.pops), 0];
    if seed.is_none() || second.status == SearchStatus::Found {
        return SearchOutcome { attempts_pops, retried: true, seed_dropped: false, engine, res: better_of(first, second) };
    }
    let best2 = better_of(first, second);
    if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
        return SearchOutcome { attempts_pops, retried: true, seed_dropped: false, engine, res: best2 };
    }
    warn!(retry_pops, "seeded retry also exhausted its budget; dropping the seed");
    let third = search(None, Some(retry_pops), false);
    attempts_pops[2] = third.pops;
    if third.status == SearchStatus::Found || (third.found && (!best2.found || third.cost < best2.cost)) {
        return SearchOutcome { attempts_pops, retried: true, seed_dropped: true, engine, res: third };
    }
    if !best2.found {
        // No rung found a path; the deeper search's verdict (e.g. a heap-exhausting
        // NotFound) is the most truthful one.
        return SearchOutcome { attempts_pops, retried: true, seed_dropped: false, engine, res: better_of(best2, third) };
    }
    SearchOutcome { attempts_pops, retried: true, seed_dropped: false, engine, res: best2 }
}

/// Choose which of (first attempt, budget retry) to serve. A proven result always wins;
/// otherwise a discovered path — even a truncated one — beats no path, and between two
/// truncated paths the cheaper wins. The retry is unseeded and proves optimality on the
/// base graph, so a `Found` retry costs no more than any truncated path (jitter only
/// ever adds to edge weights).
pub(crate) fn better_of(first: SearchResult, retry: SearchResult) -> SearchResult {
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

/// `macro_lookup` supplies each edge's requirement list, decoded once at load by
/// [`build_neighbor_provider`] (the metadata used to be parsed a second time here).
pub fn build_component_graph(
    snapshot: &Snapshot,
    globals: &[GlobalTeleport],
    fairy_rings: &[FairyRing],
    macro_lookup: &MacroLookup,
) -> ComponentGraph {
    let comp = snapshot.comp_ids();
    let components = snapshot.counts().walk_components as usize;
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
        // Same fail-closed requirement decoding as the search setup: unknown ids map to
        // usize::MAX, which no mask satisfies.
        let reqs = macro_lookup.edge_reqs(idx).to_vec();
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
///
/// Convenience form that builds the profile's [`ProfileReach`] on the spot; the service
/// resolves it once per profile via [`ProfileArtifacts::reach`] instead.
pub fn goal_reachable(
    cg: &ComponentGraph,
    mask: &EligibilityMask,
    start_comp: Option<u16>,
    goal_comp: u16,
) -> bool {
    ProfileReach::build(cg, mask).reachable(start_comp, goal_comp)
}

/// One profile's view of the [`ComponentGraph`] (T3.14): the eligible macro edges as a
/// deduplicated component CSR, the eligible fairy-ring components, and the components
/// eligible global teleports land in. Everything the per-request precheck used to
/// re-filter from the full edge list (3 Vec allocations plus fixpoint sweeps over every
/// edge until nothing changed) is decided once per profile; a request then runs ONE
/// graph search over a few hundred components.
pub struct ProfileReach {
    /// Slots in the bitsets / CSR: every component id any edge, ring or global names
    /// (normally the snapshot's walk-component count).
    n: usize,
    offsets: Vec<u32>,
    succ: Vec<u16>,
    /// Eligible ring components, deduplicated. Eligible rings form a clique: reaching any
    /// of them reaches all of them.
    rings: Vec<u16>,
    ring_bits: Vec<u64>,
    /// Components eligible global teleports land in (origin seeds), deduplicated.
    globals: Vec<u16>,
}

impl ProfileReach {
    pub fn build(cg: &ComponentGraph, mask: &EligibilityMask) -> Self {
        let eligible = |reqs: &[usize]| reqs.iter().all(|&i| mask.is_satisfied(i));
        let mut n = cg.components;
        let mut edges: Vec<(u16, u16)> = Vec::new();
        for (s, d, reqs) in &cg.macro_edges {
            if eligible(reqs) {
                n = n.max(*s as usize + 1).max(*d as usize + 1);
                edges.push((*s, *d));
            }
        }
        edges.sort_unstable();
        edges.dedup();
        let mut rings: Vec<u16> = cg.fairy.iter().filter(|(_, r)| eligible(r)).map(|&(c, _)| c).collect();
        rings.sort_unstable();
        rings.dedup();
        let mut globals: Vec<u16> = cg.globals.iter().filter(|(_, r)| eligible(r)).map(|&(c, _)| c).collect();
        globals.sort_unstable();
        globals.dedup();
        for &c in rings.iter().chain(globals.iter()) {
            n = n.max(c as usize + 1);
        }
        let mut offsets = vec![0u32; n + 1];
        for &(s, _) in &edges {
            offsets[s as usize + 1] += 1;
        }
        for i in 0..n {
            offsets[i + 1] += offsets[i];
        }
        // `edges` is sorted by source, so the successor list fills in CSR order.
        let succ: Vec<u16> = edges.iter().map(|&(_, d)| d).collect();
        let mut ring_bits = vec![0u64; n.div_ceil(64)];
        for &c in &rings {
            ring_bits[c as usize / 64] |= 1u64 << (c % 64);
        }
        ProfileReach { n, offsets, succ, rings, ring_bits, globals }
    }

    /// Exact reachability of `goal_comp` from `start_comp` (None = virtual start: the
    /// origin enters the world only through eligible globals). Same closure as the
    /// original fixpoint: seeds = start + eligible global landings, closed under
    /// eligible macro edges and the eligible-ring clique.
    pub fn reachable(&self, start_comp: Option<u16>, goal_comp: u16) -> bool {
        let g = goal_comp as usize;
        if g >= self.n {
            // No edge, ring or global touches this component: only a start already
            // inside it reaches it.
            return start_comp == Some(goal_comp);
        }
        // A few hundred components: a stack-sized bitset covers the common case.
        let words = self.n.div_ceil(64);
        let mut inline = [0u64; 16];
        let mut heap: Vec<u64>;
        let seen: &mut [u64] = if words <= inline.len() {
            &mut inline[..words]
        } else {
            heap = vec![0u64; words];
            &mut heap
        };
        let mut stack: Vec<u16> = Vec::with_capacity(32);
        fn visit(c: u16, seen: &mut [u64], stack: &mut Vec<u16>) {
            let (w, b) = (c as usize / 64, 1u64 << (c % 64));
            if seen[w] & b == 0 {
                seen[w] |= b;
                stack.push(c);
            }
        }
        if let Some(sc) = start_comp {
            if (sc as usize) < self.n {
                visit(sc, seen, &mut stack);
            }
        }
        for &c in &self.globals {
            visit(c, seen, &mut stack);
        }
        let mut rings_joined = false;
        while let Some(c) = stack.pop() {
            if c as usize == g {
                return true;
            }
            if !rings_joined && self.ring_bits[c as usize / 64] & (1u64 << (c % 64)) != 0 {
                rings_joined = true;
                for &r in &self.rings {
                    visit(r, seen, &mut stack);
                }
            }
            let (s, e) = (self.offsets[c as usize] as usize, self.offsets[c as usize + 1] as usize);
            for &d in &self.succ[s..e] {
                visit(d, seen, &mut stack);
            }
        }
        seen[g / 64] & (1u64 << (g % 64)) != 0
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
    /// This profile's reachability view of the snapshot's [`ComponentGraph`], built on
    /// the first precheck that needs it (see [`ProfileArtifacts::reach`]).
    pub reach: OnceLock<ProfileReach>,
    /// Backward-heuristic aggregate of the eligible globals (the anchor set every
    /// bidirectional search of this profile folds its origin into), built on first use
    /// (see [`ProfileArtifacts::rev_base`]).
    pub rev_base: OnceLock<RevAnchorBase>,
}

impl ProfileArtifacts {
    /// The profile's [`ProfileReach`], built once. `mask` must be the mask these
    /// artifacts were built for (the profile cache is keyed on its exact bits, and both
    /// live per snapshot, like `cg`).
    pub fn reach(&self, cg: &ComponentGraph, mask: &EligibilityMask) -> &ProfileReach {
        self.reach.get_or_init(|| ProfileReach::build(cg, mask))
    }

    /// The eligible globals' backward-landmark aggregate over `lm` (the snapshot these
    /// artifacts belong to), built once: bidirectional searches then fold only their own
    /// origin into it instead of re-aggregating ~125 anchors x every landmark per
    /// search (and per retry rung / race arm). The engine uses it only when its
    /// fingerprint matches the anchor list it would aggregate, so results are
    /// bit-identical either way.
    pub fn rev_base(&self, lm: &LandmarkHeuristic) -> &RevAnchorBase {
        self.rev_base.get_or_init(|| {
            let anchors: Vec<(u32, f32)> =
                self.eligible_globals.iter().copied().filter(|&(d, _)| (d as usize) < lm.nodes).collect();
            lm.rev_base(&anchors)
        })
    }
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
        reach: OnceLock::new(),
        rev_base: OnceLock::new(),
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
        "poa_item" => 7,
        "use_on" => 8,
        _ => 0,
    }
}

/// Per-edge macro metadata as the payload builder consumes it, decoded ONCE at load
/// (T3.7/T3.15: the payload used to `serde_json::from_slice` every parallel candidate
/// edge on every payload build — cache hits included — and the component graph parsed
/// the whole blob a second time).
pub struct MacroEdgeInfo {
    /// Requirement tag indices from the metadata's top-level `requirements` array
    /// (integer entries only; unknown ids decode to `usize::MAX`, which no mask
    /// satisfies — fail-closed). Empty when the metadata is missing or unparseable,
    /// i.e. the edge is allowed (fail-open), exactly as the per-request parse treated it.
    pub reqs: Box<[usize]>,
    meta: MacroMeta,
}

enum MacroMeta {
    /// No metadata bytes, or bytes that do not parse: the payload shows `{}`.
    Empty,
    Parsed(JsonValue),
    /// The synthetic global-teleport carrier (src = dst = 0): its ~113 KB document is
    /// already held per teleport in [`GlobalTeleport::meta`], so it is re-parsed on
    /// demand rather than kept twice (a path never contains the 0 -> 0 self-loop).
    Carrier,
}

/// Macro edges by (src, dst), plus the per-edge decoded metadata (see
/// [`MacroEdgeInfo`]), indexed by snapshot macro-edge index.
#[derive(Default)]
pub struct MacroLookup {
    by_pair: FxHashMap<(u32, u32), Vec<u32>>,
    edges: Vec<MacroEdgeInfo>,
}

impl MacroLookup {
    /// Snapshot macro-edge indices from `src` to `dst`, in index order.
    pub fn get(&self, key: &(u32, u32)) -> Option<&Vec<u32>> {
        self.by_pair.get(key)
    }

    /// Decoded requirement list of edge `idx` (empty when out of range).
    pub fn edge_reqs(&self, idx: usize) -> &[usize] {
        self.edges.get(idx).map_or(&[], |e| &e.reqs)
    }

    /// Whether `mask` satisfies edge `idx`'s requirements.
    pub fn allowed(&self, idx: usize, mask: &EligibilityMask) -> bool {
        self.edge_reqs(idx).iter().all(|&i| mask.is_satisfied(i))
    }

    /// Owned copy of edge `idx`'s metadata for a payload action (`{}` when missing or
    /// unparseable). Owned because the payload builder mutates it per action.
    pub fn meta_value(&self, snap: &Snapshot, idx: usize) -> JsonValue {
        match self.edges.get(idx).map(|e| &e.meta) {
            Some(MacroMeta::Parsed(v)) => v.clone(),
            Some(MacroMeta::Carrier) | None => snap
                .macro_meta_at(idx)
                .and_then(|b| serde_json::from_slice(b).ok())
                .unwrap_or_else(|| serde_json::json!({})),
            Some(MacroMeta::Empty) => serde_json::json!({}),
        }
    }
}

/// Decode a metadata `requirements` array into tag indices (fail-closed on unknown ids).
fn decode_reqs(val: &JsonValue, id_to_idx: &FxHashMap<u32, usize>, missing: &mut u64) -> Vec<usize> {
    let mut reqs = Vec::new();
    if let Some(arr) = val.get("requirements").and_then(|v| v.as_array()) {
        for ridv in arr {
            if let Some(rid) = ridv.as_u64() {
                if let Some(&tag_idx) = id_to_idx.get(&(rid as u32)) {
                    reqs.push(tag_idx);
                } else {
                    // Fail-closed: unknown requirement id means the edge can never be satisfied
                    reqs.push(usize::MAX);
                    *missing += 1;
                }
            }
        }
    }
    reqs
}

pub fn build_neighbor_provider(snapshot: &Snapshot) -> (NeighborProvider, NeighborProvider, Vec<GlobalTeleport>, MacroLookup) {
    // 1. Map of req_id -> tag_index
    let id_to_idx = crate::build_req_tag_index(Some(snapshot));

    // 2. Iterate macro edges and parse their metadata once
    let msrc = snapshot.macro_src();
    let len = msrc.len();
    let mut macro_reqs: Vec<Vec<usize>> = Vec::with_capacity(len);
    let mut edges: Vec<MacroEdgeInfo> = Vec::with_capacity(len);
    let mut globals: Vec<GlobalTeleport> = Vec::new();
    let mut by_pair: FxHashMap<(u32, u32), Vec<u32>> = FxHashMap::with_capacity_and_hasher(len, Default::default());

    let msrc_vec: &[u32] = msrc;
    let mdst_vec: &[u32] = snapshot.macro_dst();
    let mw_vec: &[f32] = snapshot.macro_w();
    let mkind_vec: &[u32] = snapshot.macro_kind_first();

    let mut missing_req_ids: u64 = 0;

    for idx in 0..len {
        let mut reqs = Vec::new();
        let is_carrier = msrc_vec[idx] == 0 && mdst_vec[idx] == 0;
        let mut info = MacroEdgeInfo { reqs: Box::default(), meta: MacroMeta::Empty };

        if let Some(bytes) = snapshot.macro_meta_at(idx) {
            if let Ok(val) = serde_json::from_slice::<JsonValue>(bytes) {
                // Check for global def
                if is_carrier {
                    if let Some(arr) = val.get("global").and_then(|v| v.as_array()) {
                         for g in arr {
                             let dst = g.get("dst").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                             let cost = g.get("cost_ms").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                             let kind_first = g
                                 .get("steps")
                                 .and_then(|v| v.as_array())
                                 .and_then(|a| a.first())
                                 .and_then(|s| s.get("kind"))
                                 .and_then(|v| v.as_str())
                                 .map(kind_code)
                                 .unwrap_or(0);
                             let g_reqs = decode_reqs(g, &id_to_idx, &mut missing_req_ids);
                             if dst != 0 {
                                 globals.push(GlobalTeleport { dst, cost, reqs: g_reqs, kind_first, meta: Arc::new(g.clone()) });
                             }
                         }
                    }
                    // The carrier is not a search edge (no requirements on the provider),
                    // but the payload decodes its top-level list like any other edge.
                    let mut uncounted = 0;
                    info.reqs = decode_reqs(&val, &id_to_idx, &mut uncounted).into_boxed_slice();
                    info.meta = MacroMeta::Carrier;
                } else {
                    reqs = decode_reqs(&val, &id_to_idx, &mut missing_req_ids);
                    info.reqs = reqs.clone().into_boxed_slice();
                    info.meta = MacroMeta::Parsed(val);
                }
            }
        }
        macro_reqs.push(reqs);
        edges.push(info);
        by_pair
            .entry((msrc_vec[idx], mdst_vec[idx]))
            .or_default()
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

    (provider, provider_rev, globals, MacroLookup { by_pair, edges })
}

/// Build fairy ring runtime data from snapshot.
/// Returns: (Vec<FairyRing>, HashMap<node_id, ring_index>)
pub fn build_fairy_rings(snapshot: &Snapshot) -> (Vec<FairyRing>, FxHashMap<u32, usize>) {
    // Build req_id -> tag_index map
    let id_to_idx = crate::build_req_tag_index(Some(snapshot));

    let fairy_count = snapshot.counts().fairy_rings as usize;
    let mut rings: Vec<FairyRing> = Vec::with_capacity(fairy_count);
    let mut node_to_ring: FxHashMap<u32, usize> = FxHashMap::with_capacity_and_hasher(fairy_count, Default::default());
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

                    // Parse requirements and map to tag indices (fail-closed on unknown ids)
                    let reqs = decode_reqs(&val, &id_to_idx, &mut missing_req_ids);

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
    // Engine selection: `Policy` for normal requests, `Uni`/`Bidir` for the race.
    engine: EngineChoice,
    // Per-search state: one context for unidirectional searches, two for bidirectional
    // (see [`SearchContexts`]); reset per search, checked out for this call only.
    ctxs: &mut dyn SearchContexts,
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
    let lm = LandmarkHeuristic::from_snapshot(snap_ref);

    let mut view = EngineView {
        nodes,
        walk: WalkGraph::from_snapshot(snap_ref),
        macros: neighbors,
        lm,
        extra: ExtraEdges::default(),
        coords: Some(snap_ref.coords_packed()),
        canonical,
        jps: navpath_core::engine::search::jps_enabled(),
    };

    // Per-profile artifacts: eligible globals (available from every node; the engine
    // relaxes them once from the start, so they never enter per-pop neighbor merges),
    // fairy sources/dests, and the folded macro filters — all pre-sorted exactly as the
    // engine expects (see build_profile_artifacts).
    view.extra.global = std::borrow::Cow::Borrowed(artifacts.eligible_globals.as_slice());
    view.extra.fairy_sources = std::borrow::Cow::Borrowed(artifacts.fairy_sources.as_slice());
    view.extra.fairy_dests = std::borrow::Cow::Borrowed(artifacts.fairy_dests.as_slice());

    let macro_filter = &artifacts.macro_filter;

    // Weak-backward demotion (roadmap 4.2): compare the two lower bounds of C* the
    // engines will steer by. lb_fwd = min(h(start), min over seeds (w + h(dst))) is
    // the forward search's effective bound; hb_goal is the backward bound anchored on
    // the same origin set. If the backward bound is provably loose relative to the
    // forward one, unidirectional wins — skip bidir for this request.
    let bidir_ok = bidir_enabled() && neighbors_rev.is_some() && engine != EngineChoice::Uni && {
        let ratio = bidir_min_hb_ratio();
        if ratio <= 0.0 || engine == EngineChoice::Bidir {
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
    if bidir.is_some() {
        // Per-profile backward-anchor aggregate (T3.9): the engine folds only this
        // request's origin into it.
        view.extra.global_rev_base = Some(artifacts.rev_base(&view.lm));
    }

    let search = |seed: Option<u64>, max_pops: Option<u32>, resume: bool, ctxs: &mut dyn SearchContexts| -> SearchResult {
        let params = SearchParams { start: start_id, goal: goal_id, macro_filter: Some(macro_filter), seed, max_pops, cancel, bucket_ms: bucket_for(seed) };
        if let Some((rev, macro_filter_rev)) = bidir.as_ref() {
            let bp = BidirParams { macros_rev: rev, macro_filter_rev: Some(macro_filter_rev) };
            let (cf, cb) = ctxs.two();
            return if resume { view.astar_bidir_resume(&bp, params, cf, cb) } else { view.astar_bidir(&bp, params, cf, cb) };
        }
        if resume { view.astar_resume(params, ctxs.one()) } else { view.astar(params, ctxs.one()) }
    };

    let res = search(seed, default_max_pops(nodes), false, ctxs);
    let engine_name = if bidir.is_some() { "bidir" } else if view.jps && view.canonical.is_some() { "jps" } else { "uni" };
    retry_ladder(res, seed, cancel, nodes, engine_name, |s, m, r| search(s, m, r, ctxs))
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
    engine: EngineChoice,
    ctxs: &mut dyn SearchContexts,
) -> (SearchOutcome, Option<u32>) {
    let eligible_globals: &[(u32, f32)] = &artifacts.eligible_globals;
    if eligible_globals.is_empty() {
        return (
            SearchOutcome { res: not_found_result(), retried: false, attempts_pops: [0, 0, 0], seed_dropped: false, engine: "uni" },
            None,
        );
    }

    let nodes = snapshot.counts().nodes as usize;
    let snap_ref: &Snapshot = &snapshot;
    let lm = LandmarkHeuristic::from_snapshot(snap_ref);
    let mut view = EngineView {
        nodes,
        walk: WalkGraph::from_snapshot(snap_ref),
        macros: neighbors,
        lm,
        extra: ExtraEdges::default(),
        coords: Some(snap_ref.coords_packed()),
        canonical,
        jps: navpath_core::engine::search::jps_enabled(),
    };
    view.extra.fairy_sources = std::borrow::Cow::Borrowed(artifacts.fairy_sources.as_slice());
    view.extra.fairy_dests = std::borrow::Cow::Borrowed(artifacts.fairy_dests.as_slice());

    let macro_filter = &artifacts.macro_filter;

    // Weak-backward demotion, virtual-start flavor (roadmap 4.2/4.4): anchors are the
    // seed set itself, and lb_fwd = min over seeds (g0 + h(dst)).
    let bidir_ok = bidir_enabled() && neighbors_rev.is_some() && engine != EngineChoice::Uni && {
        let ratio = bidir_min_hb_ratio();
        if ratio <= 0.0 || engine == EngineChoice::Bidir {
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
    if bidir.is_some() {
        // Per-profile backward-anchor aggregate (T3.9): the engine folds only this
        // request's origin into it.
        view.extra.global_rev_base = Some(artifacts.rev_base(&view.lm));
    }

    // One multi-source search replaces one full A* per eligible teleport: every teleport
    // destination is seeded at g = its cost, and the winning entry is path[0]. The engine
    // leaves `extra.global` unused in multi-source mode, and this out-of-graph start has
    // no mid-route teleports by construction (a second teleport at any node u would cost
    // g(u) + c >= c, dominated by seeding it directly).
    let search = |seed: Option<u64>, max_pops: Option<u32>, resume: bool, ctxs: &mut dyn SearchContexts| -> SearchResult {
        let params = SearchParams { start: goal_id, goal: goal_id, macro_filter: Some(macro_filter), seed, max_pops, cancel, bucket_ms: bucket_for(seed) };
        if let Some((rev, macro_filter_rev)) = bidir.as_ref() {
            let bp = BidirParams { macros_rev: rev, macro_filter_rev: Some(macro_filter_rev) };
            let (cf, cb) = ctxs.two();
            return if resume {
                view.astar_bidir_multi_resume(eligible_globals, &bp, params, cf, cb)
            } else {
                view.astar_bidir_multi(eligible_globals, &bp, params, cf, cb)
            };
        }
        if resume {
            view.astar_multi_resume(eligible_globals, params, ctxs.one())
        } else {
            view.astar_multi(eligible_globals, params, ctxs.one())
        }
    };

    let res = search(seed, default_max_pops(nodes), false, ctxs);
    let engine_name = if bidir.is_some() { "bidir" } else if view.jps && view.canonical.is_some() { "jps" } else { "uni" };
    let outcome = retry_ladder(res, seed, cancel, nodes, engine_name, |s, m, r| search(s, m, r, ctxs));

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
            path_g: if found { vec![0.0, cost] } else { Vec::new() },
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

    /// The pre-T3.14 precheck, verbatim: per-request filtering plus fixpoint sweeps.
    fn goal_reachable_reference(cg: &ComponentGraph, mask: &EligibilityMask, start_comp: Option<u16>, goal_comp: u16) -> bool {
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
        let edges: Vec<(u16, u16)> = cg.macro_edges.iter()
            .filter(|(_, _, reqs)| reqs.iter().all(|&i| mask.is_satisfied(i)))
            .map(|&(s, d, _)| (s, d))
            .collect();
        let ring_comps: Vec<u16> = cg.fairy.iter()
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

    #[test]
    fn profile_reach_matches_the_fixpoint_precheck() {
        let mut state: u64 = 0x5EED_1234_ABCD_0001;
        let mut next = |m: u64| -> u64 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) % m
        };
        for round in 0..300 {
            // Up to 1100 components exercises the heap-bitset branch too.
            let comps = 2 + next(if round % 10 == 0 { 1100 } else { 60 }) as usize;
            let tags = 1 + next(6) as usize;
            let reqs = |next: &mut dyn FnMut(u64) -> u64| -> Vec<usize> {
                (0..next(3)).map(|_| if next(10) == 0 { usize::MAX } else { next(tags as u64) as usize }).collect()
            };
            let macro_edges = (0..next(3 * comps as u64))
                .map(|_| (next(comps as u64) as u16, next(comps as u64) as u16, reqs(&mut next)))
                .collect();
            let fairy = (0..next(5)).map(|_| (next(comps as u64) as u16, reqs(&mut next))).collect();
            let globals = (0..next(4)).map(|_| (next(comps as u64) as u16, reqs(&mut next))).collect();
            let cg = ComponentGraph { components: comps, macro_edges, fairy, globals };
            for _ in 0..8 {
                let mask = mask_of(&(0..tags).map(|_| next(3) != 0).collect::<Vec<_>>());
                let reach = ProfileReach::build(&cg, &mask);
                for _ in 0..8 {
                    let start = if next(5) == 0 { None } else { Some(next(comps as u64) as u16) };
                    // Occasionally a goal id past every component (reference sizes for it).
                    let goal = if next(20) == 0 { comps as u16 + next(3) as u16 } else { next(comps as u64) as u16 };
                    assert_eq!(
                        reach.reachable(start, goal),
                        goal_reachable_reference(&cg, &mask, start, goal),
                        "round {round} start {start:?} goal {goal}"
                    );
                }
            }
        }
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
