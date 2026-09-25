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
//!   astar_rr       round-robin over ALL corpus pairs per iteration (uni/bidir,
//!                  unseeded and seeded): every other group repeats one pair and so
//!                  only measures the hot-cache case; here consecutive searches differ
//!   astar_gated    lodestone-only quick-tele profile (heavily gated MacroFilter)
//!   astar_teleport goals on global-teleport destinations
//!   astar_virtual  multi-source virtual-start searches (astar_multi)
//!   astar_incident the 2026-07-06 production pair (2887,3535,0)->(3563,3408,0)
//!   astar_hard     cross-plane flood + validated cross-plane found pair
//!   heuristic / provider_build   microbenches (unchanged)
//!
//! Setup cost (efficiency audit T5.12): everything setup derives by *searching* — the
//! corpus pairs and the validated teleport/virtual/hard targets — is a pure function of
//! the snapshot bytes, so it is cached in `<target>/navpath-bench/plan-<tail hash>.txt`
//! (`NAVPATH_BENCH_PLAN_REFRESH=1` recomputes it). Providers (canonical grid, reversed
//! macro CSR, gated filters) are built lazily on the first bench that needs them, so a
//! filtered run (`cargo bench -- astar_bidir/short`) pays only for what it measures.
//!
//! The snapshot is pre-faulted after open, like the service does (T5.9);
//! `NAVPATH_HARNESS_COLD=1` skips that for cold-cache studies.

use std::cell::LazyCell;
use std::hint::black_box;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use navpath_core::engine::canonical::CanonicalGrid;
use navpath_core::engine::heuristics::LandmarkHeuristic;
use navpath_core::engine::neighbors::{MacroFilter, NeighborProvider};
use navpath_core::engine::search::{BidirParams, SearchContext, SearchParams};
use navpath_core::{EngineView, Snapshot};

// Production allocator (the service and builder both run on mimalloc).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Production defaults for the seeded/budgeted groups.
const BENCH_SEED: u64 = 0x5EED;
const BENCH_BUDGET: u32 = 1_500_000;

/// Bump when the plan derivation below changes (it must stay in lockstep with the
/// bench ids that perf-gate baselines are keyed on).
const PLAN_FORMAT: &str = "navpath-bench-plan v1";

fn snapshot_path() -> String {
    std::env::var("NAVPATH_BENCH_SNAPSHOT")
        .unwrap_or_else(|_| format!("{}/../../graph.snapshot", env!("CARGO_MANIFEST_DIR")))
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")))
}

/// Hex of the snapshot's trailing 32-byte blake3 hash (what perf-gate keys on too).
fn snapshot_tail_hash(path: &str) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::End(-32)).ok()?;
    let mut tail = [0u8; 32];
    f.read_exact(&mut tail).ok()?;
    Some(tail.iter().map(|b| format!("{b:02x}")).collect())
}

/// T5.9: pre-fault the mapping (the service does this at load), so the bench measures
/// the engine and not the disk. Skipped for `--list` and under NAVPATH_HARNESS_COLD=1.
fn warm_snapshot(snap: &Snapshot) {
    if std::env::var("NAVPATH_HARNESS_COLD").ok().as_deref() == Some("1") {
        eprintln!("NAVPATH_HARNESS_COLD=1: snapshot not pre-faulted");
        return;
    }
    if std::env::args().any(|a| a == "--list") {
        return;
    }
    let t = Instant::now();
    let bytes = snap.populate();
    eprintln!("snapshot pre-faulted: {:.0} MiB in {:?}", bytes as f64 / (1 << 20) as f64, t.elapsed());
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
                                "poa_item" => 7,
                                "use_on" => 8,
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

/// T5.11: one iteration = one pass over every corpus pair, starting at a rotating
/// offset, so no search runs right after itself and each sample mixes all buckets.
fn round_robin(iters: u64, pairs: &[(u32, u32)], mut run: impl FnMut(u32, u32) -> bool) -> Duration {
    let len = pairs.len();
    let t0 = Instant::now();
    for it in 0..iters {
        let off = (it as usize) % len;
        for k in 0..len {
            let (a, b) = pairs[(off + k) % len];
            black_box(run(a, b));
        }
    }
    t0.elapsed()
}

fn bucket_name(s: &str) -> Option<&'static str> {
    match s {
        "short" => Some("short"),
        "medium" => Some("medium"),
        "long" => Some("long"),
        _ => None,
    }
}

/// Everything setup derives by searching. A pure function of the snapshot bytes (and
/// of the derivation below, versioned by PLAN_FORMAT), so it is cached on disk.
#[derive(Default, PartialEq, Debug)]
struct BenchPlan {
    corpus: Vec<(&'static str, u32, u32)>,
    /// astar_teleport goals (global-teleport destinations) validated reachable from
    /// the first corpus start.
    teleport_goals: Vec<u32>,
    /// astar_virtual (bucket, goal) pairs whose multi-source search finds a route.
    virtual_goals: Vec<(&'static str, u32)>,
    /// astar_hard: the plane-3 goal and whether it is reachable (names the bench).
    hard_cross: Option<(u32, bool)>,
    /// astar_hard: the first reachable plane-1 goal among the deterministic candidates.
    hard_plane1: Option<u32>,
}

impl BenchPlan {
    fn serialize(&self) -> String {
        let mut lines = vec![PLAN_FORMAT.to_string()];
        lines.extend(self.corpus.iter().map(|(n, a, b)| format!("corpus {n} {a} {b}")));
        lines.extend(self.teleport_goals.iter().map(|g| format!("teleport {g}")));
        lines.extend(self.virtual_goals.iter().map(|(n, g)| format!("virtual {n} {g}")));
        if let Some((g, found)) = self.hard_cross {
            lines.push(format!("hard_cross {g} {}", if found { "found" } else { "flood" }));
        }
        if let Some(g) = self.hard_plane1 {
            lines.push(format!("hard_plane1 {g}"));
        }
        lines.push(String::new());
        lines.join("\n")
    }

    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != PLAN_FORMAT {
            return None;
        }
        let mut plan = BenchPlan::default();
        for line in lines {
            let f: Vec<&str> = line.split_whitespace().collect();
            match f.as_slice() {
                [] => {}
                ["corpus", n, a, b] => plan.corpus.push((bucket_name(n)?, a.parse().ok()?, b.parse().ok()?)),
                ["teleport", g] => plan.teleport_goals.push(g.parse().ok()?),
                ["virtual", n, g] => plan.virtual_goals.push((bucket_name(n)?, g.parse().ok()?)),
                ["hard_cross", g, r] => plan.hard_cross = Some((g.parse().ok()?, *r == "found")),
                ["hard_plane1", g] => plan.hard_plane1 = Some(g.parse().ok()?),
                _ => return None,
            }
        }
        Some(plan)
    }
}

/// Deterministic corpus: sample plane-0 nodes with a fixed LCG stride and bucket pairs
/// by octile coordinate distance. Reachability is validated once in setup with a plain
/// search so the measured loop only times found routes (the flood case is measured
/// separately). Unchanged selection: the pair ids name the benches.
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

/// Derive the full plan by searching (the slow path: a quadratic, unbudgeted scan).
fn derive_plan(
    snap: &Snapshot,
    view: &EngineView,
    globals: &[(u32, f32)],
    ctx: &mut SearchContext,
) -> BenchPlan {
    let n = snap.counts().nodes as usize;
    let mut coords = BenchCoords { x: Vec::with_capacity(n), y: Vec::with_capacity(n), p: Vec::with_capacity(n) };
    for id in 0..n as u32 {
        let (x, y, pl) = snap.node_coord(id);
        coords.x.push(x);
        coords.y.push(y);
        coords.p.push(pl);
    }
    let mut plan = BenchPlan { corpus: build_corpus(view, &coords, ctx), ..Default::default() };

    if let (Some(&(_, start, _)), true) = (plan.corpus.first(), !globals.is_empty()) {
        for i in [0usize, globals.len() / 2, globals.len() - 1] {
            let goal = globals[i].0;
            if goal != start && run_uni(view, ctx, start, goal, None, None, None) {
                plan.teleport_goals.push(goal);
            }
        }
    }

    let goals: Vec<(&'static str, u32)> =
        plan.corpus.iter().filter(|(n, _, _)| *n != "short").map(|&(n, _, b)| (n, b)).take(3).collect();
    for (name, goal) in goals {
        let params = SearchParams {
            start: goal, goal, macro_filter: None, seed: None, max_pops: None, cancel: None, bucket_ms: 0.0 };
        if view.astar_multi(globals, params, ctx).found {
            plan.virtual_goals.push((name, goal));
        }
    }

    if let Some(start) = plan.corpus.first().map(|c| c.1) {
        if let Some(goal) = (0..coords.p.len()).rev().find(|&i| coords.p[i] == 3) {
            let goal = goal as u32;
            plan.hard_cross = Some((goal, run_uni(view, ctx, start, goal, None, None, None)));
        }
        let plane1: Vec<u32> = (0..coords.p.len()).filter(|&i| coords.p[i] == 1).map(|i| i as u32).collect();
        if !plane1.is_empty() {
            let step = (plane1.len() / 8).max(1);
            plan.hard_plane1 = plane1
                .iter()
                .step_by(step)
                .take(8)
                .copied()
                .find(|&g| run_uni(view, ctx, start, g, None, None, None));
        }
    }
    plan
}

/// Load the cached plan for this snapshot, or derive and cache it. `view` is only
/// dereferenced (i.e. a lazy view only built) on a cache miss.
fn load_or_derive_plan<'v>(
    snap: &Snapshot,
    snap_path: &str,
    view: &impl std::ops::Deref<Target = EngineView<'v>>,
    globals: &[(u32, f32)],
    ctx: &mut SearchContext,
) -> BenchPlan {
    let refresh = std::env::var("NAVPATH_BENCH_PLAN_REFRESH").ok().as_deref() == Some("1");
    let cache = snapshot_tail_hash(snap_path)
        .map(|h| target_dir().join("navpath-bench").join(format!("plan-{}.txt", &h[..32])));
    if let (Some(path), false) = (&cache, refresh) {
        if let Some(plan) = std::fs::read_to_string(path).ok().and_then(|t| BenchPlan::parse(&t)) {
            eprintln!("bench plan: reused {}", path.display());
            return plan;
        }
    }
    let t = Instant::now();
    let plan = derive_plan(snap, view, globals, ctx);
    eprintln!("bench plan: derived in {:?}", t.elapsed());
    if let Some(path) = &cache {
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let res = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(&tmp, plan.serialize()))
            .and_then(|_| std::fs::rename(&tmp, path));
        match res {
            Ok(()) => eprintln!("bench plan: cached at {}", path.display()),
            Err(e) => eprintln!("bench plan: could not cache at {}: {e}", path.display()),
        }
    }
    plan
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
    warm_snapshot(&snap);

    let n = counts.nodes as usize;
    let globals_full = parse_globals_full(&snap);
    let globals: Vec<(u32, f32)> = globals_full.iter().map(|&(d, w, _)| (d, w)).collect();
    eprintln!("bench globals: {}", globals.len());

    // Everything below is built on first use (T5.12): the bench closures only run for
    // ids matching the criterion filter, so filtered runs skip unrelated setup.

    // Canonical strict-domination pruning: production default (NAVPATH_CANONICAL=0
    // disables), engages on the unseeded groups only — exactly as served traffic.
    let canonical: LazyCell<Option<Arc<CanonicalGrid>>, _> = LazyCell::new(|| {
        if std::env::var("NAVPATH_CANONICAL").ok().as_deref() == Some("0") {
            return None;
        }
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
        .map(Arc::new)
    });
    let view = LazyCell::new(|| {
        let mut v = EngineView::from_snapshot(&snap);
        v.extra.global = globals.clone().into();
        v.canonical = (*canonical).clone();
        v
    });

    // Reversed macro provider for the bidirectional groups (what the service builds at
    // load). No requirement data: the all-eligible profile.
    let macros_rev = LazyCell::new(|| NeighborProvider::new(n, snap.macro_dst(), snap.macro_src(), snap.macro_w()));

    // Gated (lodestone-only quick-tele) profile: only lodestone macro edges eligible,
    // rewritten to the 2400 ms quick-tele cost; globals reduced to lodestones at 2400.
    // Kind data per CSR slot comes from a kinds-aware provider built from the same
    // arrays (identical counting-sort slot order as the view's provider); the
    // providers are dropped once the (fw, rev) filters are extracted.
    let gated_filters: LazyCell<(MacroFilter, MacroFilter), _> = LazyCell::new(|| {
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
        let empty_reqs: Vec<Vec<usize>> = vec![Vec::new(); snap.macro_src().len()];
        let kinds_fw = NeighborProvider::new_with_reqs(
            n, snap.macro_src(), snap.macro_dst(), snap.macro_w(), snap.macro_kind_first(), &empty_reqs,
        );
        let fw = gated_filter_of(&kinds_fw);
        drop(kinds_fw);
        let kinds_rev = NeighborProvider::new_with_reqs(
            n, snap.macro_dst(), snap.macro_src(), snap.macro_w(), snap.macro_kind_first(), &empty_reqs,
        );
        (fw, gated_filter_of(&kinds_rev))
    });
    let view_gated = LazyCell::new(|| {
        let mut v = EngineView::from_snapshot(&snap);
        v.canonical = (*canonical).clone();
        v.extra.global = globals_full
            .iter()
            .filter(|&&(_, _, k)| k == 2)
            .map(|&(d, _, _)| (d, 2400.0))
            .collect::<Vec<_>>()
            .into();
        v
    });

    let mut ctx = SearchContext::new(n);
    let mut cf = SearchContext::new(n);
    let mut cb = SearchContext::new(n);
    let plan = load_or_derive_plan(&snap, &path, &view, &globals, &mut ctx);
    let corpus = &plan.corpus;
    for (name, a, b) in corpus {
        let (ax, ay, _) = snap.node_coord(*a);
        let (bx, by, _) = snap.node_coord(*b);
        eprintln!("corpus {name}: {a}->{b} ({ax},{ay})->({bx},{by})");
    }

    // --- unidirectional baseline (historical group; ids must stay stable) ---
    let mut group = c.benchmark_group("astar");
    group.sample_size(10);
    for (name, a, b) in corpus {
        group.bench_with_input(BenchmarkId::new(*name, format!("{a}-{b}")), &(*a, *b), |bench, &(a, b)| {
            let view = &*view;
            bench.iter(|| run_uni(view, &mut ctx, a, b, None, None, None))
        });
    }
    group.finish();

    // --- bidirectional MM on the same pairs (the production default engine) ---
    let mut group = c.benchmark_group("astar_bidir");
    group.sample_size(10);
    for (name, a, b) in corpus {
        group.bench_with_input(BenchmarkId::new(*name, format!("{a}-{b}")), &(*a, *b), |bench, &(a, b)| {
            let (view, bp) = (&*view, BidirParams { macros_rev: &macros_rev, macro_filter_rev: None });
            bench.iter(|| run_bidir(view, &bp, &mut cf, &mut cb, a, b, None, None, None))
        });
    }
    group.finish();

    // --- seeded + budgeted, uni and bidir: the production request shape ---
    let mut group = c.benchmark_group("astar_seeded");
    group.sample_size(10);
    for (name, a, b) in corpus {
        group.bench_with_input(
            BenchmarkId::new(format!("uni_{name}"), format!("{a}-{b}")),
            &(*a, *b),
            |bench, &(a, b)| {
                let view = &*view;
                bench.iter(|| run_uni(view, &mut ctx, a, b, Some(BENCH_SEED), Some(BENCH_BUDGET), None))
            },
        );
        group.bench_with_input(
            BenchmarkId::new(format!("bidir_{name}"), format!("{a}-{b}")),
            &(*a, *b),
            |bench, &(a, b)| {
                let (view, bp) = (&*view, BidirParams { macros_rev: &macros_rev, macro_filter_rev: None });
                bench.iter(|| {
                    run_bidir(view, &bp, &mut cf, &mut cb, a, b, Some(BENCH_SEED), Some(BENCH_BUDGET), None)
                })
            },
        );
    }
    group.finish();

    // --- round-robin over every corpus pair (T5.11): the not-hot-cache counterpart of
    // the three groups above. Time per iteration = one pass over all pairs. ---
    if !corpus.is_empty() {
        let pairs: Vec<(u32, u32)> = corpus.iter().map(|&(_, a, b)| (a, b)).collect();
        let mut group = c.benchmark_group("astar_rr");
        group.sample_size(10);
        for (id, seeded) in [("uni", false), ("uni_seeded", true)] {
            let (seed, budget) = if seeded { (Some(BENCH_SEED), Some(BENCH_BUDGET)) } else { (None, None) };
            group.bench_function(format!("{id}_all{}", pairs.len()), |bench| {
                let view = &*view;
                bench.iter_custom(|iters| round_robin(iters, &pairs, |a, b| run_uni(view, &mut ctx, a, b, seed, budget, None)))
            });
        }
        for (id, seeded) in [("bidir", false), ("bidir_seeded", true)] {
            let (seed, budget) = if seeded { (Some(BENCH_SEED), Some(BENCH_BUDGET)) } else { (None, None) };
            group.bench_function(format!("{id}_all{}", pairs.len()), |bench| {
                let (view, bp) = (&*view, BidirParams { macros_rev: &macros_rev, macro_filter_rev: None });
                bench.iter_custom(|iters| {
                    round_robin(iters, &pairs, |a, b| run_bidir(view, &bp, &mut cf, &mut cb, a, b, seed, budget, None))
                })
            });
        }
        group.finish();
    }

    // --- gated lodestone-only quick-tele profile on the medium/long pairs ---
    let mut group = c.benchmark_group("astar_gated");
    group.sample_size(10);
    for (name, a, b) in corpus.iter().filter(|(n, _, _)| *n != "short") {
        group.bench_with_input(
            BenchmarkId::new(format!("uni_{name}"), format!("{a}-{b}")),
            &(*a, *b),
            |bench, &(a, b)| {
                let (view_gated, (gated_filter, _)) = (&*view_gated, &*gated_filters);
                bench.iter(|| {
                    run_uni(view_gated, &mut ctx, a, b, None, Some(BENCH_BUDGET), Some(gated_filter))
                })
            },
        );
        group.bench_with_input(
            BenchmarkId::new(format!("bidir_{name}"), format!("{a}-{b}")),
            &(*a, *b),
            |bench, &(a, b)| {
                let (view_gated, (gated_filter, gated_filter_rev)) = (&*view_gated, &*gated_filters);
                let gated_bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: Some(gated_filter_rev) };
                bench.iter(|| {
                    run_bidir(
                        view_gated, &gated_bp, &mut cf, &mut cb,
                        a, b, None, Some(BENCH_BUDGET), Some(gated_filter),
                    )
                })
            },
        );
    }
    group.finish();

    // --- teleport-heavy: goals on global-teleport destinations ---
    if let (Some(&(_, start, _)), true) = (corpus.first(), !globals.is_empty()) {
        let mut group = c.benchmark_group("astar_teleport");
        group.sample_size(10);
        for &goal in &plan.teleport_goals {
            group.bench_with_input(
                BenchmarkId::new("uni", format!("{start}-{goal}")),
                &(start, goal),
                |bench, &(a, b)| {
                    let view = &*view;
                    bench.iter(|| run_uni(view, &mut ctx, a, b, None, None, None))
                },
            );
        }
        group.finish();
    }

    // --- virtual start: one multi-source search over every eligible global ---
    {
        let mut group = c.benchmark_group("astar_virtual");
        group.sample_size(10);
        for &(name, goal) in &plan.virtual_goals {
            group.bench_with_input(BenchmarkId::new("multi", format!("{name}_{goal}")), &goal, |bench, &g| {
                let view = &*view;
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
            let view = &*view;
            bench.iter(|| run_uni(view, &mut ctx, s, g, None, None, None))
        });
        group.bench_function("bidir", |bench| {
            let (view, bp) = (&*view, BidirParams { macros_rev: &macros_rev, macro_filter_rev: None });
            bench.iter(|| run_bidir(view, &bp, &mut cf, &mut cb, s, g, None, None, None))
        });
        group.bench_function("bidir_seeded_budgeted", |bench| {
            let (view, bp) = (&*view, BidirParams { macros_rev: &macros_rev, macro_filter_rev: None });
            bench.iter(|| run_bidir(view, &bp, &mut cf, &mut cb, s, g, Some(BENCH_SEED), Some(BENCH_BUDGET), None))
        });
        group.bench_function("bidir_gated_seeded", |bench| {
            let (view_gated, (gated_filter, gated_filter_rev)) = (&*view_gated, &*gated_filters);
            let gated_bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: Some(gated_filter_rev) };
            bench.iter(|| {
                run_bidir(
                    view_gated, &gated_bp, &mut cf, &mut cb,
                    s, g, Some(BENCH_SEED), Some(BENCH_BUDGET), Some(gated_filter),
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
        if let Some((goal, res)) = plan.hard_cross {
            group.bench_function(
                format!("cross_plane_{}", if res { "found" } else { "flood" }),
                |bench| {
                    let view = &*view;
                    bench.iter(|| run_uni(view, &mut ctx, start, goal, None, None, None))
                },
            );
        }
        if let Some(goal) = plan.hard_plane1 {
            group.bench_function("cross_plane_found_pair", |bench| {
                let view = &*view;
                bench.iter(|| run_uni(view, &mut ctx, start, goal, None, None, None))
            });
        }
        group.finish();
    }

    // Heuristic microbench: select_active + h_active over a fixed node walk.
    let lm = LandmarkHeuristic::from_snapshot(&snap);
    let _ = (n, counts);
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
                    let node = (a.wrapping_add(i * 977)) % (n as u32);
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
