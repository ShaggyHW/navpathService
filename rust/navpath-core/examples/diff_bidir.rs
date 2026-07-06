//! Differential verification: bidirectional A* must return exactly the same cost as
//! unidirectional A* for every (start, goal) pair. Run against the real snapshot:
//!
//!   cargo run --release -p navpath-core --example diff_bidir -- 500
//!
//! Pairs are LCG-deterministic; the eligible-global set is the full parsed global list
//! (the service's per-request masks only shrink it, which both searches see equally).

use navpath_core::engine::neighbors::NeighborProvider;
use navpath_core::engine::search::{BidirParams, SearchContext, SearchParams};
use navpath_core::{EngineView, Snapshot};

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
                            if dst != 0 {
                                out.push((dst, cost));
                            }
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
    let nodes = snap.counts().nodes as usize;

    let mut view = EngineView::from_snapshot(&snap);
    view.extra.global = parse_globals(&snap);
    // Fairy clique from the snapshot (all rings, mirroring an all-eligible profile).
    let mut sources: Vec<u32> = snap.fairy_nodes().to_vec();
    sources.sort_unstable();
    let mut dests: Vec<(u32, f32)> = snap
        .fairy_nodes()
        .iter()
        .zip(snap.fairy_cost_ms().iter())
        .map(|(&n, &c)| (n, c))
        .collect();
    dests.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    view.extra.fairy_sources = sources;
    view.extra.fairy_dests = dests;

    // Reversed macro provider from the snapshot's raw arrays.
    let macros_rev = NeighborProvider::new(nodes, snap.macro_dst(), snap.macro_src(), snap.macro_w());
    let bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: None };

    let mut ctx = SearchContext::new(nodes);
    let mut cf = SearchContext::new(nodes);
    let mut cb = SearchContext::new(nodes);

    let mut state: u64 = 0xDEADBEEFCAFEF00D;
    let mut next = |m: usize| -> u32 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((state >> 33) as usize % m) as u32
    };

    let mut checked = 0usize;
    let mut found_ct = 0usize;
    let mut mismatches = 0usize;
    let mut uni_pops: u64 = 0;
    let mut bi_pops: u64 = 0;
    let t0 = std::time::Instant::now();
    while checked < n_pairs {
        let s = next(nodes);
        let g = next(nodes);
        if s == g { continue; }
        let params = || SearchParams { start: s, goal: g, macro_filter: None, seed: None, max_pops: Some(2_000_000), cancel: None };
        let uni = view.astar(params(), &mut ctx);
        let bi = view.astar_bidir(&bp, params(), &mut cf, &mut cb);
        checked += 1;
        uni_pops += uni.pops as u64;
        bi_pops += bi.pops as u64;
        if uni.found != bi.found
            || (uni.found && (uni.cost - bi.cost).abs() > 1e-3 * uni.cost.max(1.0))
        {
            mismatches += 1;
            println!(
                "MISMATCH {s}->{g}: uni found={} cost={} pops={} | bidir found={} cost={} pops={}",
                uni.found, uni.cost, uni.pops, bi.found, bi.cost, bi.pops
            );
        }
        if uni.found {
            found_ct += 1;
        }
    }
    println!(
        "checked {checked} pairs ({found_ct} found) in {:?}: {mismatches} mismatches; pops uni={uni_pops} bidir={bi_pops} ({:.2}x)",
        t0.elapsed(),
        uni_pops as f64 / bi_pops.max(1) as f64
    );
    std::process::exit(if mismatches == 0 { 0 } else { 1 });
}
