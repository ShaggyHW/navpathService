//! Route-to-area (goal SET) searches: exactness against brute force on a synthetic
//! two-plane grid with obstacles, directed macro edges, global teleports, a real
//! (exactly built) ALT table and the canonical / jump-point grid.
//!
//! Contract checked for every engine (uni plain / canonical / JPS, bidir plain /
//! canonical, on-graph and multi-source starts, seeded and unseeded):
//!   - cost == min over g in G of d(start, g) (brute-force Dijkstra on the same
//!     jittered pricing);
//!   - the path is a valid walk of the graph, priced exactly as reported, ends on a goal
//!     and contains no earlier goal;
//!   - the reached goal's own single-goal distance equals the cost.

use std::collections::BinaryHeap;
use std::sync::Arc;

use navpath_core::engine::canonical::CanonicalGrid;
use navpath_core::engine::heuristics::quantize_alt_ms;
use navpath_core::engine::neighbors::WalkGraph;
use navpath_core::engine::search::{edge_jitter, BidirParams, SearchContext};
use navpath_core::snapshot::{pack_coord, unpack_coord, walk_diagonal_ms, ALT_QUANTUM_MS, WALK_CARDINAL_MS};
use navpath_core::{EngineView, GoalSet, LandmarkHeuristic, NeighborProvider, SearchParams, SearchResult, SearchStatus};

const DELTAS: [(i32, i32); 8] = [(-1, 0), (0, -1), (1, 0), (0, 1), (-1, 1), (-1, -1), (1, -1), (1, 1)];

struct Fixture {
    coords: Vec<u32>,
    offsets: Vec<u32>,
    dst: Vec<u32>,
    diag: Vec<u8>,
    msrc: Vec<u32>,
    mdst: Vec<u32>,
    mw: Vec<f32>,
    globals: Vec<(u32, f32)>,
    tab: Vec<u16>,
    landmarks: usize,
    canonical: Arc<CanonicalGrid>,
}

impl Fixture {
    fn n(&self) -> usize {
        self.coords.len()
    }

    fn node(&self, x: i32, y: i32, p: i32) -> Option<u32> {
        if !(0..32768).contains(&x) || !(0..32768).contains(&y) {
            return None; // rect_set rectangles hang off the grid edge
        }
        self.coords.binary_search(&pack_coord(x, y, p)).ok().map(|i| i as u32)
    }

    /// Plane 0: 26x26 with walls; plane 1: 12x12 open, joined to plane 0 by a stair
    /// pair; a directed "teleport" macro on plane 0; two global teleports.
    fn build() -> Fixture {
        let blocked0 = |x: i32, y: i32| {
            (y == 8 && (2..=20).contains(&x)) || (x == 14 && (12..=24).contains(&y)) || (x == 6 && (14..=19).contains(&y))
        };
        let mut tiles: Vec<(i32, i32, i32)> = Vec::new();
        for x in 1..27 {
            for y in 1..27 {
                if !blocked0(x, y) {
                    tiles.push((x, y, 0));
                }
            }
        }
        for x in 1..13 {
            for y in 1..13 {
                tiles.push((x, y, 1));
            }
        }
        let blocked = |x: i32, y: i32, p: i32| p == 0 && blocked0(x, y);
        let mut coords: Vec<u32> = tiles.iter().map(|&(x, y, p)| pack_coord(x, y, p)).collect();
        coords.sort_unstable();
        let pos = |x: i32, y: i32, p: i32| -> Option<u32> {
            if x < 0 || y < 0 {
                return None;
            }
            coords.binary_search(&pack_coord(x, y, p)).ok().map(|i| i as u32)
        };
        let n = coords.len();
        let mut offsets = vec![0u32];
        let mut dst = Vec::new();
        let mut diag_bits: Vec<bool> = Vec::new();
        for &c in &coords {
            let (ux, uy, up) = unpack_coord(c);
            for (d, &(dx, dy)) in DELTAS.iter().enumerate() {
                let Some(v) = pos(ux + dx, uy + dy, up) else { continue };
                if d >= 4 && (blocked(ux + dx, uy, up) || blocked(ux, uy + dy, up) || pos(ux + dx, uy, up).is_none() || pos(ux, uy + dy, up).is_none()) {
                    continue; // no corner cutting
                }
                dst.push(v);
                diag_bits.push(d >= 4);
            }
            offsets.push(dst.len() as u32);
        }
        let mut diag = vec![0u8; diag_bits.len().div_ceil(8) + 1];
        for (i, &b) in diag_bits.iter().enumerate() {
            if b {
                diag[i / 8] |= 1 << (i % 8);
            }
        }
        let p = |x, y, pl| pos(x, y, pl).unwrap();
        // Stairs (both ways), a one-way teleport across the wall.
        let msrc = vec![p(5, 5, 0), p(5, 5, 1), p(3, 22, 0)];
        let mdst = vec![p(5, 5, 1), p(5, 5, 0), p(22, 3, 0)];
        let mw = vec![1200.0f32, 1200.0, 1500.0];
        let globals = vec![(p(20, 20, 0), 9000.0f32), (p(10, 10, 1), 7000.0)];
        let lms = [p(1, 1, 0), p(26, 26, 0), p(26, 1, 0), p(12, 12, 1)];

        let mut canonical = CanonicalGrid::build(n, &coords, &offsets, &dst, &msrc, &mdst, &mw).expect("canonical grid");
        canonical.build_jump_tables(&offsets, &dst, &coords);

        let mut fx = Fixture {
            coords,
            offsets,
            dst,
            diag,
            msrc,
            mdst,
            mw,
            globals,
            tab: Vec::new(),
            landmarks: 0,
            canonical: Arc::new(canonical),
        };
        // Exact ALT table over walk + macro edges (globals are origin-only, uncovered).
        let l = lms.len();
        let mut tab = vec![0u16; n * 2 * l];
        for (li, &lm) in lms.iter().enumerate() {
            let fw = fx.dijkstra(&[(lm, 0.0)], None, false, false);
            let bw = fx.dijkstra(&[(lm, 0.0)], None, true, false);
            for v in 0..n {
                tab[v * 2 * l + 2 * li] = quantize_alt_ms(fw[v]);
                tab[v * 2 * l + 2 * li + 1] = quantize_alt_ms(bw[v]);
            }
        }
        fx.tab = tab;
        fx.landmarks = l;
        fx
    }

    fn edges_out(&self, u: u32, reverse: bool, mut f: impl FnMut(u32, f32)) {
        let ui = u as usize;
        for i in self.offsets[ui] as usize..self.offsets[ui + 1] as usize {
            let w = if self.diag[i / 8] & (1 << (i % 8)) != 0 { walk_diagonal_ms() } else { WALK_CARDINAL_MS };
            f(self.dst[i], w); // walk graph is symmetric
        }
        for i in 0..self.msrc.len() {
            let (s, d) = if reverse { (self.mdst[i], self.msrc[i]) } else { (self.msrc[i], self.mdst[i]) };
            if s == u {
                f(d, self.mw[i]);
            }
        }
    }

    /// Plain Dijkstra; `seed` prices edges like the engine (forward identity), and
    /// `globals_from_start` relaxes the globals from the (single) start seed.
    fn dijkstra(&self, seeds: &[(u32, f32)], seed: Option<u64>, reverse: bool, globals_from_start: bool) -> Vec<f32> {
        #[derive(PartialEq)]
        struct E(f32, u32);
        impl Eq for E {}
        impl PartialOrd for E {
            fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(o)) }
        }
        impl Ord for E {
            fn cmp(&self, o: &Self) -> std::cmp::Ordering { o.0.partial_cmp(&self.0).unwrap().then(o.1.cmp(&self.1)) }
        }
        let jit = |a: u32, b: u32, w: f32| match seed { Some(s) => w + edge_jitter(s, a, b), None => w };
        let mut dist = vec![f32::INFINITY; self.n()];
        let mut heap = BinaryHeap::new();
        for &(s, g0) in seeds {
            if g0 < dist[s as usize] {
                dist[s as usize] = g0;
                heap.push(E(g0, s));
            }
        }
        if globals_from_start {
            let s = seeds[0].0;
            for &(d, w) in &self.globals {
                let ng = jit(s, d, w);
                if ng < dist[d as usize] {
                    dist[d as usize] = ng;
                    heap.push(E(ng, d));
                }
            }
        }
        while let Some(E(g, u)) = heap.pop() {
            if g > dist[u as usize] {
                continue;
            }
            self.edges_out(u, reverse, |v, w| {
                let ng = g + if reverse { jit(v, u, w) } else { jit(u, v, w) };
                if ng < dist[v as usize] {
                    dist[v as usize] = ng;
                    heap.push(E(ng, v));
                }
            });
        }
        dist
    }

    fn view(&self, canonical: bool, jps: bool) -> EngineView<'_> {
        let mut v = EngineView {
            nodes: self.n(),
            walk: WalkGraph::Csr { offsets: &self.offsets, dst: &self.dst, diag: &self.diag },
            macros: Arc::new(NeighborProvider::new(self.n(), &self.msrc, &self.mdst, &self.mw)),
            lm: LandmarkHeuristic::new(self.n(), self.landmarks, &self.tab, ALT_QUANTUM_MS),
            extra: Default::default(),
            coords: Some(&self.coords),
            canonical: if canonical { Some(self.canonical.clone()) } else { None },
            jps,
        };
        v.extra.global = std::borrow::Cow::Borrowed(&self.globals);
        v
    }

    fn macros_rev(&self) -> NeighborProvider {
        NeighborProvider::new(self.n(), &self.mdst, &self.msrc, &self.mw)
    }

    /// Price `path` edge by edge (walk, macro, or — first hop of an on-graph start —
    /// a global); None when some step is not an edge.
    fn path_cost(&self, path: &[u32], g0: f32, seed: Option<u64>, origin: bool) -> Option<f32> {
        let jit = |a: u32, b: u32, w: f32| match seed { Some(s) => w + edge_jitter(s, a, b), None => w };
        let mut g = g0;
        for (i, w) in path.windows(2).enumerate() {
            let (a, b) = (w[0], w[1]);
            let mut best: Option<f32> = None;
            self.edges_out(a, false, |v, wt| {
                if v == b {
                    best = Some(best.map_or(wt, |x: f32| x.min(wt)));
                }
            });
            if best.is_none() && i == 0 && origin {
                best = self.globals.iter().filter(|g| g.0 == b).map(|g| g.1).reduce(f32::min);
            }
            g += jit(a, b, best?);
        }
        Some(g)
    }
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self, m: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % m
    }
}

fn params(start: u32, goal: u32, seed: Option<u64>, max_pops: Option<u32>) -> SearchParams<'static> {
    SearchParams { start, goal, macro_filter: None, seed, max_pops, cancel: None, bucket_ms: 0.0 }
}

/// All engine configurations for one query. `origin = None` = multi-source over the
/// globals (a virtual start).
fn run_all(fx: &Fixture, origin: Option<u32>, goals: &GoalSet, seed: Option<u64>) -> Vec<(&'static str, SearchResult)> {
    let rep = goals.nodes().first().copied().unwrap_or(0);
    let seeds: &[(u32, f32)] = if origin.is_some() { &[] } else { &fx.globals };
    let start = origin.unwrap_or(rep);
    let rev = fx.macros_rev();
    let bp = BidirParams { macros_rev: &rev, macro_filter_rev: None };
    let n = fx.n();
    let mut out = Vec::new();
    for (name, canon, jps) in [("uni", false, false), ("uni-canon", true, false), ("uni-jps", true, true)] {
        let view = fx.view(canon, jps);
        let mut ctx = SearchContext::new(n);
        out.push((name, view.astar_set(origin, seeds, goals, params(start, rep, seed, None), &mut ctx, false)));
    }
    for (name, canon) in [("bidir", false), ("bidir-canon", true)] {
        let view = fx.view(canon, false);
        let (mut cf, mut cb) = (SearchContext::new(n), SearchContext::new(n));
        out.push((name, view.astar_bidir_set(origin, seeds, goals, &bp, params(start, rep, seed, None), &mut cf, &mut cb, false)));
    }
    out
}

fn check(fx: &Fixture, origin: Option<u32>, goals: &GoalSet, seed: Option<u64>, label: &str) {
    let dist = match origin {
        Some(s) => fx.dijkstra(&[(s, 0.0)], seed, false, true),
        None => fx.dijkstra(&fx.globals, seed, false, false),
    };
    let best = goals.nodes().iter().map(|&g| dist[g as usize]).fold(f32::INFINITY, f32::min);
    for (name, res) in run_all(fx, origin, goals, seed) {
        let ctx = format!("{label} {name} origin={origin:?} seed={seed:?} |G|={}", goals.len());
        if !best.is_finite() {
            assert!(!res.found, "{ctx}: found a path to an unreachable set");
            assert_eq!(res.status, SearchStatus::NotFound, "{ctx}");
            continue;
        }
        assert!(res.found && res.status == SearchStatus::Found, "{ctx}: {:?}", res.status);
        assert!((res.cost - best).abs() < 0.05, "{ctx}: cost {} != brute force {best}", res.cost);
        let last = *res.path.last().unwrap();
        assert!(goals.contains(last), "{ctx}: path ends off the goal set");
        assert_eq!(goals.first_on_path(&res.path), Some(res.path.len() - 1), "{ctx}: an earlier goal on the path");
        assert!((dist[last as usize] - res.cost).abs() < 0.05, "{ctx}: reached goal is not a cheapest one");
        match origin {
            Some(s) => assert_eq!(res.path[0], s, "{ctx}"),
            None => assert!(fx.globals.iter().any(|g| g.0 == res.path[0]), "{ctx}: path must start at a seed"),
        }
        let g0 = if origin.is_some() { 0.0 } else { fx.globals.iter().find(|g| g.0 == res.path[0]).unwrap().1 };
        let priced = fx.path_cost(&res.path, g0, seed, origin.is_some()).unwrap_or_else(|| panic!("{ctx}: path is not a graph walk"));
        assert!((priced - res.cost).abs() < 0.05, "{ctx}: path prices at {priced}, reported {}", res.cost);
        assert_eq!(res.path_g.len(), res.path.len(), "{ctx}");
        assert!((res.path_g.last().unwrap() - res.cost).abs() < 1e-3, "{ctx}");
    }
}

fn rect_set(fx: &Fixture, x0: i32, y0: i32, x1: i32, y1: i32, p: i32) -> Vec<u32> {
    let mut v = Vec::new();
    for x in x0..=x1 {
        for y in y0..=y1 {
            if let Some(id) = fx.node(x, y, p) {
                v.push(id);
            }
        }
    }
    v
}

#[test]
fn goal_set_costs_match_brute_force_on_every_engine() {
    let fx = Fixture::build();
    let n = fx.n() as u64;
    let mut rng = Lcg(0xA4EA_2026);
    for round in 0..120 {
        let start = rng.next(n) as u32;
        let (sx, sy, sp) = unpack_coord(fx.coords[start as usize]);
        let _ = (sx, sy, sp);
        // Rectangles (partly off the grid / over walls), occasionally on plane 1 or
        // around a teleport landing; every 7th round a scattered multi-plane set.
        let goals: Vec<u32> = if round % 7 == 6 {
            (0..1 + rng.next(6)).map(|_| rng.next(n) as u32).collect()
        } else {
            let p = if round % 4 == 3 { 1 } else { 0 };
            let (cx, cy) = if round % 5 == 4 { (22, 3) } else { (rng.next(28) as i32, rng.next(28) as i32) };
            let (w, h) = (rng.next(9) as i32, rng.next(9) as i32);
            rect_set(&fx, cx - w / 2, cy - h / 2, cx + w - w / 2, cy + h - h / 2, p)
        };
        if goals.is_empty() {
            continue;
        }
        let set = GoalSet::new(goals);
        for seed in [None, Some(round as u64 * 31 + 7)] {
            check(&fx, Some(start), &set, seed, &format!("round {round}"));
            if round % 3 == 0 {
                check(&fx, None, &set, seed, &format!("round {round} virtual"));
            }
        }
    }
}

#[test]
fn start_inside_the_set_is_a_zero_cost_route() {
    let fx = Fixture::build();
    let set = GoalSet::new(rect_set(&fx, 3, 3, 6, 6, 0));
    let s = fx.node(4, 4, 0).unwrap();
    for (name, res) in run_all(&fx, Some(s), &set, None) {
        assert!(res.found, "{name}");
        assert_eq!(res.path, vec![s], "{name}");
        assert_eq!(res.cost, 0.0, "{name}");
    }
}

#[test]
fn empty_set_finds_nothing() {
    let fx = Fixture::build();
    for (name, res) in run_all(&fx, Some(0), &GoalSet::new(Vec::new()), None) {
        assert!(!res.found, "{name}");
        assert_eq!(res.status, SearchStatus::NotFound, "{name}");
    }
}

#[test]
fn exact_ties_resolve_by_rank_then_node_id() {
    // Plane-0 corridor row y=26 from x=1..26 is open; from (13,26) the tiles (9,26) and
    // (17,26) are both exactly four cardinal steps away (bit-identical costs). A rank decides
    // between them; equal ranks fall back to the lower node id. Unidirectional engines.
    let fx = Fixture::build();
    let s = fx.node(13, 26, 0).unwrap();
    let (a, b) = (fx.node(9, 26, 0).unwrap(), fx.node(17, 26, 0).unwrap());
    for (ra, rb, want) in [(0u32, 1u32, a), (1, 0, b), (0, 0, a.min(b))] {
        let set = GoalSet::with_ranks([(a, ra), (b, rb)]);
        for (name, canon, jps) in [("uni", false, false), ("uni-canon", true, false), ("uni-jps", true, true)] {
            let view = fx.view(canon, jps);
            let mut ctx = SearchContext::new(fx.n());
            let res = view.astar_set(Some(s), &[], &set, params(s, a, None, None), &mut ctx, false);
            assert_eq!(res.cost, 4.0 * WALK_CARDINAL_MS, "{name}");
            assert_eq!(*res.path.last().unwrap(), want, "{name} ranks ({ra},{rb})");
            // Deterministic: the same request ends on the same tile.
            let again = view.astar_set(Some(s), &[], &set, params(s, a, None, None), &mut ctx, false);
            assert_eq!(again.path, res.path, "{name}");
        }
    }
}

#[test]
fn budget_stopped_set_search_resumes_bit_identically() {
    let fx = Fixture::build();
    // Far corner, no teleport landing inside (the (20,20) global would end it at pop 1).
    let set = GoalSet::new(rect_set(&fx, 1, 23, 3, 26, 0));
    let s = fx.node(25, 2, 0).unwrap();
    let rep = set.nodes()[0];
    let rev = fx.macros_rev();
    let bp = BidirParams { macros_rev: &rev, macro_filter_rev: None };
    for seed in [None, Some(5u64)] {
        let view = fx.view(true, false);
        let mut ctx = SearchContext::new(fx.n());
        let fresh = view.astar_set(Some(s), &[], &set, params(s, rep, seed, None), &mut ctx, false);
        let first = view.astar_set(Some(s), &[], &set, params(s, rep, seed, Some(3)), &mut ctx, false);
        assert_eq!(first.status, SearchStatus::BudgetExceeded);
        let cont = view.astar_set(Some(s), &[], &set, params(s, rep, seed, None), &mut ctx, true);
        assert_eq!((cont.cost.to_bits(), &cont.path, cont.pops), (fresh.cost.to_bits(), &fresh.path, fresh.pops));

        let (mut cf, mut cb) = (SearchContext::new(fx.n()), SearchContext::new(fx.n()));
        let fresh_b = view.astar_bidir_set(Some(s), &[], &set, &bp, params(s, rep, seed, None), &mut cf, &mut cb, false);
        let first_b = view.astar_bidir_set(Some(s), &[], &set, &bp, params(s, rep, seed, Some(3)), &mut cf, &mut cb, false);
        assert_eq!(first_b.status, SearchStatus::BudgetExceeded);
        let cont_b = view.astar_bidir_set(Some(s), &[], &set, &bp, params(s, rep, seed, None), &mut cf, &mut cb, true);
        assert_eq!((cont_b.cost.to_bits(), &cont_b.path, cont_b.pops), (fresh_b.cost.to_bits(), &fresh_b.path, fresh_b.pops));
    }
}

#[test]
fn single_node_set_matches_the_single_goal_search() {
    let fx = Fixture::build();
    let n = fx.n() as u64;
    let mut rng = Lcg(77);
    for _ in 0..40 {
        let (s, g) = (rng.next(n) as u32, rng.next(n) as u32);
        let set = GoalSet::new([g]);
        for (canon, jps) in [(false, false), (true, false), (true, true)] {
            let view = fx.view(canon, jps);
            let mut ctx = SearchContext::new(fx.n());
            let one = view.astar(params(s, g, None, None), &mut ctx);
            let via_set = view.astar_set(Some(s), &[], &set, params(s, g, None, None), &mut ctx, false);
            assert_eq!(one.found, via_set.found);
            if one.found {
                assert!((one.cost - via_set.cost).abs() < 0.05, "{s}->{g}: {} vs {}", one.cost, via_set.cost);
            }
        }
    }
}
