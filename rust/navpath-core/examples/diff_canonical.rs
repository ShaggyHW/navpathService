//! Differential + measurement harness for canonical strict-domination pruning
//! (roadmap Phase E Stage 2a). Full expansion vs canonical pruning must agree on
//! found flags and optimal costs for every unseeded pair, unidirectional AND
//! bidirectional; pruning is cost-exact (every optimal path survives), so only
//! equal-cost tie paths — and therefore final-ulp f32 sums — may differ.
//!
//!   cargo run --release -p navpath-core --example diff_canonical -- 500
//!
//! Also reports the pops and wall-time deltas — the Stage 2a go/no-go measurement.

use std::sync::Arc;

use navpath_core::engine::canonical::CanonicalGrid;
use navpath_core::engine::neighbors::{NeighborProvider, WalkGraph};
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
    let n_pairs: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(300);
    // --gated: lodestone-only quick-tele profile (weak, inconsistent-h shape) — the
    // bench group where canonical pruning showed a pathological slowdown.
    let gated = std::env::args().any(|a| a == "--gated");
    // --pair S G: check exactly one (start, goal) id pair instead of random ones.
    let explicit_pair: Option<(u32, u32)> = {
        let args: Vec<String> = std::env::args().collect();
        args.iter().position(|a| a == "--pair").and_then(|i| {
            Some((args.get(i + 1)?.parse().ok()?, args.get(i + 2)?.parse().ok()?))
        })
    };
    let path = std::env::var("NAVPATH_BENCH_SNAPSHOT")
        .unwrap_or_else(|_| format!("{}/../../graph.snapshot", env!("CARGO_MANIFEST_DIR")));
    let snap = Snapshot::open(&path).expect("open snapshot");
    let nodes = snap.counts().nodes as usize;

    let t = std::time::Instant::now();
    let grid = Arc::new(
        CanonicalGrid::build(
            nodes,
            snap.coords_packed(),
            snap.walk_offsets(),
            snap.walk_dst(),
            snap.macro_src(),
            snap.macro_dst(),
            snap.macro_w(),
        )
        .expect("canonical grid preconditions hold"),
    );
    println!("canonical grid built in {:?} ({} nodes)", t.elapsed(), nodes);

    let mk_view = |canonical: Option<Arc<CanonicalGrid>>| -> EngineView {
        let mut view = EngineView::from_snapshot(&snap);
        view.extra.global = parse_globals(&snap);
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
        view.canonical = canonical;
        view
    };
    let mut full = mk_view(None);
    let mut canon = mk_view(Some(grid));
    if gated {
        // Mirror the bench's view_gated exactly: lodestone globals at the quick-tele
        // cost, no fairy extras.
        let lode: Vec<(u32, f32)> = {
            let msrc = snap.macro_src();
            let mut out = Vec::new();
            for idx in 0..msrc.len() {
                if msrc[idx] == 0 && snap.macro_dst()[idx] == 0 {
                    if let Some(bytes) = snap.macro_meta_at(idx) {
                        if let Ok(val) = serde_json::from_slice::<serde_json::Value>(bytes) {
                            if let Some(arr) = val.get("global").and_then(|v| v.as_array()) {
                                for g in arr {
                                    let dst = g.get("dst").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                    let kind = g.get("steps").and_then(|v| v.as_array())
                                        .and_then(|a| a.first()).and_then(|s| s.get("kind"))
                                        .and_then(|v| v.as_str()).unwrap_or("");
                                    if dst != 0 && kind == "lodestone" {
                                        out.push((dst, 2400.0));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            out.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.partial_cmp(&b.1).unwrap()));
            out
        };
        for v in [&mut full, &mut canon] {
            v.extra.global = lode.clone();
            v.extra.fairy_sources.clear();
            v.extra.fairy_dests.clear();
        }
    }
    // sanity: canonical view really is a CSR walk with coords
    assert!(matches!(canon.walk, WalkGraph::Csr { .. }) && canon.coords.is_some());

    // Gated profile: only lodestone macro edges, at the 2400 ms quick-tele cost;
    // globals reduced to lodestones (mirrors the bench's astar_gated group).
    let empty_reqs: Vec<Vec<usize>> = vec![Vec::new(); snap.macro_src().len()];
    let kinds_fw = NeighborProvider::new_with_reqs(
        nodes, snap.macro_src(), snap.macro_dst(), snap.macro_w(), snap.macro_kind_first(), &empty_reqs,
    );
    let gated_filter = navpath_core::engine::neighbors::MacroFilter {
        allowed: kinds_fw.macro_data.iter().map(|d| d.kind_first == 2).collect(),
        w: kinds_fw.macro_edges.w.iter().zip(kinds_fw.macro_data.iter())
            .map(|(&w, d)| if d.kind_first == 2 { 2400.0 } else { w }).collect(),
    };
    let filter: Option<&navpath_core::engine::neighbors::MacroFilter> =
        if gated { Some(&gated_filter) } else { None };

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
    let mut mismatches = 0usize;
    let (mut pops_full, mut pops_canon) = (0u64, 0u64);
    let (mut pops_full_bi, mut pops_canon_bi) = (0u64, 0u64);
    let (mut t_full, mut t_canon) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    let (mut t_full_bi, mut t_canon_bi) = (std::time::Duration::ZERO, std::time::Duration::ZERO);

    while checked < if explicit_pair.is_some() { 1 } else { n_pairs } {
        let (s, g) = explicit_pair.unwrap_or_else(|| (next(nodes), next(nodes)));
        if s == g {
            continue;
        }
        checked += 1;
        let params = || SearchParams {
            start: s, goal: g, macro_filter: filter, seed: None,
            max_pops: Some(1_500_000), cancel: None, bucket_ms: 0.0,
        };
        let t0 = std::time::Instant::now();
        let a = full.astar(params(), &mut ctx);
        t_full += t0.elapsed();
        let t0 = std::time::Instant::now();
        let b = canon.astar(params(), &mut ctx);
        t_canon += t0.elapsed();
        let t0 = std::time::Instant::now();
        let abi = full.astar_bidir(&bp, params(), &mut cf, &mut cb);
        t_full_bi += t0.elapsed();
        let t0 = std::time::Instant::now();
        let bbi = canon.astar_bidir(&bp, params(), &mut cf, &mut cb);
        t_canon_bi += t0.elapsed();

        pops_full += a.pops as u64;
        pops_canon += b.pops as u64;
        pops_full_bi += abi.pops as u64;
        pops_canon_bi += bbi.pops as u64;

        for (label, x, y) in [("uni", &a, &b), ("bidir", &abi, &bbi)] {
            if x.found != y.found
                || (x.found && (x.cost - y.cost).abs() > 1e-4 * x.cost.max(1.0))
            {
                mismatches += 1;
                println!(
                    "MISMATCH {label} {s}->{g}: full found={} cost={} | canonical found={} cost={}",
                    x.found, x.cost, y.found, y.cost
                );
            }
        }
    }
    println!(
        "checked {checked} pairs: {mismatches} mismatches\n\
         uni   pops {pops_full} -> {pops_canon} ({:.2}x), wall {:?} -> {:?} ({:.2}x)\n\
         bidir pops {pops_full_bi} -> {pops_canon_bi} ({:.2}x), wall {:?} -> {:?} ({:.2}x)",
        pops_full as f64 / pops_canon.max(1) as f64,
        t_full, t_canon,
        t_full.as_secs_f64() / t_canon.as_secs_f64().max(1e-9),
        pops_full_bi as f64 / pops_canon_bi.max(1) as f64,
        t_full_bi, t_canon_bi,
        t_full_bi.as_secs_f64() / t_canon_bi.as_secs_f64().max(1e-9),
    );
    std::process::exit(if mismatches == 0 { 0 } else { 1 });
}
