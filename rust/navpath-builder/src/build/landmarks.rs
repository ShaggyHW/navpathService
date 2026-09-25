//! Landmark selection and the quantized ALT table.
//!
//! Two selection strategies share one table writer:
//!
//! * [`Strategy::Legacy`] — the pre-2026-09-25 forward-only farthest-point selection,
//!   reproduced EXACTLY (same landmark ids, same table bytes) for A/B runs, but with the
//!   selection Dijkstras pruned to each pick's Voronoi cell and the columns computed in
//!   parallel batches (roadmap T5.1–T5.3).
//! * [`Strategy::Scc`] — SCC-aware selection (roadmap T1.1): the budget is split across
//!   strongly connected components by size and landmarks are placed inside each SCC by
//!   symmetric farthest-point, plus local landmarks for weakly disconnected components
//!   written into existing columns (T1.1b, see [`LocalFill`] for the admissibility
//!   argument).
//!
//! # Why SCCs
//!
//! The runtime heuristic (navpath-core `heuristics.rs`, `select_active`) only uses a
//! landmark L for goal g when BOTH d(L,g) and d(g,L) are finite, i.e. when L and g share
//! an SCC of the table graph. The legacy selection scored candidates by forward distance
//! only, where "unreachable from every chosen landmark" scores INFINITY and wins: it
//! kept planting landmarks in one-way pockets (source pockets reachable by a handful of
//! nodes, a sink region, small islands), which leaves those columns unusable for goals in
//! the main SCC.

use navpath_core::engine::heuristics::quantize_alt_ms;
use navpath_core::snapshot::{ALT_SATURATED, ALT_UNREACHABLE};
use rayon::prelude::*;

use super::components::Structure;
use super::dijkstra::{dijkstra, dijkstra_multi, AltGraph, Dir, RadixHeap};

/// Minimum weak-component size (in nodes) for the legacy strategy to place landmarks in
/// it; also the minimum SCC size for a dedicated SCC-strategy column. 4096 = one full
/// 64x64 region.
pub const MIN_LANDMARK_COMPONENT: usize = 4096;

/// Landmark counts are kept a multiple of this: the runtime's AVX-512 full-row heuristic
/// needs a row stride (2 u16 per landmark) that is a whole number of 32-lane registers.
pub const LANDMARK_ALIGN: u32 = 16;

/// Columns computed per table-writer batch: 16 landmarks x (fw, bw) u16 = one 64-byte
/// cache line per node row, and 32 concurrent Dijkstras.
const TABLE_BATCH: usize = 16;

/// Non-main dedicated SCCs may take at most this fraction of the budget (1/n).
const MAX_POCKET_SHARE_DIV: usize = 4;

/// Pocket SCCs get this multiple of their size-proportional share. Goals in a one-way
/// pocket depend entirely on that pocket's own landmarks (the runtime drops a column
/// for a goal unless BOTH its entries are finite, so no main-SCC landmark bounds them),
/// while the main SCC is near saturation. Measured 2026-09-25 at 64 landmarks over 500
/// random pairs (examples/alt_eval): boost 2 (52 main / 8 in the 68,815-node sink
/// region) vs boost 1 (56 / 4): main-SCC goals 2.30M vs 2.28M uni pops (+1%),
/// sink goals 89k vs 174k (-49%).
const POCKET_BOOST: f64 = 2.0;

/// Main-SCC columns never written by the local fill (see [`LocalFill`]).
const FILL_SENTINELS: usize = 2;
/// Local landmarks per weakly disconnected component: one per this many nodes, capped.
const FILL_NODES_PER_LOCAL: usize = 2048;
const FILL_MAX_LOCAL: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Forward-only farthest-point over components >= 4096 nodes (pre-T1.1, exact).
    Legacy,
    /// SCC-aware symmetric farthest-point with optional local fill.
    Scc,
}

#[derive(Clone, Copy, Debug)]
pub struct AltConfig {
    /// Requested landmark count (already aligned by the caller if desired).
    pub count: usize,
    pub strategy: Strategy,
    /// T1.1b: write local landmarks for weakly disconnected components into the
    /// fillable columns (SCC strategy only).
    pub local_fill: bool,
}

/// Round a requested landmark count up to a multiple of [`LANDMARK_ALIGN`].
pub fn align_landmark_count(n: u32) -> u32 {
    n.div_ceil(LANDMARK_ALIGN) * LANDMARK_ALIGN
}

/// Local-landmark fill for one weakly connected component (T1.1b).
///
/// Every node of `wcc` gets, in each column of `columns[j]`, its distances to/from local
/// landmark `landmarks[j]` (a node of `wcc`) instead of the UNREACHABLE pair the
/// column's main landmark leaves there.
///
/// # Admissibility
///
/// Notation: R_c(v) is the node a column-c entry of v measures against — the column's
/// main landmark if v shares its WCC, the local landmark if v's WCC was filled in c,
/// otherwise nothing (both entries UNREACHABLE). By construction R_c is constant on each
/// WCC. The table graph is a superset of every edge the engine relaxes from a
/// non-origin node (walk, macro floored at quick-tele, full fairy clique); global
/// teleports are relaxed only from the search origin (search.rs, uni and bidir
/// seeding), and are not table edges. Hence **a non-origin node can only ever reach
/// nodes of its own WCC**.
///
/// Forward `h_active(u)` for goal g (column c active: g's entries both < SATURATED):
/// * u, g in the same WCC: R_c(u) = R_c(g), so every term is the textbook ALT bound
///   against one landmark and the INF rule (bu == UNREACHABLE while g reaches R_c)
///   proves u cannot reach g. Admissible exactly as before.
/// * u, g in different WCCs: the true d(u, g) through relaxable edges is INFINITY, so
///   any value — finite garbage or INFINITY — is a valid lower bound. The one node for
///   which "cannot reach" is not the whole story is the ORIGIN, which also reaches g via
///   globals; but the origin's globals are relaxed unconditionally at seeding, whatever
///   h(origin) is (an INF h only skips pushing the origin itself, whose table-edge
///   expansions cannot reach g). Every node on an optimal path after the origin lies in
///   g's WCC, so it keeps an admissible h — which is all A* (with re-opening, as here)
///   and the incumbent prune `f <= g(goal)` need. Multi-source seeds are the same
///   situation: a seed outside g's WCC cannot reach g.
///
/// Backward `h_active_rev(v)` (bidir), anchors = origin + global dsts or seeds, bound
/// on d(anchor set, v) = min over anchors a of g0(a) + d(a, v):
/// * a-side / b-side aggregate `c1 = min_a (g0 - fa)`, `c2 = min_a (g0 + ba)`, i.e. the
///   bound is the MIN over anchors of each anchor's own bound. For an anchor in v's WCC
///   the per-anchor term uses one landmark on both ends (valid ALT bound); for an anchor
///   in another WCC d(a, v) = INFINITY and its finite term is trivially below it. A min
///   of per-anchor lower bounds is a lower bound of the min. Validity flags (all
///   anchors fa < SATURATED / ba != UNREACHABLE) only get easier to satisfy, never
///   admit an overstating term: fill values are exact distances like any other.
/// * INF rule (`inf_ok`: every anchor has fa != UNREACHABLE; then fv == UNREACHABLE =>
///   INFINITY): an anchor in v's WCC is reached by R_c(v) (fa finite, same reference),
///   so if it reached v then R_c(v) would reach v, contradicting fv == UNREACHABLE; an
///   anchor in another WCC cannot reach v at all. So no anchor reaches v. Sound.
///
/// # What it costs, and the sentinels
///
/// The only thing fill can lose is INF pruning: before, every node of an uncovered WCC
/// W had UNREACHABLE entries in every column, so for a goal in the main SCC the origin
/// (or a global dst) in W got h = INFINITY and W was never expanded. The first
/// [`FILL_SENTINELS`] main-SCC columns and every non-main dedicated column are never
/// filled; for a goal in the main SCC (or in a dedicated pocket SCC) those columns are
/// active and still UNREACHABLE on all of W, so that pruning is fully retained. The
/// remaining "fillable" columns are all filled for every non-main WCC (cycling through
/// that WCC's local landmarks), which also keeps the bidir backward aggregate usable
/// when a global teleport lands in a small WCC: previously ONE such anchor turned every
/// column's `fa`/`ba` UNREACHABLE and switched the backward heuristic off entirely.
/// (Globals landing in one-way POCKETS of the main WCC still do that — a source-pocket
/// anchor has fa = UNREACHABLE for every main landmark, a sink-pocket anchor
/// ba = UNREACHABLE — and no table can admissibly fix it; the runtime would have to
/// drop anchors that cannot reach the goal before aggregating.)
#[derive(Clone, Debug)]
pub struct LocalFill {
    pub wcc: u32,
    pub landmarks: Vec<u32>,
    /// `columns[j]`: the table columns that receive local landmark `j`.
    pub columns: Vec<Vec<u32>>,
    /// `entries[j][i]`: quantized (d(L_j, v), d(v, L_j)) for the i-th node of the WCC
    /// (ascending node id).
    entries: Vec<Vec<(u16, u16)>>,
}

/// Selection result: one main landmark per table column, plus local fills.
pub struct AltPlan {
    pub landmarks: Vec<u32>,
    /// SCC rank of each column's landmark.
    pub landmark_scc: Vec<u32>,
    pub fills: Vec<LocalFill>,
}

/// Select landmarks and build the interleaved quantized ALT table
/// `[node][landmark][fw, bw]` (u16 quanta).
pub fn build_alt(g: &AltGraph, st: &Structure, cfg: &AltConfig) -> (AltPlan, Vec<u16>) {
    let t = std::time::Instant::now();
    let plan = match cfg.strategy {
        _ if cfg.count == 0 || g.n == 0 => AltPlan { landmarks: Vec::new(), landmark_scc: Vec::new(), fills: Vec::new() },
        Strategy::Legacy => {
            let landmarks = select_legacy(g, st, cfg.count);
            let landmark_scc = landmarks.iter().map(|&l| st.scc_of(l as usize)).collect();
            AltPlan { landmarks, landmark_scc, fills: Vec::new() }
        }
        Strategy::Scc => select_scc(g, st, cfg.count, cfg.local_fill),
    };
    if plan.landmarks.is_empty() {
        return (plan, Vec::new());
    }
    let select_ms = t.elapsed().as_millis() as u64;
    let t = std::time::Instant::now();
    let mut tab = write_table(g, &plan.landmarks);
    let table_ms = t.elapsed().as_millis() as u64;
    let t = std::time::Instant::now();
    apply_fills(st, &plan, &mut tab);
    tracing::info!(select_ms, table_ms, fill_ms = t.elapsed().as_millis() as u64, "ALT stage timings");
    (plan, tab)
}

// ---------------------------------------------------------------------------------
// Legacy strategy (exact reproduction of the old output)
// ---------------------------------------------------------------------------------

/// The pre-T1.1 selection: start from the lowest-id node of the largest weak component,
/// repeatedly take the eligible node maximizing forward min-distance to the chosen set
/// (INFINITY — unreached — wins; ties to the lowest id). Eligible = weak component of
/// the table graph with >= min(4096, largest) nodes.
///
/// Identical picks to the old code: its `min_dist` after each pick is the pointwise min
/// of full forward Dijkstras, which the pruned update reproduces bit for bit (see
/// [`dijkstra`]), and distances themselves are heap-independent.
fn select_legacy(g: &AltGraph, st: &Structure, count: usize) -> Vec<u32> {
    let n = g.n;
    let largest = st.wcc_size.iter().copied().max().unwrap_or(0);
    let threshold = MIN_LANDMARK_COMPONENT.min(largest.max(1));
    let eligible: Vec<bool> = (0..n).map(|v| st.wcc_size[st.wcc_of(v) as usize] >= threshold).collect();
    let eligible_count = eligible.iter().filter(|&&e| e).count();
    let k = count.min(eligible_count);
    if k == 0 {
        return Vec::new();
    }
    // Old code: max_by_key((component size, Reverse(v))) then the first node of that
    // component — i.e. the lowest node id among nodes of maximum component size.
    let seed = (0..n).find(|&v| st.wcc_size[st.wcc_of(v) as usize] == largest).unwrap();

    let mut heap = RadixHeap::new();
    let mut min_dist = vec![f32::INFINITY; n];
    dijkstra(g, Dir::Fwd, seed, &mut min_dist, &mut heap);

    let mut is_landmark = vec![false; n];
    let mut landmarks: Vec<u32> = Vec::with_capacity(k);
    for pick in 0..k {
        // argmax of min_dist over eligible non-landmarks; INFINITY wins; ties -> lowest id
        // (the old sequential scan with a strict `>`).
        let best = (0..n)
            .into_par_iter()
            .filter(|&v| eligible[v] && !is_landmark[v])
            .map(|v| (min_dist[v], v))
            .reduce_with(|a, b| if b.0 > a.0 || (b.0 == a.0 && b.1 < a.1) { b } else { a });
        let Some((_, lm)) = best else { break };
        is_landmark[lm] = true;
        landmarks.push(lm as u32);
        if pick == 0 {
            // The seed only bootstraps the first pick and is not itself a landmark.
            min_dist.fill(f32::INFINITY);
        }
        dijkstra(g, Dir::Fwd, lm, &mut min_dist, &mut heap);
    }
    landmarks
}

// ---------------------------------------------------------------------------------
// SCC strategy
// ---------------------------------------------------------------------------------

/// Split `total` columns over candidate SCCs (`sizes` descending, `sizes[0]` = the main
/// SCC): every other candidate gets [`POCKET_BOOST`] x its size-proportional share,
/// rounded to nearest, at least 1; while those exceed `total / MAX_POCKET_SHARE_DIV`
/// the smallest candidates are dropped (a single oversized pocket is clamped to the
/// cap). The main SCC takes the rest.
pub fn allocate(total: usize, sizes: &[usize]) -> Vec<usize> {
    let mut out = vec![0usize; sizes.len()];
    if sizes.is_empty() {
        return out;
    }
    let cap = total / MAX_POCKET_SHARE_DIV;
    let sum: usize = sizes.iter().sum::<usize>().max(1);
    let mut shares: Vec<usize> = sizes[1..]
        .iter()
        .map(|&s| ((POCKET_BOOST * (total * s) as f64 / sum as f64).round() as usize).max(1))
        .collect();
    let mut m = shares.len();
    while m > 0 && shares[..m].iter().sum::<usize>() > cap {
        m -= 1;
    }
    if m == 0 && !shares.is_empty() && cap > 0 {
        shares[0] = shares[0].min(cap);
        m = 1;
    }
    out[1..=m].copy_from_slice(&shares[..m]);
    out[0] = total - out.iter().sum::<usize>();
    out
}

/// Reusable per-worker buffers: forward/backward distance arrays kept all-INFINITY
/// between uses, and their heaps.
struct Scratch {
    f: Vec<f32>,
    b: Vec<f32>,
    hf: RadixHeap,
    hb: RadixHeap,
}

impl Scratch {
    fn new(n: usize) -> Self {
        Scratch { f: vec![f32::INFINITY; n], b: vec![f32::INFINITY; n], hf: RadixHeap::new(), hb: RadixHeap::new() }
    }

    /// Restore the all-INFINITY invariant. `closed` = a node set containing everything
    /// the last runs could touch (a whole WCC), for an O(|closed|) reset.
    fn reset(&mut self, closed: Option<&[u32]>) {
        match closed {
            Some(nodes) => {
                for &v in nodes {
                    self.f[v as usize] = f32::INFINITY;
                    self.b[v as usize] = f32::INFINITY;
                }
            }
            None => {
                self.f.fill(f32::INFINITY);
                self.b.fill(f32::INFINITY);
            }
        }
    }

    fn run_both(&mut self, g: &AltGraph, src: usize, parallel: bool) {
        self.run_both_multi(g, &[src as u32], parallel);
    }

    fn run_both_multi(&mut self, g: &AltGraph, srcs: &[u32], parallel: bool) {
        let Scratch { f, b, hf, hb } = self;
        if parallel {
            rayon::join(|| dijkstra_multi(g, Dir::Fwd, srcs, f, hf), || dijkstra_multi(g, Dir::Rev, srcs, b, hb));
        } else {
            dijkstra_multi(g, Dir::Fwd, srcs, f, hf);
            dijkstra_multi(g, Dir::Rev, srcs, b, hb);
        }
    }
}

/// Per-local-landmark quantized (fw, bw) entries for the nodes of a WCC.
type LocalColumns = Vec<Vec<(u16, u16)>>;

/// Symmetric farthest-point inside one SCC (`members`, ascending node ids).
///
/// Score(v) = F(v) + B(v), where F(v) = min over chosen L of d(L, v) and
/// B(v) = min over chosen L of d(v, L): how far v is from the landmark set going out
/// AND coming back. A landmark only serves a goal when both of the goal's entries are
/// finite, so both directions must count; the legacy forward-only score let a node that
/// is merely hard to reach one way (or not at all — INFINITY) win every argmax. Inside
/// an SCC both terms are finite, so INFINITY can never win.
///
/// Why this form rather than min over L of the round trip d(L,v) + d(v,L): both
/// envelopes are pointwise minima of full Dijkstras, so each pick updates them with the
/// exact PRUNED Dijkstra (see [`dijkstra`]) that expands only the new landmark's
/// forward/backward Voronoi cells — about 10x less work than two full Dijkstras per pick
/// in the sequential part of the build. On this map (walk edges symmetric, a few
/// thousand one-way macro/fairy edges) the two scores rank nodes almost identically:
/// where the nearest landmark is the same both ways they coincide.
///
/// Each pick maximizes the score, ties to the lowest id. Bootstrapping:
/// * `boundary = None` (the main SCC, island WCCs): the first landmark is the node with
///   the largest round trip from the SCC's lowest-id node, which is then discarded, as
///   in the legacy selection;
/// * `boundary = Some(nodes)` (a one-way pocket): the pocket's boundary — its nodes
///   with an edge to or from another SCC — acts as a permanent virtual landmark. Every
///   query from outside enters a sink pocket through its boundary (and leaves a source
///   pocket through it), so for a goal g in the pocket and a start u outside, the only
///   usable bound d(u, L) - d(g, L) = d(u, E) + d(E, L) - d(g, L) is tight when g lies
///   between the entrance E and L: pocket landmarks belong far from the boundary, not
///   merely far from each other. (This is what the legacy forward-only selection got
///   right by accident inside the sink region: its distances were measured from the
///   main landmarks, i.e. through the entrances. Measured on the 68,815-node sink with
///   8 landmarks, 23 random queries into it: 617k uni pops without the boundary
///   bootstrap, 89k with it; legacy's 8 there: 91k.)
///
/// Farthest-point (over avoid / maxCover) keeps selection deterministic and cheap, and
/// on a grid-like graph its peripheral picks are what ALT wants: a landmark "behind"
/// the goal as seen from the start. The table's columns are computed afterwards by
/// [`write_table`].
///
/// `closed`: if every node the runs can touch is known (a whole WCC), resets cost
/// O(|closed|) instead of O(n).
fn farthest_symmetric(
    g: &AltGraph,
    members: &[u32],
    k: usize,
    sc: &mut Scratch,
    closed: Option<&[u32]>,
    parallel: bool,
    boundary: Option<&[u32]>,
) -> Vec<u32> {
    let k = k.min(members.len());
    let mut picks: Vec<u32> = Vec::with_capacity(k);
    if k == 0 {
        return picks;
    }
    let mut picked = vec![false; members.len()];
    let keep_bootstrap = boundary.is_some_and(|b| !b.is_empty());
    match boundary {
        Some(b) if !b.is_empty() => sc.run_both_multi(g, b, parallel),
        _ => sc.run_both(g, members[0] as usize, parallel),
    }

    for _ in 0..k {
        let (f, b) = (&sc.f, &sc.b);
        let score = |i: usize| f[members[i] as usize] + b[members[i] as usize];
        let best = if parallel && members.len() > 1 << 16 {
            (0..members.len())
                .into_par_iter()
                .filter(|&i| !picked[i])
                .map(|i| (score(i), i))
                .reduce_with(|a, b| if b.0 > a.0 || (b.0 == a.0 && b.1 < a.1) { b } else { a })
        } else {
            let mut best: Option<(f32, usize)> = None;
            for i in 0..members.len() {
                if !picked[i] {
                    let s = score(i);
                    if best.map_or(true, |(bs, _)| s > bs) {
                        best = Some((s, i));
                    }
                }
            }
            best
        };
        let Some((_, i)) = best else { break };
        picked[i] = true;
        let lm = members[i];
        if picks.is_empty() && !keep_bootstrap {
            // The seed only bootstraps the first pick; coverage restarts from it.
            sc.reset(closed);
        }
        picks.push(lm);
        // Pruned updates: F = min(F, d(lm, .)), B = min(B, d(., lm)).
        sc.run_both(g, lm as usize, parallel);
    }
    sc.reset(closed);
    picks
}

/// Full forward/backward columns of `landmarks` restricted to the nodes of one WCC
/// (`nodes`, which contains everything reachable from/to them), quantized.
fn local_columns(g: &AltGraph, landmarks: &[u32], nodes: &[u32], sc: &mut Scratch) -> LocalColumns {
    landmarks
        .iter()
        .map(|&lm| {
            sc.run_both(g, lm as usize, false);
            let col = nodes
                .iter()
                .map(|&v| (quantize_alt_ms(sc.f[v as usize]), quantize_alt_ms(sc.b[v as usize])))
                .collect();
            sc.reset(Some(nodes));
            col
        })
        .collect()
}

fn select_scc(g: &AltGraph, st: &Structure, count: usize, local_fill: bool) -> AltPlan {
    let n = g.n;
    let main_wcc = st.scc_wcc[0];
    // Dedicated-column candidates: the main SCC, plus SCCs of at least one region —
    // only inside the main WCC when the local fill covers the other WCCs.
    let mut cands: Vec<u32> = vec![0];
    for s in 1..st.scc_size.len() as u32 {
        let big = st.scc_size[s as usize] >= MIN_LANDMARK_COMPONENT;
        if big && (!local_fill || st.scc_wcc[s as usize] == main_wcc) {
            cands.push(s);
        }
    }
    let sizes: Vec<usize> = cands.iter().map(|&s| st.scc_size[s as usize]).collect();
    let alloc = allocate(count, &sizes);
    let chosen: Vec<(u32, usize)> = cands.iter().copied().zip(alloc.iter().copied()).filter(|&(_, k)| k > 0).collect();

    let scc_members = st.members(true);
    let wcc_members = st.members(false);

    // Boundary of every SCC: its nodes with an edge to or from another SCC. Walk
    // components never straddle SCCs, so only extra (macro/fairy) edges can cross.
    let mut boundary: Vec<Vec<u32>> = vec![Vec::new(); st.scc_size.len()];
    for (s, d) in g.fwd.pairs() {
        let (cs, cd) = (st.scc_of(s as usize), st.scc_of(d as usize));
        if cs != cd {
            boundary[cs as usize].push(s);
            boundary[cd as usize].push(d);
        }
    }
    for b in boundary.iter_mut() {
        b.sort_unstable();
        b.dedup();
    }

    // Local-fill targets: every WCC other than the main one.
    let fill_targets: Vec<u32> =
        if local_fill { (0..st.wcc_size.len() as u32).filter(|&w| w != main_wcc).collect() } else { Vec::new() };

    // The main SCC dominates the wall time (its picks are sequential, two Dijkstras
    // each), so run it alongside everything else.
    let (dedicated, locals): (Vec<(u32, Vec<u32>)>, Vec<(u32, Vec<u32>, LocalColumns)>) = rayon::join(
        || {
            chosen
                .par_iter()
                .map(|&(s, k)| {
                    let t = std::time::Instant::now();
                    let mut sc = Scratch::new(n);
                    // Pockets bootstrap from their boundary; the main SCC from its seed.
                    let bnd = (s != 0).then(|| boundary[s as usize].as_slice());
                    let picks = farthest_symmetric(g, &scc_members[s as usize], k, &mut sc, None, true, bnd);
                    tracing::info!(
                        scc = s,
                        size = st.scc_size[s as usize],
                        picks = picks.len(),
                        elapsed_ms = t.elapsed().as_millis() as u64,
                        "dedicated landmark selection"
                    );
                    (s, picks)
                })
                .collect()
        },
        || {
            // Sequential with one scratch: the non-main WCCs are small (every run stays
            // inside its WCC and resets in O(|WCC|)), so per-thread n-sized buffers
            // would cost far more memory than the parallelism saves.
            let mut sc = if fill_targets.is_empty() { None } else { Some(Scratch::new(n)) };
            fill_targets
                .iter()
                .map(|&w| {
                    let sc = sc.as_mut().unwrap();
                    let wn = &wcc_members[w as usize];
                    // Local landmarks go into the WCC's largest SCC (lowest SCC rank).
                    let s = st.scc_of(wn.iter().copied().min_by_key(|&v| st.scc_of(v as usize)).unwrap() as usize);
                    let k = wn.len().div_ceil(FILL_NODES_PER_LOCAL).clamp(1, FILL_MAX_LOCAL);
                    let picks = farthest_symmetric(g, &scc_members[s as usize], k, sc, Some(wn.as_slice()), false, None);
                    let cols = local_columns(g, &picks, wn, sc);
                    (w, picks, cols)
                })
                .collect()
        },
    );

    let mut landmarks = Vec::with_capacity(count);
    let mut landmark_scc = Vec::with_capacity(count);
    for (s, picks) in &dedicated {
        for &p in picks {
            landmarks.push(p);
            landmark_scc.push(*s);
        }
    }
    // Tiny graphs can run out of distinct nodes; pad with duplicate columns (harmless:
    // same bounds twice) so the row stride stays aligned for the SIMD heuristic.
    let mut i = 0;
    while !landmarks.is_empty() && landmarks.len() < count {
        landmarks.push(landmarks[i]);
        landmark_scc.push(landmark_scc[i]);
        i += 1;
    }

    // Fillable columns: main-SCC columns except the first FILL_SENTINELS.
    let fillable: Vec<u32> = (0..landmarks.len() as u32)
        .filter(|&c| landmark_scc[c as usize] == 0)
        .skip(FILL_SENTINELS)
        .collect();
    let mut fills = Vec::new();
    if !fillable.is_empty() {
        for (w, picks, entries) in locals {
            if picks.is_empty() {
                continue;
            }
            let mut columns: Vec<Vec<u32>> = vec![Vec::new(); picks.len()];
            for (j, &c) in fillable.iter().enumerate() {
                columns[j % picks.len()].push(c);
            }
            fills.push(LocalFill { wcc: w, landmarks: picks, columns, entries });
        }
    }
    AltPlan { landmarks, landmark_scc, fills }
}

/// Write each fill's local-landmark entries into its columns for the WCC's nodes.
fn apply_fills(st: &Structure, plan: &AltPlan, tab: &mut [u16]) {
    if plan.fills.is_empty() {
        return;
    }
    let l = plan.landmarks.len();
    let wcc_members = st.members(false);
    for fill in &plan.fills {
        let nodes = &wcc_members[fill.wcc as usize];
        for (j, columns) in fill.columns.iter().enumerate() {
            let entries = &fill.entries[j];
            debug_assert_eq!(entries.len(), nodes.len());
            for &c in columns {
                let c = c as usize;
                for (idx, &v) in nodes.iter().enumerate() {
                    let at = (v as usize * l + c) * 2;
                    // Fillable columns belong to main-SCC landmarks, which cannot reach
                    // (or be reached from) another WCC.
                    debug_assert!(
                        tab[at] == ALT_UNREACHABLE && tab[at + 1] == ALT_UNREACHABLE,
                        "fill target cell already covered by the column's main landmark"
                    );
                    tab[at] = entries[idx].0;
                    tab[at + 1] = entries[idx].1;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------
// Table writer (shared, exact)
// ---------------------------------------------------------------------------------

/// Compute every landmark's forward (d(L, n)) and backward (d(n, L)) column and write
/// them, quantized, into the interleaved `[node][landmark][fw, bw]` table.
///
/// Columns are produced in batches of [`TABLE_BATCH`] landmarks (2 x 16 Dijkstras in
/// parallel) and quantized while being scattered block-wise into the table, so only one
/// batch of columns is ever alive next to the table — instead of every f32 forward and
/// backward column (~600 MB at 1.17M nodes / 64 landmarks) plus the table (roadmap
/// T5.1). Each batch writes exactly one 64-byte line per node row. Values are
/// bit-identical to the old all-at-once transpose: same distances (see the `dijkstra`
/// module docs), same quantizer, same layout.
pub fn write_table(g: &AltGraph, landmarks: &[u32]) -> Vec<u16> {
    thread_local! {
        static HEAP: std::cell::RefCell<RadixHeap> = std::cell::RefCell::new(RadixHeap::new());
    }
    let n = g.n;
    let l = landmarks.len();
    let mut tab = vec![0u16; n * l * 2];
    if l == 0 {
        return tab;
    }
    for b0 in (0..l).step_by(TABLE_BATCH) {
        let b1 = (b0 + TABLE_BATCH).min(l);
        let jobs: Vec<(usize, Dir)> = (b0..b1).flat_map(|li| [(li, Dir::Fwd), (li, Dir::Rev)]).collect();
        let cols: Vec<Vec<f32>> = jobs
            .par_iter()
            .map(|&(li, dir)| {
                let mut dist = vec![f32::INFINITY; n];
                HEAP.with(|h| dijkstra(g, dir, landmarks[li] as usize, &mut dist, &mut h.borrow_mut()));
                dist
            })
            .collect();
        const BLOCK: usize = 8192;
        let width = b1 - b0;
        tab.par_chunks_mut(BLOCK * l * 2).enumerate().for_each(|(ci, chunk)| {
            let base = ci * BLOCK;
            let rows = chunk.len() / (l * 2);
            for k in 0..rows {
                let row = &mut chunk[k * l * 2 + b0 * 2..k * l * 2 + b1 * 2];
                for j in 0..width {
                    row[2 * j] = quantize_alt_ms(cols[2 * j][base + k]);
                    row[2 * j + 1] = quantize_alt_ms(cols[2 * j + 1][base + k]);
                }
            }
        });
    }
    tab
}

// ---------------------------------------------------------------------------------
// Usability statistics
// ---------------------------------------------------------------------------------

/// How useful a table is to the runtime heuristic, whose `select_active` keeps column c
/// for goal g only when both of g's entries are below SATURATED.
#[derive(Debug, Clone)]
pub struct TableStats {
    pub columns: usize,
    pub main_scc_size: usize,
    /// Columns usable (both entries < SATURATED) for EVERY node of the main SCC.
    pub usable_main: usize,
    /// Nodes with no usable column at all: any query targeting them runs with h = 0.
    pub h0_nodes: usize,
    /// ... of which inside the main WCC (one-way pockets) / outside it (islands).
    pub h0_main_wcc: usize,
    pub h0_other_wcc: usize,
    /// Mean usable columns per node, and over main-SCC nodes.
    pub mean_usable: f64,
    pub mean_usable_main: f64,
    pub saturated_entries: usize,
}

pub fn table_stats(st: &Structure, tab: &[u16], l: usize) -> TableStats {
    let n = st.nodes();
    let main_wcc = st.scc_wcc[0];
    #[derive(Default, Clone)]
    struct Acc {
        main_bad: Vec<bool>,
        h0: usize,
        h0_main: usize,
        usable_sum: u64,
        usable_main_sum: u64,
        sat: usize,
    }
    let acc = (0..n)
        .into_par_iter()
        .fold(
            || Acc { main_bad: vec![false; l], ..Default::default() },
            |mut a, v| {
                let row = &tab[v * l * 2..(v + 1) * l * 2];
                let in_main = st.scc_of(v) == 0;
                let mut usable = 0u32;
                for c in 0..l {
                    let (f, b) = (row[2 * c], row[2 * c + 1]);
                    a.sat += (f == ALT_SATURATED) as usize + (b == ALT_SATURATED) as usize;
                    if f < ALT_SATURATED && b < ALT_SATURATED {
                        usable += 1;
                    } else if in_main {
                        a.main_bad[c] = true;
                    }
                }
                if usable == 0 {
                    a.h0 += 1;
                    if st.wcc_of(v) == main_wcc {
                        a.h0_main += 1;
                    }
                }
                a.usable_sum += usable as u64;
                if in_main {
                    a.usable_main_sum += usable as u64;
                }
                a
            },
        )
        .reduce(
            || Acc { main_bad: vec![false; l], ..Default::default() },
            |mut a, b| {
                for c in 0..l {
                    a.main_bad[c] |= b.main_bad[c];
                }
                a.h0 += b.h0;
                a.h0_main += b.h0_main;
                a.usable_sum += b.usable_sum;
                a.usable_main_sum += b.usable_main_sum;
                a.sat += b.sat;
                a
            },
        );
    let main = st.scc_size.first().copied().unwrap_or(0);
    TableStats {
        columns: l,
        main_scc_size: main,
        usable_main: acc.main_bad.iter().filter(|&&b| !b).count(),
        h0_nodes: acc.h0,
        h0_main_wcc: acc.h0_main,
        h0_other_wcc: acc.h0 - acc.h0_main,
        mean_usable: acc.usable_sum as f64 / n.max(1) as f64,
        mean_usable_main: acc.usable_main_sum as f64 / main.max(1) as f64,
        saturated_entries: acc.sat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::components::walk_components;

    /// Symmetric line 0-1-2-3 (weights implied: cardinal 300 ms) + isolated pair 4-5.
    fn line_csr() -> (Vec<u32>, Vec<u32>, Vec<u8>) {
        let off = vec![0u32, 1, 3, 5, 6, 7, 8];
        let dst = vec![1u32, 0, 2, 1, 3, 2, 5, 4];
        let diag = vec![0u8; 1];
        (off, dst, diag)
    }

    fn structure(g: &AltGraph) -> Structure {
        let (comp, cc) = walk_components(g.walk_off, g.walk_dst);
        Structure::new(comp, cc, g.fwd.pairs())
    }

    #[test]
    fn legacy_farthest_point_spreads_landmarks() {
        let (off, dst, diag) = line_csr();
        let g = AltGraph::new(6, &off, &dst, &diag, &[], &[], &[]);
        let st = structure(&g);
        let cfg = AltConfig { count: 2, strategy: Strategy::Legacy, local_fill: false };
        let (plan, tab) = build_alt(&g, &st, &cfg);
        let mut sorted = plan.landmarks.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 3]);
        let li = |lm: u32| plan.landmarks.iter().position(|&x| x == lm).unwrap();
        // one hop = 300 ms = 4 quanta (floor(300/64))
        assert_eq!(tab[(li(0)) * 2], 0);
        assert_eq!(tab[(3 * 2 + li(0)) * 2], quantize_alt_ms(900.0));
        assert_eq!(tab[(3 * 2 + li(0)) * 2 + 1], quantize_alt_ms(900.0));
        assert_eq!(tab[(4 * 2 + li(0)) * 2], ALT_UNREACHABLE);
    }

    /// Main SCC: a 6-node symmetric line 0..5 plus a one-way pocket 6 (5 -> 6 only);
    /// island WCC 7-8. The legacy strategy's INF-wins rule would target the pocket; the
    /// SCC strategy must keep every dedicated landmark inside the main SCC, and the fill
    /// must give the island a local landmark in the fillable columns only.
    #[test]
    fn scc_strategy_stays_in_scc_and_fills_islands() {
        // walk: 0-1-2-3-4-5 line, 7-8 pair; 6 has no walk edges.
        let off = vec![0u32, 1, 3, 5, 7, 9, 10, 10, 11, 12];
        let dst = vec![1u32, 0, 2, 1, 3, 2, 4, 3, 5, 4, 8, 7];
        let diag = vec![0u8; 2];
        let g = AltGraph::new(9, &off, &dst, &diag, &[5], &[6], &[100.0]);
        let st = structure(&g);
        assert_eq!(st.scc_size[0], 6);
        let cfg = AltConfig { count: 4, strategy: Strategy::Scc, local_fill: true };
        let (plan, tab) = build_alt(&g, &st, &cfg);
        assert_eq!(plan.landmarks.len(), 4);
        for &lm in &plan.landmarks {
            assert!(lm <= 5, "landmark {lm} outside the main SCC");
        }
        let l = plan.landmarks.len();
        // Endpoints first (farthest round trip from node 0 is node 5, then node 0).
        assert_eq!(&plan.landmarks[..2], &[5, 0]);
        // Island 7-8 is filled in the fillable columns (all but the first 2 sentinels).
        assert_eq!(plan.fills.len(), 1 + 0, "one non-main WCC with >1 node plus none else");
        for c in 0..l {
            let (f7, b7) = (tab[(7 * l + c) * 2], tab[(7 * l + c) * 2 + 1]);
            if c < FILL_SENTINELS {
                assert_eq!((f7, b7), (ALT_UNREACHABLE, ALT_UNREACHABLE), "sentinel column {c} filled");
            } else {
                assert!(f7 < ALT_SATURATED && b7 < ALT_SATURATED, "column {c} not filled for the island");
            }
        }
        // Pocket node 6: reachable from the main landmarks, cannot return.
        for c in 0..l {
            assert!(tab[(6 * l + c) * 2] < ALT_SATURATED);
            assert_eq!(tab[(6 * l + c) * 2 + 1], ALT_UNREACHABLE);
        }
        let s = table_stats(&st, &tab, l);
        assert_eq!(s.usable_main, 4);
        assert_eq!(s.h0_nodes, 1, "only the one-way pocket node has no usable column");
    }

    #[test]
    fn allocation_gives_main_the_rest() {
        assert_eq!(allocate(64, &[1_000_000, 68_000, 5_000]), vec![55, 8, 1]);
        // Too many pockets: smallest dropped until they fit in total/4.
        let a = allocate(16, &[100, 90, 80, 70, 60, 50]);
        assert!(a[0] >= 12, "{a:?}");
        assert_eq!(a.iter().sum::<usize>(), 16);
        assert_eq!(align_landmark_count(24), 32);
        assert_eq!(align_landmark_count(64), 64);
    }
}
