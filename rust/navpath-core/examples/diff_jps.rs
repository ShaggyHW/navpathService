//! Differential + performance check for jump-point expansion (Phase E Stage 3):
//! for LCG pairs, run plain unidirectional A*, JPS unidirectional, and bidirectional
//! MM (all unseeded, all-eligible profile) and assert the three costs agree; report
//! pops and wall per cost bucket. Also validates every JPS path: tile-adjacent, walk
//! edges exist, and the re-costed path equals the reported cost.
//!
//!   cargo run --release -p navpath-core --example diff_jps -- 300
//!
//! The three engines run in a per-pair rotated order so none is systematically timed
//! with a cache the others warmed.
use navpath_core::engine::canonical::CanonicalGrid;
use navpath_core::engine::neighbors::NeighborProvider;
use navpath_core::engine::search::{BidirParams, SearchContext, SearchParams};
use navpath_core::{EngineView, SearchResult, Snapshot};
use std::collections::HashMap;

// Production allocator (the service and builder both run on mimalloc; efficiency audit
// T5.14) so allocation-heavy paths are timed as they run in production.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Pre-fault the snapshot like the service does at load, so timings measure the engine
/// rather than disk reads (efficiency audit T5.9). `NAVPATH_HARNESS_COLD=1` skips it for
/// cold-cache studies.
fn warm_snapshot(snap: &Snapshot) {
    if std::env::var("NAVPATH_HARNESS_COLD").ok().as_deref() == Some("1") {
        eprintln!("NAVPATH_HARNESS_COLD=1: snapshot not pre-faulted");
        return;
    }
    let t = std::time::Instant::now();
    let bytes = snap.populate();
    eprintln!("snapshot pre-faulted: {:.0} MiB in {:?}", bytes as f64 / (1 << 20) as f64, t.elapsed());
}

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
                            let cost = g.get("cost_ms").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                            if dst != 0 { out.push((dst, cost)); }
                        }
                    }
                }
            }
        }
    }
    out.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.partial_cmp(&b.1).unwrap()));
    out
}

fn main() {
    let n_pairs: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(200);
    let path = std::env::var("NAVPATH_BENCH_SNAPSHOT")
        .unwrap_or_else(|_| format!("{}/../../graph.snapshot", env!("CARGO_MANIFEST_DIR")));
    let snap = Snapshot::open(&path).expect("open snapshot");
    warm_snapshot(&snap);
    let nodes = snap.counts().nodes as usize;
    let mut view = EngineView::from_snapshot(&snap);
    view.extra.global = parse_globals(&snap).into();
    let mut sources: Vec<u32> = snap.fairy_nodes().to_vec();
    sources.sort_unstable();
    let mut dests: Vec<(u32, f32)> = snap.fairy_nodes().iter().zip(snap.fairy_cost_ms().iter()).map(|(&n, &c)| (n, c)).collect();
    dests.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    view.extra.fairy_sources = sources.into();
    view.extra.fairy_dests = dests.into();
    let mut cg = CanonicalGrid::build(
        nodes, snap.coords_packed(), snap.walk_offsets(), snap.walk_dst(), snap.macro_src(), snap.macro_dst(), snap.macro_w(),
    ).expect("canonical grid");
    cg.add_stop_nodes(snap.fairy_nodes());
    let t = std::time::Instant::now();
    cg.build_jump_tables(snap.walk_offsets(), snap.walk_dst(), snap.coords_packed());
    eprintln!("jump tables built in {:?}", t.elapsed());
    view.canonical = Some(std::sync::Arc::new(cg));
    let macros_rev = NeighborProvider::new(nodes, snap.macro_dst(), snap.macro_src(), snap.macro_w());
    let bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: None };
    let mut ctx = SearchContext::new(nodes);
    let mut cf = SearchContext::new(nodes);
    let mut cb = SearchContext::new(nodes);
    // Edge lookups for path validation, indexed once instead of linear scans per hop
    // (efficiency audit T5.15). Same answers as the scans they replace: the minimum
    // weight over parallel macro edges (folded in index order), and the FIRST
    // (dst, cost) entry in array order for globals / fairy destinations.
    let macro_min: HashMap<(u32, u32), f32> = {
        let (ms, md, mw) = (snap.macro_src(), snap.macro_dst(), snap.macro_w());
        let mut m = HashMap::with_capacity(ms.len());
        for i in 0..ms.len() {
            m.entry((ms[i], md[i])).and_modify(|b: &mut f32| *b = b.min(mw[i])).or_insert(mw[i]);
        }
        m
    };
    let first_by_dst = |entries: &[(u32, f32)]| -> HashMap<u32, f32> {
        let mut m = HashMap::with_capacity(entries.len());
        for &(d, c) in entries {
            m.entry(d).or_insert(c);
        }
        m
    };
    let global_cost = first_by_dst(&view.extra.global);
    let fairy_cost = first_by_dst(&view.extra.fairy_dests);
    let macro_w = |u: u32, v: u32| -> Option<f32> { macro_min.get(&(u, v)).copied() };
    let recost = |p: &[u32]| -> Option<f32> {
        let mut t = 0f32;
        for w in p.windows(2) {
            let c = snap.walk_edge_weight(w[0], w[1])
                .or_else(|| macro_w(w[0], w[1]))
                .or_else(|| global_cost.get(&w[1]).copied())
                .or_else(|| fairy_cost.get(&w[1]).copied())?;
            t += c;
        }
        Some(t)
    };

    let mut state: u64 = 0xDEADBEEFCAFEF00D;
    let mut next = |m: usize| -> u32 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((state >> 33) as usize % m) as u32
    };
    let bnames = ["<20s", "20-60s", "60-120s", ">=120s"];
    // per bucket: n, t_uni, t_jps, t_bi, pops_uni, pops_jps, pops_bi, jps_beats_bi
    let mut agg = [[0f64; 8]; 4];
    let (mut checked, mut mismatches, mut bad_paths) = (0usize, 0usize, 0usize);
    let mut pair_idx = 0usize; // every attempted pair, for the engine-order rotation
    let t0 = std::time::Instant::now();
    while checked < n_pairs {
        let s = next(nodes);
        let g = next(nodes);
        if s == g { continue; }
        let params = || SearchParams { start: s, goal: g, macro_filter: None, seed: None, max_pops: Some(3_000_000), cancel: None, bucket_ms: 0.0 };
        // Rotate the engine order per pair (efficiency audit T5.10): whichever engine
        // runs later finds more of the pair's working set cached, so a fixed order
        // flatters it. Engine e: 0 = uni, 1 = JPS, 2 = bidir. The untimed warm-up (so
        // the first timed engine does not pay the page-cache cost) is done by the
        // engine that runs last, so it rotates too and no engine is favoured.
        let order = [pair_idx % 3, (pair_idx + 1) % 3, (pair_idx + 2) % 3];
        let mut runs: [Option<(SearchResult, f64)>; 3] = [None, None, None];
        for (k, e) in std::iter::once(order[2]).chain(order).enumerate() {
            view.jps = e == 1;
            let t = std::time::Instant::now();
            let r = if e == 2 {
                view.astar_bidir(&bp, params(), &mut cf, &mut cb)
            } else {
                view.astar(params(), &mut ctx)
            };
            if k > 0 {
                runs[e] = Some((r, t.elapsed().as_secs_f64() * 1e6));
            }
        }
        view.jps = false;
        pair_idx += 1;
        let [uni, jps, bi] = runs.map(|r| r.expect("every engine ran"));
        let ((uni, tu), (jps, tj), (bi, tb)) = (uni, jps, bi);
        if uni.found != jps.found || uni.found != bi.found {
            mismatches += 1;
            eprintln!("FOUND MISMATCH {s}->{g}: uni {} jps {} bidir {}", uni.found, jps.found, bi.found);
            checked += 1;
            continue;
        }
        if !uni.found { continue; }
        checked += 1;
        let tol = 1e-4 * uni.cost.max(1.0);
        if (uni.cost - jps.cost).abs() > tol || (uni.cost - bi.cost).abs() > tol {
            mismatches += 1;
            eprintln!("COST MISMATCH {s}->{g}: uni {:.3} ({} pops) jps {:.3} ({} pops) bidir {:.3}", uni.cost, uni.pops, jps.cost, jps.pops, bi.cost);
        }
        // JPS path validity
        let ok_len = jps.path_g.len() == jps.path.len() && (jps.path_g.last().copied().unwrap_or(f32::NAN) - jps.cost).abs() < 1e-3;
        let ok_edges = jps.path.windows(2).all(|w| {
            snap.walk_edge_weight(w[0], w[1]).is_some() || macro_w(w[0], w[1]).is_some()
                || global_cost.contains_key(&w[1]) || fairy_cost.contains_key(&w[1])
        });
        let ok_cost = recost(&jps.path).map_or(false, |c| (c - jps.cost).abs() <= 1e-4 * jps.cost.max(1.0));
        if !(ok_len && ok_edges && ok_cost && jps.path[0] == s && *jps.path.last().unwrap() == g) {
            bad_paths += 1;
            eprintln!("BAD JPS PATH {s}->{g}: len_ok={ok_len} edges_ok={ok_edges} cost_ok={ok_cost} recost={:?} cost={}", recost(&jps.path), jps.cost);
        }
        println!("{s},{g},{:.1},{:.0},{:.0},{:.0},{},{},{},{}", uni.cost / 1000.0, tu, tj, tb, uni.pops, jps.pops, bi.pops, jps.path.len());
        let b = if uni.cost < 20_000.0 { 0 } else if uni.cost < 60_000.0 { 1 } else if uni.cost < 120_000.0 { 2 } else { 3 };
        let a = &mut agg[b];
        a[0] += 1.0; a[1] += tu; a[2] += tj; a[3] += tb;
        a[4] += uni.pops as f64; a[5] += jps.pops as f64; a[6] += bi.pops as f64;
        if tj < tb { a[7] += 1.0; }
    }
    eprintln!("\n{checked} pairs in {:?}: {mismatches} cost/found mismatches, {bad_paths} invalid JPS paths", t0.elapsed());
    eprintln!("{:8} {:4} {:>9} {:>9} {:>9} {:>10} {:>10} {:>10} {:>8}", "bucket", "n", "uni_ms", "jps_ms", "bidir_ms", "uni_pops", "jps_pops", "bidir_pops", "jps<bi");
    for b in 0..4 {
        let a = agg[b]; if a[0] == 0.0 { continue; }
        eprintln!("{:8} {:4} {:9.1} {:9.1} {:9.1} {:10.0} {:10.0} {:10.0} {:7.0}%", bnames[b], a[0], a[1]/1e3, a[2]/1e3, a[3]/1e3, a[4]/a[0], a[5]/a[0], a[6]/a[0], 100.0*a[7]/a[0]);
    }
    std::process::exit(if mismatches == 0 && bad_paths == 0 { 0 } else { 1 });
}
