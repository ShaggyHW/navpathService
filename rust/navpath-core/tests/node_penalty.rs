//! Danger-area avoidance (`EngineView::astar_penalized` + `NodePenalty`): on a small
//! grid with a flagged block, the penalized search must
//!   - detour around the block when any detour exists;
//!   - still cross it (minimally) when it is the only way through;
//!   - be exact against a brute-force Dijkstra on the penalized pricing;
//!   - report real (penalty-free) costs after `NodePenalty::strip`.

use std::collections::BinaryHeap;

use navpath_core::engine::search::SearchContext;
use navpath_core::{EngineView, GoalSet, NodePenalty, SearchParams, SearchStatus};

const W: i32 = 12;
const H: i32 = 9;
const STEP: f32 = 600.0;
const PEN: f32 = 1_000_000.0;

fn id(x: i32, y: i32) -> u32 {
    (y * W + x) as u32
}

/// 4-connected W x H grid; `open(x, y)` decides which tiles exist.
fn grid(open: impl Fn(i32, i32) -> bool) -> (Vec<u32>, Vec<u32>, Vec<f32>) {
    let (mut src, mut dst, mut w) = (Vec::new(), Vec::new(), Vec::new());
    for y in 0..H {
        for x in 0..W {
            if !open(x, y) {
                continue;
            }
            for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                let (nx, ny) = (x + dx, y + dy);
                if nx >= 0 && ny >= 0 && nx < W && ny < H && open(nx, ny) {
                    src.push(id(x, y));
                    dst.push(id(nx, ny));
                    w.push(STEP);
                }
            }
        }
    }
    (src, dst, w)
}

fn bits_for(flag: impl Fn(i32, i32) -> bool) -> Vec<u64> {
    let n = (W * H) as usize;
    let mut bits = vec![0u64; n.div_ceil(64)];
    for y in 0..H {
        for x in 0..W {
            if flag(x, y) {
                let v = id(x, y);
                bits[(v >> 6) as usize] |= 1 << (v & 63);
            }
        }
    }
    bits
}

fn dijkstra(n: usize, src: &[u32], dst: &[u32], w: &[f32], pen: &NodePenalty, s: u32, g: u32) -> f32 {
    #[derive(PartialEq)]
    struct E(f32, u32);
    impl Eq for E {}
    impl PartialOrd for E {
        fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(o)) }
    }
    impl Ord for E {
        fn cmp(&self, o: &Self) -> std::cmp::Ordering { o.0.total_cmp(&self.0).then(o.1.cmp(&self.1)) }
    }
    let mut d = vec![f32::INFINITY; n];
    d[s as usize] = 0.0;
    let mut heap = BinaryHeap::new();
    heap.push(E(0.0, s));
    while let Some(E(c, u)) = heap.pop() {
        if c > d[u as usize] { continue; }
        if u == g { return c; }
        for i in 0..src.len() {
            if src[i] == u {
                let v = dst[i];
                let nc = c + w[i] + pen.get(v);
                if nc < d[v as usize] {
                    d[v as usize] = nc;
                    heap.push(E(nc, v));
                }
            }
        }
    }
    f32::INFINITY
}

fn params(start: u32, goal: u32, seed: Option<u64>) -> SearchParams<'static> {
    SearchParams { start, goal, macro_filter: None, seed, max_pops: None, cancel: None, bucket_ms: 0.0 }
}

fn view<'a>(src: &'a [u32], dst: &'a [u32], w: &'a [f32], ms: &'a [u32], md: &'a [u32], mw: &'a [f32]) -> EngineView<'a> {
    let mut v = EngineView::from_parts((W * H) as usize, src, dst, w, ms, md, mw, &[], 0);
    v.jps = false;
    v
}

#[test]
fn detours_around_a_flagged_block() {
    // Open field; danger block x 4..=7, y 2..=6. Straight line (1,4) -> (10,4) crosses it.
    let (src, dst, w) = grid(|_, _| true);
    let bits = bits_for(|x, y| (4..=7).contains(&x) && (2..=6).contains(&y));
    let pen = NodePenalty { bits: &bits, ms: PEN };
    let v = view(&src, &dst, &w, &[], &[], &[]);
    let mut ctx = SearchContext::new((W * H) as usize);

    let (s, g) = (id(1, 4), id(10, 4));
    let base = v.astar(params(s, g, None), &mut ctx);
    assert!(base.found);
    assert!(base.path.iter().any(|&n| pen.contains(n)), "the unpenalized optimum crosses the block");

    let mut res = v.astar_penalized(Some(s), &[], None, params(s, g, None), pen, &mut ctx, false);
    assert_eq!(res.status, SearchStatus::Found);
    assert_eq!(res.cost, dijkstra((W * H) as usize, &src, &dst, &w, &pen, s, g));
    let charged = pen.strip(&mut res, true);
    assert_eq!(charged, 0);
    assert!(!res.path.iter().any(|&n| pen.contains(n)), "a detour exists, so no danger tile is used");
    // Real cost of the detour: around the block (9 cols + 2x3 rows) = 15 steps.
    assert_eq!(res.cost, 15.0 * STEP);
    assert!(res.cost > base.cost);
}

#[test]
fn crosses_minimally_when_the_block_is_a_chokepoint() {
    // Wall at x 4..=7 except a corridor at y=4 that is all danger: the only way through.
    let open = |x: i32, y: i32| !(4..=7).contains(&x) || y == 4;
    let (src, dst, w) = grid(open);
    let bits = bits_for(|x, y| (4..=7).contains(&x) && y == 4);
    let pen = NodePenalty { bits: &bits, ms: PEN };
    let v = view(&src, &dst, &w, &[], &[], &[]);
    let mut ctx = SearchContext::new((W * H) as usize);

    let (s, g) = (id(1, 1), id(10, 7));
    let base = v.astar(params(s, g, None), &mut ctx);
    let mut res = v.astar_penalized(Some(s), &[], None, params(s, g, None), pen, &mut ctx, false);
    assert_eq!(res.status, SearchStatus::Found);
    assert_eq!(res.cost, dijkstra((W * H) as usize, &src, &dst, &w, &pen, s, g));
    let charged = pen.strip(&mut res, true);
    assert_eq!(charged, 4, "the 4-tile corridor is crossed once");
    assert_eq!(res.cost, base.cost, "stripped cost is the real travel cost");
    assert_eq!(*res.path_g.last().unwrap(), res.cost);
}

#[test]
fn start_and_goal_inside_the_area() {
    let (src, dst, w) = grid(|_, _| true);
    let bits = bits_for(|x, y| (4..=7).contains(&x) && (2..=6).contains(&y));
    let pen = NodePenalty { bits: &bits, ms: PEN };
    let v = view(&src, &dst, &w, &[], &[], &[]);
    let mut ctx = SearchContext::new((W * H) as usize);

    // Start inside: leaves by the exit with the fewest danger tiles (west via (4,4)),
    // even though the goal is east.
    let (s, g) = (id(5, 4), id(10, 4));
    let mut res = v.astar_penalized(Some(s), &[], None, params(s, g, None), pen, &mut ctx, false);
    assert_eq!(res.cost, dijkstra((W * H) as usize, &src, &dst, &w, &pen, s, g));
    assert_eq!(pen.strip(&mut res, true), 1, "start is free, (4,4) is the only charged tile");

    // Goal inside: enters along the shortest danger stretch.
    let (s, g) = (id(1, 4), id(5, 3));
    let mut res = v.astar_penalized(Some(s), &[], None, params(s, g, None), pen, &mut ctx, false);
    assert_eq!(res.cost, dijkstra((W * H) as usize, &src, &dst, &w, &pen, s, g));
    assert_eq!(pen.strip(&mut res, true), 2, "(4,3) then (5,3)");
}

#[test]
fn seeded_goal_set_and_multi_source() {
    let (src, dst, w) = grid(|_, _| true);
    let bits = bits_for(|x, y| (4..=7).contains(&x) && (2..=6).contains(&y));
    let pen = NodePenalty { bits: &bits, ms: PEN };
    let v = view(&src, &dst, &w, &[], &[], &[]);
    let mut ctx = SearchContext::new((W * H) as usize);

    // Goal set straddling the block: the safe goal tile wins even though it is farther.
    let goals = GoalSet::new([id(5, 4), id(10, 8)]);
    let s = id(1, 4);
    let mut res = v.astar_penalized(Some(s), &[], Some(&goals), params(s, id(5, 4), Some(7)), pen, &mut ctx, false);
    assert!(res.found);
    assert_eq!(*res.path.last().unwrap(), id(10, 8));
    assert_eq!(pen.strip(&mut res, true), 0);

    // Multi-source: a seed landing inside the area is charged; the safe seed wins.
    let seeds = [(id(5, 4), 0.0f32), (id(0, 0), 3.0 * STEP)];
    let mut res = v.astar_penalized(None, &seeds, None, params(0, id(1, 4), None), pen, &mut ctx, false);
    assert!(res.found);
    assert_eq!(res.path[0], id(0, 0));
    assert_eq!(pen.strip(&mut res, false), 0);
}
