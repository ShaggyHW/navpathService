//! Diagnostic probe: for a few (start, goal) pairs, print the ALT bound at the start,
//! the true optimal cost, the pop count, and the bound tightness ratio.
//!
//! Usage: cargo run --release -p navpath-core --example probe [-- start goal]...
//! Snapshot path from NAVPATH_BENCH_SNAPSHOT (defaults to ../../graph.snapshot).

use navpath_core::engine::heuristics::ACTIVE_LANDMARKS;
use navpath_core::engine::search::{SearchContext, SearchParams};
use navpath_core::{EngineView, Snapshot};

fn main() {
    let path = std::env::var("NAVPATH_BENCH_SNAPSHOT")
        .unwrap_or_else(|_| format!("{}/../../graph.snapshot", env!("CARGO_MANIFEST_DIR")));
    let snap = Snapshot::open(&path).expect("open snapshot");
    let view = EngineView::from_snapshot(&snap);
    let mut ctx = SearchContext::new(view.nodes);

    let args: Vec<u32> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let pairs: Vec<(u32, u32)> = if args.len() >= 2 && args.len() % 2 == 0 {
        args.chunks(2).map(|c| (c[0], c[1])).collect()
    } else {
        // Default: the bench corpus's slow medium/long pairs plus a short one.
        vec![(923, 1957), (392, 74053), (392, 74489), (392, 923), (392, 1310)]
    };

    for (s, g) in pairs {
        let active = view.lm.select_active(s, g, ACTIVE_LANDMARKS);
        let h0 = view.lm.h_active(s, &active);
        let t = std::time::Instant::now();
        let res = view.astar(
            SearchParams { start: s, goal: g, macro_filter: None, seed: None, max_pops: None, cancel: None },
            &mut ctx,
        );
        let el = t.elapsed();
        let (sx, sy, _) = snap.node_coord(s);
        let (gx, gy, _) = snap.node_coord(g);
        println!(
            "{}->{} ({},{})->({},{}): h0={:.0} cost={:.0} ratio={:.3} pops={} path_len={} time={:?} active_lms={:?}",
            s, g,
            sx, sy, gx, gy,
            h0, res.cost,
            if res.cost > 0.0 { h0 / res.cost } else { 0.0 },
            res.pops, res.path.len(), el, active.indices
        );
    }
}
