//! End-to-end A* benchmarks over the real snapshot.
//!
//! Set `NAVPATH_BENCH_SNAPSHOT` to the snapshot path (defaults to `../../graph.snapshot`
//! relative to this crate). The corpus is derived deterministically from the snapshot's
//! node coordinates, so runs before/after a rebuild with the same tile DB are comparable.


use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use navpath_core::engine::search::{SearchContext, SearchParams};
use navpath_core::engine::heuristics::LandmarkHeuristic;
use navpath_core::{EngineView, Snapshot};

fn snapshot_path() -> String {
    std::env::var("NAVPATH_BENCH_SNAPSHOT")
        .unwrap_or_else(|_| format!("{}/../../graph.snapshot", env!("CARGO_MANIFEST_DIR")))
}

struct BenchCoords {
    x: Vec<i32>,
    y: Vec<i32>,
    p: Vec<i32>,
}

/// Parse the (0,0) macro edge's "global" metadata into (dst, cost) pairs, mirroring
/// what the service injects as `ExtraEdges::global` on every request.
fn parse_globals(snap: &Snapshot) -> Vec<(u32, f32)> {
    let msrc = snap.macro_src();
    let mdst = snap.macro_dst();
    let mut out = Vec::new();
    for idx in 0..msrc.len() {
        if msrc[idx] == 0 && mdst[idx] == 0 {
            if let Some(bytes) = snap.macro_meta_at(idx) {
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(bytes) {
                    if let Some(arr) = val.get("global").and_then(|v| v.as_array()) {
                        for g in arr {
                            let dst = g.get("dst").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                            let cost =
                                g.get("cost_ms").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                            if dst != 0 {
                                out.push((dst, cost));
                            }
                        }
                    }
                }
            }
        }
    }
    out.sort_unstable_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    out
}

/// Deterministic corpus: sample plane-0 nodes with a fixed LCG stride and bucket pairs
/// by octile coordinate distance. Reachability is validated once in setup with a plain
/// search so the measured loop only times found routes (the flood case is measured
/// separately).
fn build_corpus(
    view: &EngineView,
    coords: &BenchCoords,
    ctx: &mut SearchContext,
    globals: &[(u32, f32)],
) -> Vec<(&'static str, u32, u32)> {
    let n = coords.x.len();
    let mut samples: Vec<u32> = Vec::new();
    let mut state: u64 = 0x9E3779B97F4A7C15;
    for _ in 0..4096 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let id = (state >> 33) as usize % n;
        if coords.p[id] == 0 {
            samples.push(id as u32);
        }
    }
    samples.sort_unstable();
    samples.dedup();

    let dist = |a: u32, b: u32| -> i64 {
        let (ax, ay) = (coords.x[a as usize] as i64, coords.y[a as usize] as i64);
        let (bx, by) = (coords.x[b as usize] as i64, coords.y[b as usize] as i64);
        (ax - bx).abs().max((ay - by).abs())
    };

    let mut corpus: Vec<(&'static str, u32, u32)> = Vec::new();
    let buckets: [(&'static str, i64, i64, usize); 3] = [
        ("short", 50, 200, 4),
        ("medium", 500, 1500, 4),
        ("long", 2500, i64::MAX, 4),
    ];

    'outer: for (name, lo, hi, want) in buckets {
        let mut got = 0usize;
        for i in 0..samples.len() {
            for j in (i + 1)..samples.len() {
                let (a, b) = (samples[i], samples[j]);
                let d = dist(a, b);
                if d < lo || d > hi {
                    continue;
                }
                let res = run_one(view, coords, ctx, globals, a, b);
                if res {
                    corpus.push((name, a, b));
                    got += 1;
                    if got >= want {
                        continue 'outer;
                    }
                }
            }
        }
    }
    corpus
}

fn run_one(
    view: &EngineView,
    _coords: &BenchCoords,
    ctx: &mut SearchContext,
    _globals: &[(u32, f32)],
    start: u32,
    goal: u32,
) -> bool {
    let res = view.astar(
        SearchParams {
            start,
            goal,
            macro_filter: None,
            
            seed: None,
            max_pops: None, cancel: None,
        },
        ctx,
    );
    res.found
}

fn bench_astar(c: &mut Criterion) {
    let path = snapshot_path();
    let snap = match Snapshot::open(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping astar bench: cannot open {path}: {e}");
            return;
        }
    };
    let counts = snap.counts();
    eprintln!(
        "bench snapshot: {} nodes, {} walk edges, {} landmarks",
        counts.nodes, counts.walk_edges, counts.landmarks
    );

    let n = counts.nodes as usize;
    let mut coords = BenchCoords { x: Vec::with_capacity(n), y: Vec::with_capacity(n), p: Vec::with_capacity(n) };
    for id in 0..n as u32 {
        let (x, y, pl) = snap.node_coord(id);
        coords.x.push(x);
        coords.y.push(y);
        coords.p.push(pl);
    }
    let globals = parse_globals(&snap);
    eprintln!("bench globals: {}", globals.len());

    let mut view = EngineView::from_snapshot(&snap);
    view.extra.global = globals.clone();

    let mut ctx = SearchContext::new(view.nodes);
    let corpus = build_corpus(&view, &coords, &mut ctx, &globals);
    for (name, a, b) in &corpus {
        eprintln!(
            "corpus {name}: {a}->{b} ({},{})->({},{})",
            coords.x[*a as usize], coords.y[*a as usize],
            coords.x[*b as usize], coords.y[*b as usize]
        );
    }

    let mut group = c.benchmark_group("astar");
    group.sample_size(10);
    for (name, a, b) in &corpus {
        group.bench_with_input(
            BenchmarkId::new(*name, format!("{a}-{b}")),
            &(*a, *b),
            |bench, &(a, b)| {
                bench.iter(|| {
                    run_one(&view, &coords, &mut ctx, &globals, a, b);
                })
            },
        );
    }
    group.finish();

    // The flood case: a goal that is (almost certainly) unreachable from plane 0 by
    // using an isolated high-plane node. Find one deterministically: the highest node
    // id whose plane is 3. If none exists, skip.
    if let Some(start) = corpus.first().map(|c| c.1) {
        if let Some(goal) = (0..coords.p.len()).rev().find(|&i| coords.p[i] == 3) {
            let goal = goal as u32;
            let res = run_one(&view, &coords, &mut ctx, &globals, start, goal);
            let mut group = c.benchmark_group("astar_hard");
            group.sample_size(10);
            group.bench_function(
                format!("cross_plane_{}", if res { "found" } else { "flood" }),
                |bench| {
                    bench.iter(|| {
                        run_one(&view, &coords, &mut ctx, &globals, start, goal);
                    })
                },
            );
            group.finish();
        }
    }

    // Heuristic microbench: select_active + h_active over a fixed node walk.
    let lm = LandmarkHeuristic {
        nodes: view.nodes,
        landmarks: counts.landmarks as usize,
        tab: snap.lm_tab(),
        quantum: snap.manifest().alt_quantum_ms,
    };
    if let Some(&(_, a, b)) = corpus.last() {
        let mut group = c.benchmark_group("heuristic");
        group.bench_function("select_active", |bench| {
            bench.iter(|| lm.select_active(a, b, navpath_core::engine::heuristics::ACTIVE_LANDMARKS))
        });
        let active = lm.select_active(a, b, navpath_core::engine::heuristics::ACTIVE_LANDMARKS);
        group.bench_function("h_active_1k", |bench| {
            bench.iter(|| {
                let mut acc = 0.0f32;
                for i in 0..1000u32 {
                    let node = (a.wrapping_add(i * 977)) % (view.nodes as u32);
                    acc += lm.h_active(node, &active);
                }
                acc
            })
        });
        group.finish();
    }

    // Engine view construction cost — what the service pays per snapshot load (v8: the
    // walk graph is borrowed zero-copy from the mmap, so this should be trivial).
    {
        let mut group = c.benchmark_group("provider_build");
        group.sample_size(10);
        group.bench_function("engine_view_from_snapshot", |bench| {
            bench.iter(|| EngineView::from_snapshot(&snap))
        });
        group.finish();
    }
}

criterion_group!(benches, bench_astar);
criterion_main!(benches);
