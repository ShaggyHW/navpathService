//! Walk pricing since snapshot v10: every step costs 300 ms in all 8 directions
//! (Chebyshev), diagonals plus a tiny tie-break (`WALK_DIAGONAL_TIEBREAK_MS`).
//!
//! Checked on random obstacle grids with an exact ALT table and the canonical /
//! jump-point grid: every engine returns the plain-Dijkstra cost (the heuristic stays
//! admissible and the pruning stays exact under the new weights), a pure diagonal walk
//! costs n x 300 (+ the tie-break), paths are deterministic, and the tie-break keeps
//! paths natural (fewest steps, then fewest diagonals; no zig-zags).

use std::collections::BinaryHeap;
use std::sync::Arc;

use navpath_core::engine::canonical::CanonicalGrid;
use navpath_core::engine::heuristics::quantize_alt_ms;
use navpath_core::engine::neighbors::WalkGraph;
use navpath_core::engine::search::{BidirParams, SearchContext};
use navpath_core::snapshot::{pack_coord, unpack_coord, walk_diagonal_ms, ALT_QUANTUM_MS, WALK_CARDINAL_MS, WALK_DIAGONAL_TIEBREAK_MS};
use navpath_core::{EngineView, LandmarkHeuristic, NeighborProvider, SearchParams, SearchResult};

const DELTAS: [(i32, i32); 8] = [(-1, 0), (0, -1), (1, 0), (0, 1), (-1, 1), (-1, -1), (1, -1), (1, 1)];

struct Grid {
    coords: Vec<u32>,
    offsets: Vec<u32>,
    dst: Vec<u32>,
    diag: Vec<u8>,
    tab: Vec<u16>,
    landmarks: usize,
    canonical: Arc<CanonicalGrid>,
    empty: Vec<u32>,
    empty_w: Vec<f32>,
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self, m: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % m
    }
}

impl Grid {
    /// `size` x `size` tiles at (1..=size)^2; `blocked` tiles are absent. Diagonals need
    /// both orthogonal neighbours (no corner cutting), as the builder's walk rule.
    fn build(size: i32, blocked: impl Fn(i32, i32) -> bool, lm_seed: u64) -> Grid {
        let mut coords: Vec<u32> = Vec::new();
        for x in 1..=size {
            for y in 1..=size {
                if !blocked(x, y) {
                    coords.push(pack_coord(x, y, 0));
                }
            }
        }
        coords.sort_unstable();
        let pos = |x: i32, y: i32| -> Option<u32> {
            if x < 0 || y < 0 {
                return None;
            }
            coords.binary_search(&pack_coord(x, y, 0)).ok().map(|i| i as u32)
        };
        let mut offsets = vec![0u32];
        let mut dst = Vec::new();
        let mut diag_bits = Vec::new();
        for &c in &coords {
            let (ux, uy, _) = unpack_coord(c);
            for (d, &(dx, dy)) in DELTAS.iter().enumerate() {
                let Some(v) = pos(ux + dx, uy + dy) else { continue };
                if d >= 4 && (pos(ux + dx, uy).is_none() || pos(ux, uy + dy).is_none()) {
                    continue;
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
        let n = coords.len();
        let mut canonical = CanonicalGrid::build(n, &coords, &offsets, &dst, &[], &[], &[]).expect("canonical grid");
        canonical.build_jump_tables(&offsets, &dst, &coords);
        let mut g = Grid {
            coords,
            offsets,
            dst,
            diag,
            tab: Vec::new(),
            landmarks: 0,
            canonical: Arc::new(canonical),
            empty: Vec::new(),
            empty_w: Vec::new(),
        };
        let mut rng = Lcg(lm_seed);
        let lms: Vec<u32> = (0..4).map(|_| rng.next(n as u64) as u32).collect();
        let l = lms.len();
        let mut tab = vec![0u16; n * 2 * l];
        for (li, &lm) in lms.iter().enumerate() {
            let d = g.dijkstra(lm); // the walk graph is symmetric: fw == bw
            for v in 0..n {
                tab[v * 2 * l + 2 * li] = quantize_alt_ms(d[v]);
                tab[v * 2 * l + 2 * li + 1] = quantize_alt_ms(d[v]);
            }
        }
        g.tab = tab;
        g.landmarks = l;
        g
    }

    fn n(&self) -> usize {
        self.coords.len()
    }

    fn node(&self, x: i32, y: i32) -> u32 {
        self.coords.binary_search(&pack_coord(x, y, 0)).expect("tile exists") as u32
    }

    fn weight(&self, slot: usize) -> f32 {
        if self.diag[slot / 8] & (1 << (slot % 8)) != 0 { walk_diagonal_ms() } else { WALK_CARDINAL_MS }
    }

    fn dijkstra(&self, s: u32) -> Vec<f32> {
        #[derive(PartialEq)]
        struct E(f32, u32);
        impl Eq for E {}
        impl PartialOrd for E {
            fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(o)) }
        }
        impl Ord for E {
            fn cmp(&self, o: &Self) -> std::cmp::Ordering { o.0.partial_cmp(&self.0).unwrap().then(o.1.cmp(&self.1)) }
        }
        let mut dist = vec![f32::INFINITY; self.n()];
        dist[s as usize] = 0.0;
        let mut heap = BinaryHeap::from([E(0.0, s)]);
        while let Some(E(g, u)) = heap.pop() {
            if g > dist[u as usize] {
                continue;
            }
            for slot in self.offsets[u as usize] as usize..self.offsets[u as usize + 1] as usize {
                let v = self.dst[slot];
                let ng = g + self.weight(slot);
                if ng < dist[v as usize] {
                    dist[v as usize] = ng;
                    heap.push(E(ng, v));
                }
            }
        }
        dist
    }

    fn view(&self, canonical: bool, jps: bool) -> EngineView<'_> {
        EngineView {
            nodes: self.n(),
            walk: WalkGraph::Csr { offsets: &self.offsets, dst: &self.dst, diag: &self.diag },
            macros: Arc::new(NeighborProvider::new(self.n(), &self.empty, &self.empty, &self.empty_w)),
            lm: LandmarkHeuristic::new(self.n(), self.landmarks, &self.tab, ALT_QUANTUM_MS),
            extra: Default::default(),
            coords: Some(&self.coords),
            canonical: if canonical { Some(self.canonical.clone()) } else { None },
            jps,
        }
    }

    /// Every engine configuration for one (start, goal) pair.
    fn run_all(&self, s: u32, g: u32) -> Vec<(&'static str, SearchResult)> {
        let params = || SearchParams { start: s, goal: g, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 };
        let n = self.n();
        let mut out = Vec::new();
        for (name, canon, jps) in [("uni", false, false), ("uni-canon", true, false), ("uni-jps", true, true)] {
            let mut ctx = SearchContext::new(n);
            out.push((name, self.view(canon, jps).astar(params(), &mut ctx)));
        }
        let rev = NeighborProvider::new(n, &self.empty, &self.empty, &self.empty_w);
        let bp = BidirParams { macros_rev: &rev, macro_filter_rev: None };
        for (name, canon) in [("bidir", false), ("bidir-canon", true)] {
            let (mut cf, mut cb) = (SearchContext::new(n), SearchContext::new(n));
            out.push((name, self.view(canon, false).astar_bidir(&bp, params(), &mut cf, &mut cb)));
        }
        out
    }

    /// (steps, diagonal steps) of a node path; panics on a non-adjacent pair.
    fn shape(&self, path: &[u32]) -> (usize, usize) {
        let mut diags = 0;
        for w in path.windows(2) {
            let (ax, ay, _) = unpack_coord(self.coords[w[0] as usize]);
            let (bx, by, _) = unpack_coord(self.coords[w[1] as usize]);
            let (dx, dy) = (bx - ax, by - ay);
            assert!(dx.abs() <= 1 && dy.abs() <= 1 && (dx, dy) != (0, 0), "path step is not a walk step");
            if dx != 0 && dy != 0 {
                diags += 1;
            }
        }
        (path.len() - 1, diags)
    }
}

fn walk_ms(steps: usize, diags: usize) -> f32 {
    steps as f32 * WALK_CARDINAL_MS + diags as f32 * WALK_DIAGONAL_TIEBREAK_MS
}

#[test]
fn a_diagonal_step_takes_as_long_as_a_cardinal_one() {
    assert_eq!(WALK_CARDINAL_MS, 300.0);
    assert!(walk_diagonal_ms() > WALK_CARDINAL_MS && walk_diagonal_ms() - WALK_CARDINAL_MS < 1.0);
}

#[test]
fn pure_diagonal_walk_costs_n_times_300() {
    let g = Grid::build(24, |_, _| false, 7);
    for n in [1, 5, 13, 23] {
        let (s, t) = (g.node(1, 1), g.node(1 + n, 1 + n));
        for (name, res) in g.run_all(s, t) {
            assert!(res.found, "{name}");
            assert_eq!(g.shape(&res.path), (n as usize, n as usize), "{name}: a diagonal line is n diagonal steps");
            let want = walk_ms(n as usize, n as usize);
            assert!((res.cost - want).abs() < 1e-2, "{name}: n={n} cost {} != {want}", res.cost);
            // The walk part of the cost is n x 300; the tie-break is sub-millisecond.
            assert_eq!((res.cost / WALK_CARDINAL_MS).floor() as i32, n, "{name}");
        }
    }
}

#[test]
fn an_offset_costs_its_chebyshev_distance_with_fewest_diagonals() {
    let g = Grid::build(24, |_, _| false, 11);
    for (dx, dy) in [(10, 3), (3, 10), (7, 0), (15, 14), (20, 1)] {
        let (s, t) = (g.node(2, 2), g.node(2 + dx, 2 + dy));
        let steps = dx.max(dy) as usize;
        let diags = dx.min(dy) as usize;
        for (name, res) in g.run_all(s, t) {
            assert_eq!(g.shape(&res.path), (steps, diags), "{name} ({dx},{dy})");
            assert!((res.cost - walk_ms(steps, diags)).abs() < 1e-2, "{name} ({dx},{dy}) cost {}", res.cost);
        }
    }
}

#[test]
fn every_engine_matches_dijkstra_on_random_obstacle_grids() {
    for grid_seed in 1..6u64 {
        let mut rng = Lcg(grid_seed * 0x9E37_79B9);
        let walls: Vec<bool> = (0..32 * 32).map(|_| rng.next(100) < 22).collect();
        let g = Grid::build(30, |x, y| walls[(y * 32 + x) as usize], grid_seed);
        let n = g.n() as u64;
        for _ in 0..40 {
            let (s, t) = (rng.next(n) as u32, rng.next(n) as u32);
            let dist = g.dijkstra(s);
            let want = dist[t as usize];
            for (name, res) in g.run_all(s, t) {
                let ctx = format!("grid {grid_seed} {s}->{t} {name}");
                if !want.is_finite() {
                    assert!(!res.found, "{ctx}: found a path to an unreachable goal");
                    continue;
                }
                assert!(res.found, "{ctx}");
                assert!((res.cost - want).abs() < 1e-2, "{ctx}: cost {} != dijkstra {want}", res.cost);
                let (steps, diags) = g.shape(&res.path);
                assert!((walk_ms(steps, diags) - res.cost).abs() < 1e-2, "{ctx}: path priced differently than reported");
            }
        }
    }
}

#[test]
fn paths_are_deterministic() {
    let mut rng = Lcg(42);
    let walls: Vec<bool> = (0..32 * 32).map(|_| rng.next(100) < 18).collect();
    let g = Grid::build(30, |x, y| walls[(y * 32 + x) as usize], 3);
    let n = g.n() as u64;
    for _ in 0..30 {
        let (s, t) = (rng.next(n) as u32, rng.next(n) as u32);
        let a = g.run_all(s, t);
        let b = g.run_all(s, t);
        for ((name, ra), (_, rb)) in a.iter().zip(b.iter()) {
            assert_eq!(ra.path, rb.path, "{name}: same request, different path");
            assert_eq!(ra.cost.to_bits(), rb.cost.to_bits(), "{name}");
        }
    }
}

#[test]
fn open_ground_paths_turn_once() {
    // On open ground the jump-point engine's canonical shape is diagonals first, then
    // straight: one turn.
    let g = Grid::build(24, |_, _| false, 5);
    for (dx, dy) in [(12, 4), (4, 12), (9, 2), (15, 7)] {
        let (s, t) = (g.node(3, 3), g.node(3 + dx, 3 + dy));
        for (name, res) in g.run_all(s, t) {
            if name != "uni-jps" {
                // Only the jump-point table tie-prunes equal-cost orderings; the other
                // engines may interleave the (cost-minimal) diagonals and cardinals,
                // exactly as they could under octile weights. Never a zig-zag either way:
                // that would cost an extra diagonal, see the Dijkstra test.
                continue;
            }
            let dirs: Vec<(i32, i32)> = res
                .path
                .windows(2)
                .map(|w| {
                    let (ax, ay, _) = unpack_coord(g.coords[w[0] as usize]);
                    let (bx, by, _) = unpack_coord(g.coords[w[1] as usize]);
                    (bx - ax, by - ay)
                })
                .collect();
            let turns = dirs.windows(2).filter(|w| w[0] != w[1]).count();
            assert!(turns <= 1, "{name} ({dx},{dy}): {turns} direction changes in {dirs:?}");
        }
    }
}
