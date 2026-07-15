//! End-to-end A* benchmarks over the real snapshot.
//!
//! Set `NAVPATH_BENCH_SNAPSHOT` to the snapshot path (defaults to `../../graph.snapshot`
//! relative to this crate). The corpus is derived deterministically from the snapshot's
//! node coordinates, so runs before/after a rebuild with the same tile DB are comparable.
//!
//! Groups (docs/optimization_roadmap_v2.md §9.4 — the production-shaped corpus):
//!   astar          unidirectional, unseeded, unbudgeted (the historical baseline)
//!   astar_bidir    bidirectional MM on the same pairs (the production default engine)
//!   astar_seeded   seeded + budgeted (1.5M pops) uni/bidir — the shape both
//!                  production incidents lived in and the old bench never measured
//!   astar_gated    lodestone-only quick-tele profile (heavily gated MacroFilter)
//!   astar_teleport goals on global-teleport destinations
//!   astar_virtual  multi-source virtual-start searches (astar_multi)
//!   astar_incident the 2026-07-06 production pair (2887,3535,0)->(3563,3408,0)
//!   astar_hard     cross-plane flood + validated cross-plane found pair
//!   heuristic / provider_build   microbenches (unchanged)

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use navpath_core::engine::canonical::CanonicalGrid;
use navpath_core::engine::heuristics::LandmarkHeuristic;
use navpath_core::engine::neighbors::{MacroFilter, NeighborProvider};
use navpath_core::engine::search::{BidirParams, SearchContext, SearchParams};
use navpath_core::{EngineView, Snapshot};

/// Production defaults for the seeded/budgeted groups.
const BENCH_SEED: u64 = 0x5EED;
const BENCH_BUDGET: u32 = 1_500_000;

fn snapshot_path() -> String {
    std::env::var("NAVPATH_BENCH_SNAPSHOT")
        .unwrap_or_else(|_| format!("{}/../../graph.snapshot", env!("CARGO_MANIFEST_DIR")))
}

struct BenchCoords {
    x: Vec<i32>,
    y: Vec<i32>,
    p: Vec<i32>,
}

/// Parse the (0,0) macro edge's "global" metadata into (dst, cost, kind_code) triples,
/// mirroring what the service injects as `ExtraEdges::global` on every request.
/// kind_code 2 = lodestone (the quick-tele class).
fn parse_globals_full(snap: &Snapshot) -> Vec<(u32, f32, u32)> {
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
                            let kind = match g
                                .get("steps")
                                .and_then(|v| v.as_array())
                                .and_then(|a| a.first())
                                .and_then(|s| s.get("kind"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                            {
                                "door" => 1,
                                "lodestone" => 2,
                                "npc" => 3,
                                "object" => 4,
                                "item" => 5,
                                "ifslot" => 6,
                                _ => 0,
                            };
                            if dst != 0 {
                                out.push((dst, cost, kind));
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

fn run_uni(
    view: &EngineView,
    ctx: &mut SearchContext,
    start: u32,
    goal: u32,
    seed: Option<u64>,
    max_pops: Option<u32>,
    filter: Option<&MacroFilter>,
) -> bool {
    view.astar(
        SearchParams { start, goal, macro_filter: filter, seed, max_pops, cancel: None, bucket_ms: 0.0 },
        ctx,
    )
    .found
}

#[allow(clippy::too_many_arguments)]
fn run_bidir(
    view: &EngineView,
    bp: &BidirParams,
    cf: &mut SearchContext,
    cb: &mut SearchContext,
    start: u32,
    goal: u32,
    seed: Option<u64>,
    max_pops: Option<u32>,
    filter: Option<&MacroFilter>,
) -> bool {
    view.astar_bidir(
        bp,
        SearchParams { start, goal, macro_filter: filter, seed, max_pops, cancel: None, bucket_ms: 0.0 },
        cf,
        cb,
    )
    .found
}

/// Deterministic corpus: sample plane-0 nodes with a fixed LCG stride and bucket pairs
/// by octile coordinate distance. Reachability is validated once in setup with a plain
/// search so the measured loop only times found routes (the flood case is measured
/// separately).
fn build_corpus(
    view: &EngineView,
    coords: &BenchCoords,
    ctx: &mut SearchContext,
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
                if run_uni(view, ctx, a, b, None, None, None) {
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
    let globals_full = parse_globals_full(&snap);
    let globals: Vec<(u32, f32)> = globals_full.iter().map(|&(d, w, _)| (d, w)).collect();
    eprintln!("bench globals: {}", globals.len());

    // Canonical strict-domination pruning: production default (NAVPATH_CANONICAL=0
    // disables), engages on the unseeded groups only — exactly as served traffic.
    let canonical = if std::env::var("NAVPATH_CANONICAL").ok().as_deref() == Some("0") {
        None
    } else {
        CanonicalGrid::build(
            n,
            snap.coords_packed(),
            snap.walk_offsets(),
            snap.walk_dst(),
            snap.macro_src(),
            snap.macro_dst(),
            snap.macro_w(),
        )
        .ok()
        .map(std::sync::Arc::new)
    };
    let mut view = EngineView::from_snapshot(&snap);
    view.extra.global = globals.clone();
    view.canonical = canonical.clone();

    // Reversed macro provider for the bidirectional groups (what the service builds at
    // load). No requirement data: the all-eligible profile.
    let macros_rev = NeighborProvider::new(n, snap.macro_dst(), snap.macro_src(), snap.macro_w());
    let bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: None };

    // Gated (lodestone-only quick-tele) profile: only lodestone macro edges eligible,
    // rewritten to the 2400 ms quick-tele cost; globals reduced to lodestones at 2400.
    // Kind data per CSR slot comes from a kinds-aware provider built from the same
    // arrays (identical counting-sort slot order as the view's provider).
    let empty_reqs: Vec<Vec<usize>> = vec![Vec::new(); snap.macro_src().len()];
    let kinds_fw = NeighborProvider::new_with_reqs(
        n, snap.macro_src(), snap.macro_dst(), snap.macro_w(), snap.macro_kind_first(), &empty_reqs,
    );
    let kinds_rev = NeighborProvider::new_with_reqs(
        n, snap.macro_dst(), snap.macro_src(), snap.macro_w(), snap.macro_kind_first(), &empty_reqs,
    );
    let gated_filter_of = |p: &NeighborProvider| -> MacroFilter {
        MacroFilter {
            allowed: p.macro_data.iter().map(|d| d.kind_first == 2).collect(),
            w: p
                .macro_edges
                .w
                .iter()
                .zip(p.macro_data.iter())
                .map(|(&w, d)| if d.kind_first == 2 { 2400.0 } else { w })
                .collect(),
        }
    };
    let gated_filter = gated_filter_of(&kinds_fw);
    let gated_filter_rev = gated_filter_of(&kinds_rev);
    let gated_bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: Some(&gated_filter_rev) };
    let mut view_gated = EngineView::from_snapshot(&snap);
    view_gated.canonical = canonical.clone();
    view_gated.extra.global = globals_full
        .iter()
        .filter(|&&(_, _, k)| k == 2)
        .map(|&(d, _, _)| (d, 2400.0))
        .collect();

    let mut ctx = SearchContext::new(view.nodes);
    let mut cf = SearchContext::new(view.nodes);
    let mut cb = SearchContext::new(view.nodes);
    let corpus = build_corpus(&view, &coords, &mut ctx);
    for (name, a, b) in &corpus {
        eprintln!(
            "corpus {name}: {a}->{b} ({},{})->({},{})",
            coords.x[*a as usize], coords.y[*a as usize],
            coords.x[*b as usize], coords.y[*b as usize]
        );
    }

    // --- unidirectional baseline (historical group; ids must stay stable) ---
    let mut group = c.benchmark_group("astar");
    group.sample_size(10);
    for (name, a, b) in &corpus {
        group.bench_with_input(BenchmarkId::new(*name, format!("{a}-{b}")), &(*a, *b), |bench, &(a, b)| {
            bench.iter(|| run_uni(&view, &mut ctx, a, b, None, None, None))
        });
    }
    group.finish();

    // --- bidirectional MM on the same pairs (the production default engine) ---
    let mut group = c.benchmark_group("astar_bidir");
    group.sample_size(10);
    for (name, a, b) in &corpus {
        group.bench_with_input(BenchmarkId::new(*name, format!("{a}-{b}")), &(*a, *b), |bench, &(a, b)| {
            bench.iter(|| run_bidir(&view, &bp, &mut cf, &mut cb, a, b, None, None, None))
        });
    }
    group.finish();

    // --- seeded + budgeted, uni and bidir: the production request shape ---
    let mut group = c.benchmark_group("astar_seeded");
    group.sample_size(10);
    for (name, a, b) in &corpus {
        group.bench_with_input(
            BenchmarkId::new(format!("uni_{name}"), format!("{a}-{b}")),
            &(*a, *b),
            |bench, &(a, b)| {
                bench.iter(|| run_uni(&view, &mut ctx, a, b, Some(BENCH_SEED), Some(BENCH_BUDGET), None))
            },
        );
        group.bench_with_input(
            BenchmarkId::new(format!("bidir_{name}"), format!("{a}-{b}")),
            &(*a, *b),
            |bench, &(a, b)| {
                bench.iter(|| {
                    run_bidir(&view, &bp, &mut cf, &mut cb, a, b, Some(BENCH_SEED), Some(BENCH_BUDGET), None)
                })
            },
        );
    }
    group.finish();

    // --- gated lodestone-only quick-tele profile on the medium/long pairs ---
    let mut group = c.benchmark_group("astar_gated");
    group.sample_size(10);
    for (name, a, b) in corpus.iter().filter(|(n, _, _)| *n != "short") {
        group.bench_with_input(
            BenchmarkId::new(format!("uni_{name}"), format!("{a}-{b}")),
            &(*a, *b),
            |bench, &(a, b)| {
                bench.iter(|| {
                    run_uni(&view_gated, &mut ctx, a, b, None, Some(BENCH_BUDGET), Some(&gated_filter))
                })
            },
        );
        group.bench_with_input(
            BenchmarkId::new(format!("bidir_{name}"), format!("{a}-{b}")),
            &(*a, *b),
            |bench, &(a, b)| {
                bench.iter(|| {
                    run_bidir(
                        &view_gated, &gated_bp, &mut cf, &mut cb,
                        a, b, None, Some(BENCH_BUDGET), Some(&gated_filter),
                    )
                })
            },
        );
    }
    group.finish();

    // --- teleport-heavy: goals on global-teleport destinations ---
    if let (Some(&(_, start, _)), true) = (corpus.first(), !globals.is_empty()) {
        let picks = [0usize, globals.len() / 2, globals.len() - 1];
        let mut group = c.benchmark_group("astar_teleport");
        group.sample_size(10);
        for &i in &picks {
            let goal = globals[i].0;
            if goal == start || !run_uni(&view, &mut ctx, start, goal, None, None, None) {
                continue;
            }
            group.bench_with_input(
                BenchmarkId::new("uni", format!("{start}-{goal}")),
                &(start, goal),
                |bench, &(a, b)| bench.iter(|| run_uni(&view, &mut ctx, a, b, None, None, None)),
            );
        }
        group.finish();
    }

    // --- virtual start: one multi-source search over every eligible global ---
    {
        let mut group = c.benchmark_group("astar_virtual");
        group.sample_size(10);
        let goals: Vec<(&str, u32)> = corpus
            .iter()
            .filter(|(n, _, _)| *n != "short")
            .map(|&(n, _, b)| (n, b))
            .take(3)
            .collect();
        for (name, goal) in goals {
            let params = SearchParams {
                start: goal, goal, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 };
            if !view.astar_multi(&globals, params, &mut ctx).found {
                continue;
            }
            group.bench_with_input(BenchmarkId::new("multi", format!("{name}_{goal}")), &goal, |bench, &g| {
                bench.iter(|| {
                    let params = SearchParams {
                        start: g, goal: g, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 };
                    view.astar_multi(&globals, params, &mut ctx).found
                })
            });
        }
        group.finish();
    }

    // --- the 2026-07-06 production incident pair, in its production shapes ---
    if let (Some(s), Some(g)) = (snap.find_node(2887, 3535, 0), snap.find_node(3563, 3408, 0)) {
        let mut group = c.benchmark_group("astar_incident");
        group.sample_size(10);
        group.bench_function("uni", |bench| {
            bench.iter(|| run_uni(&view, &mut ctx, s, g, None, None, None))
        });
        group.bench_function("bidir", |bench| {
            bench.iter(|| run_bidir(&view, &bp, &mut cf, &mut cb, s, g, None, None, None))
        });
        group.bench_function("bidir_seeded_budgeted", |bench| {
            bench.iter(|| run_bidir(&view, &bp, &mut cf, &mut cb, s, g, Some(BENCH_SEED), Some(BENCH_BUDGET), None))
        });
        group.bench_function("bidir_gated_seeded", |bench| {
            bench.iter(|| {
                run_bidir(
                    &view_gated, &gated_bp, &mut cf, &mut cb,
                    s, g, Some(BENCH_SEED), Some(BENCH_BUDGET), Some(&gated_filter),
                )
            })
        });
        group.finish();
    } else {
        eprintln!("skipping astar_incident: pair coords not in this snapshot");
    }

    // The flood case: a goal that is (almost certainly) unreachable from plane 0 by
    // using an isolated high-plane node, plus a VALIDATED cross-plane found pair (the
    // old bench only ever timed the flood).
    if let Some(start) = corpus.first().map(|c| c.1) {
        let mut group = c.benchmark_group("astar_hard");
        group.sample_size(10);
        if let Some(goal) = (0..coords.p.len()).rev().find(|&i| coords.p[i] == 3) {
            let goal = goal as u32;
            let res = run_uni(&view, &mut ctx, start, goal, None, None, None);
            group.bench_function(
                format!("cross_plane_{}", if res { "found" } else { "flood" }),
                |bench| bench.iter(|| run_uni(&view, &mut ctx, start, goal, None, None, None)),
            );
        }
        // First reachable plane-1 goal among a few deterministic candidates.
        let plane1: Vec<u32> = (0..coords.p.len()).filter(|&i| coords.p[i] == 1).map(|i| i as u32).collect();
        if !plane1.is_empty() {
            let step = (plane1.len() / 8).max(1);
            if let Some(&goal) = plane1
                .iter()
                .step_by(step)
                .take(8)
                .find(|&&g| run_uni(&view, &mut ctx, start, g, None, None, None))
            {
                group.bench_function("cross_plane_found_pair", |bench| {
                    bench.iter(|| run_uni(&view, &mut ctx, start, goal, None, None, None))
                });
            }
        }
        group.finish();
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
