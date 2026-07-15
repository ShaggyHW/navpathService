//! Reproduce a production pair: coords via find_node, all-eligible globals + fairy,
//! unidirectional vs bidirectional, with pops and h-quality diagnostics.
//! Usage: cargo run --release -p navpath-core --example repro_pair -- sx sy sp gx gy gp

use navpath_core::engine::heuristics::ACTIVE_LANDMARKS;
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
                        let lode_only = std::env::var("REPRO_LODESTONES_ONLY").is_ok();
                        for g in arr {
                            let dst = g.get("dst").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                            let mut cost = g.get("cost_ms").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                            let kind = g.get("steps").and_then(|v| v.as_array()).and_then(|a| a.first())
                                .and_then(|s| s.get("kind")).and_then(|v| v.as_str()).unwrap_or("");
                            if lode_only {
                                if kind != "lodestone" { continue; }
                                cost = 2400.0; // hasQuickTele
                            }
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
    let a: Vec<i32> = std::env::args().skip(1).filter_map(|x| x.parse().ok()).collect();
    let (sx, sy, sp, gx, gy, gp) = (a[0], a[1], a[2], a[3], a[4], a[5]);
    let path = std::env::var("NAVPATH_BENCH_SNAPSHOT")
        .unwrap_or_else(|_| format!("{}/../../graph.snapshot", env!("CARGO_MANIFEST_DIR")));
    let snap = Snapshot::open(&path).expect("open snapshot");
    let nodes = snap.counts().nodes as usize;
    let s = snap.find_node(sx, sy, sp).expect("start not in snapshot");
    let g = snap.find_node(gx, gy, gp).expect("goal not in snapshot");
    println!("start node {s}, goal node {g}, landmarks {}", snap.counts().landmarks);

    let mut view = EngineView::from_snapshot(&snap);
    view.extra.global = parse_globals(&snap);
    let mut sources: Vec<u32> = snap.fairy_nodes().to_vec();
    sources.sort_unstable();
    let mut dests: Vec<(u32, f32)> = snap.fairy_nodes().iter().zip(snap.fairy_cost_ms().iter()).map(|(&n, &c)| (n, c)).collect();
    dests.sort_unstable_by(|x, y| x.0.cmp(&y.0));
    view.extra.fairy_sources = sources;
    view.extra.fairy_dests = dests;

    // h quality at the start
    let active = view.lm.select_active(s, g, ACTIVE_LANDMARKS);
    let h0 = view.lm.h_active(s, &active);
    println!("h0 = {h0:.0} (active lms: {:?})", active.indices);

    let macros_rev = NeighborProvider::new(nodes, snap.macro_dst(), snap.macro_src(), snap.macro_w());
    let bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: None };
    let mut ctx = SearchContext::new(nodes);
    let mut cf = SearchContext::new(nodes);
    let mut cb = SearchContext::new(nodes);
    let p = |budget: Option<u32>| SearchParams { start: s, goal: g, macro_filter: None, seed: None, max_pops: budget, cancel: None, bucket_ms: 0.0 };

    let t = std::time::Instant::now();
    let uni = view.astar(p(None), &mut ctx);
    println!("uni  : found={} cost={:.0} pops={} time={:?}", uni.found, uni.cost, uni.pops, t.elapsed());
    let t = std::time::Instant::now();
    let bi = view.astar_bidir(&bp, p(None), &mut cf, &mut cb);
    println!("bidir: found={} cost={:.0} pops={} time={:?}", bi.found, bi.cost, bi.pops, t.elapsed());
    let t = std::time::Instant::now();
    let bib = view.astar_bidir(&bp, p(Some(500_000)), &mut cf, &mut cb);
    println!("bidir@500k: found={} status={:?} pops={} time={:?}", bib.found, bib.status, bib.pops, t.elapsed());
    // Also without any teleports (pure walk) for h_f sanity
    view.extra.global.clear();
    view.extra.fairy_sources.clear();
    view.extra.fairy_dests.clear();
    let t = std::time::Instant::now();
    let plain = view.astar(p(None), &mut ctx);
    println!("walk-only uni: found={} cost={:.0} pops={} time={:?}", plain.found, plain.cost, plain.pops, t.elapsed());
}
