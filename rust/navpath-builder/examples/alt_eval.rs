//! ALT table evaluation across snapshots built from the same DB (landmark A/B).
//!
//!   cargo run --release -p navpath-builder --example alt_eval -- stats A.snapshot [B.snapshot ...]
//!   cargo run --release -p navpath-builder --example alt_eval -- bench N A.snapshot [B.snapshot ...]
//!
//! `stats`: per snapshot, how usable its landmarks are to the runtime heuristic
//! (`select_active` needs both goal entries < SATURATED): columns usable for the whole
//! main SCC, nodes with no usable column (h = 0 when they are the goal), where the
//! global-teleport destinations sit, and how many columns survive the bidir backward
//! anchor aggregation (`select_active_rev`) for an origin in the main SCC.
//!
//! `bench`: N LCG-deterministic (start, goal) pairs (uniform over nodes, like
//! `diff_bidir`), eligible globals (ALT_EVAL_GLOBALS=all|nopocket|main) and the full
//! fairy clique, no budget. For each pair every snapshot runs uni then bidir A*
//! (snapshot order rotated per pair against fixed-order cache bias, after one untimed
//! warm-up pass). Costs must agree across snapshots and engines for every pair
//! (optimality); any disagreement is reported and fails the run. Pops and wall time are
//! summarized per snapshot, engine and goal class over the REACHABLE pairs (the
//! service's exact component precheck rejects unreachable goals before any search).
//! ALT_EVAL_VERBOSE=1 prints every pair.

use std::time::Instant;

use navpath_builder::build::components::{walk_components, Structure};
use navpath_builder::build::dijkstra::AltGraph;
use navpath_builder::build::landmarks::table_stats;
use navpath_core::engine::heuristics::LandmarkHeuristic;
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

/// The builder's table graph, rebuilt from a snapshot: walk CSR + macro edges
/// (lodestone-first floored at the quick-tele 2400 ms) + the full fairy clique.
fn structure_of(snap: &Snapshot) -> Structure {
    let n = snap.counts().nodes as usize;
    let (ms, md, mw, mk) = (snap.macro_src(), snap.macro_dst(), snap.macro_w(), snap.macro_kind_first());
    let mut es: Vec<u32> = ms.to_vec();
    let mut ed: Vec<u32> = md.to_vec();
    let mut ew: Vec<f32> = mw.iter().zip(mk).map(|(&w, &k)| if k == 2 { w.min(2400.0) } else { w }).collect();
    let (fnodes, fcost) = (snap.fairy_nodes(), snap.fairy_cost_ms());
    for &a in fnodes {
        for (j, &b) in fnodes.iter().enumerate() {
            if a != b {
                es.push(a);
                ed.push(b);
                ew.push(fcost[j]);
            }
        }
    }
    let g = AltGraph::new(n, snap.walk_offsets(), snap.walk_dst(), snap.walk_diag(), &es, &ed, &ew);
    let (comp, cc) = walk_components(snap.walk_offsets(), snap.walk_dst());
    Structure::new(comp, cc, g.fwd.pairs())
}

/// 0 = main SCC, 1 = main WCC outside the main SCC (one-way pockets), 2 = other WCCs.
fn class_of(st: &Structure, v: usize) -> usize {
    if st.scc_of(v) == 0 {
        0
    } else if st.wcc_of(v) == st.scc_wcc[0] {
        1
    } else {
        2
    }
}
const CLASS: [&str; 3] = ["main-scc", "pocket", "island"];

/// Finer goal classes for the bench tables: main SCC, the largest pocket SCC (the
/// 68,815-node one-way sink region on the current map), other pockets >= 4096 nodes
/// (the SCC strategy's dedicated ones), smaller pockets, islands.
fn bench_class(st: &Structure, v: usize) -> usize {
    match class_of(st, v) {
        0 => 0,
        2 => 4,
        _ => {
            let s = st.scc_of(v) as usize;
            let largest_pocket = (1..st.scc_size.len()).find(|&r| st.scc_wcc[r] == st.scc_wcc[0]);
            if Some(s) == largest_pocket {
                1
            } else if st.scc_size[s] >= 4096 {
                2
            } else {
                3
            }
        }
    }
}
const BCLASS: [&str; 5] = ["main", "pocket#1", "pocket>=4k", "pocket<4k", "island"];

fn lm_heuristic(snap: &Snapshot) -> LandmarkHeuristic<'_> {
    LandmarkHeuristic::from_snapshot(snap)
}

/// The snapshot's ALT table as plain u16 rows (decoded exactly when it is packed).
fn plain_table(snap: &Snapshot) -> std::borrow::Cow<'_, [u16]> {
    match snap.lm_packed() {
        None => std::borrow::Cow::Borrowed(snap.lm_tab()),
        Some(section) => {
            let n = snap.counts().nodes as usize;
            let l = snap.counts().landmarks as usize;
            let p = navpath_core::snapshot::alt_pack::PackedAlt::from_section(section, n, l);
            let mut out = vec![0u16; n * 2 * l];
            for (u, row) in out.chunks_mut(2 * l).enumerate() {
                p.decode_row_exact(u, row);
            }
            std::borrow::Cow::Owned(out)
        }
    }
}

fn stats(paths: &[String]) {
    let first = Snapshot::open(&paths[0]).expect("open snapshot");
    let st = structure_of(&first);
    let globals = parse_globals(&first);
    let mut gclass = [0usize; 3];
    for &(d, _) in &globals {
        gclass[class_of(&st, d as usize)] += 1;
    }
    println!(
        "structure: nodes={} sccs={} wccs={} main_scc={} main_wcc={} | globals={} (main-scc {}, pocket {}, island {})",
        st.nodes(),
        st.scc_size.len(),
        st.wcc_size.len(),
        st.scc_size[0],
        st.wcc_size[st.scc_wcc[0] as usize],
        globals.len(),
        gclass[0],
        gclass[1],
        gclass[2]
    );
    let pockets: Vec<usize> = (1..st.scc_size.len())
        .filter(|&s| st.scc_wcc[s] == st.scc_wcc[0] && st.scc_size[s] >= 256)
        .map(|s| st.scc_size[s])
        .collect();
    let small_pocket_nodes: usize = (1..st.scc_size.len())
        .filter(|&s| st.scc_wcc[s] == st.scc_wcc[0] && st.scc_size[s] < 4096)
        .map(|s| st.scc_size[s])
        .sum();
    println!("pocket SCCs >= 256 nodes: {pockets:?}; nodes in pocket SCCs < 4096: {small_pocket_nodes}");
    let islands: Vec<usize> = (0..st.wcc_size.len())
        .filter(|&w| w as u32 != st.scc_wcc[0])
        .map(|w| st.wcc_size[w])
        .take(12)
        .collect();
    println!("largest island WCCs: {islands:?}");

    // An origin in the main SCC: its lowest-id node.
    let origin = (0..st.nodes()).find(|&v| st.scc_of(v) == 0).unwrap() as u32;
    // Backward-anchor survival (select_active_rev keeps a column's a-side only if every
    // anchor has fa < SATURATED, its b-side only if every anchor has
    // ba != UNREACHABLE) for three anchor sets: origin + all globals; + globals outside
    // one-way pockets; + main-SCC globals only.
    let anchor_sets: [Vec<(u32, f32)>; 3] = [
        globals.clone(),
        globals.iter().copied().filter(|&(d, _)| class_of(&st, d as usize) != 1).collect(),
        globals.iter().copied().filter(|&(d, _)| class_of(&st, d as usize) == 0).collect(),
    ];
    println!(
        "{:<24} {:>4} {:>11} {:>9} {:>10} {:>10} {:>9}   bidir a/b cols: {:>8} {:>9} {:>9}",
        "snapshot", "L", "usable_main", "h0_nodes", "h0_pocket", "h0_island", "mean_use", "all-glob", "no-pocket", "main-only"
    );
    for p in paths {
        let snap = Snapshot::open(p).expect("open snapshot");
        let l = snap.counts().landmarks as usize;
        let s = table_stats(&st, &plain_table(&snap), l);
        let lm = lm_heuristic(&snap);
        let surv: Vec<String> = anchor_sets
            .iter()
            .map(|set| {
                let mut anchors = vec![(origin, 0.0f32)];
                anchors.extend(set.iter().copied());
                let rev = lm.select_active_rev(&anchors, origin, usize::MAX);
                let a_ok = rev.c1.iter().filter(|c| !c.is_nan()).count();
                let b_ok = rev.c2.iter().filter(|c| !c.is_nan()).count();
                format!("{a_ok}/{b_ok}")
            })
            .collect();
        let name = std::path::Path::new(p).file_name().unwrap().to_string_lossy().to_string();
        println!(
            "{:<24} {:>4} {:>11} {:>9} {:>10} {:>10} {:>9.1}   {:>23} {:>9} {:>9}",
            name, l, s.usable_main, s.h0_nodes, s.h0_main_wcc, s.h0_other_wcc, s.mean_usable, surv[0], surv[1], surv[2]
        );
        let mut lclass = [0usize; 3];
        for &lmk in snap.landmarks() {
            lclass[class_of(&st, lmk as usize)] += 1;
        }
        println!("    landmark ids in main-scc/pocket/island: {lclass:?}");
    }
}

fn pct(v: &mut [u64], p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn bench(n_pairs: usize, paths: &[String]) {
    let snaps: Vec<Snapshot> = paths.iter().map(|p| Snapshot::open(p).expect("open snapshot")).collect();
    for s in &snaps {
        s.populate();
    }
    let nodes = snaps[0].counts().nodes as usize;
    for s in &snaps {
        assert_eq!(s.counts().nodes as usize, nodes, "snapshots must share the graph");
    }
    let st = structure_of(&snaps[0]);
    // ALT_EVAL_GLOBALS=all (default) | nopocket | main: which global teleports are
    // eligible (a profile's eligible set is a subset of all of them).
    let gmode = std::env::var("ALT_EVAL_GLOBALS").unwrap_or_else(|_| "all".into());
    let globals: Vec<(u32, f32)> = parse_globals(&snaps[0])
        .into_iter()
        .filter(|&(d, _)| match gmode.as_str() {
            "nopocket" => class_of(&st, d as usize) != 1,
            "main" => class_of(&st, d as usize) == 0,
            _ => true,
        })
        .collect();
    println!("globals: {} ({gmode})", globals.len());
    let mut sources: Vec<u32> = snaps[0].fairy_nodes().to_vec();
    sources.sort_unstable();
    let mut dests: Vec<(u32, f32)> =
        snaps[0].fairy_nodes().iter().zip(snaps[0].fairy_cost_ms().iter()).map(|(&n, &c)| (n, c)).collect();
    dests.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let views: Vec<EngineView> = snaps
        .iter()
        .map(|s| {
            let mut v = EngineView::from_snapshot(s);
            v.extra.global = globals.clone().into();
            v.extra.fairy_sources = sources.clone().into();
            v.extra.fairy_dests = dests.clone().into();
            v
        })
        .collect();
    let macros_rev = NeighborProvider::new(nodes, snaps[0].macro_dst(), snaps[0].macro_src(), snaps[0].macro_w());
    let bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: None };

    let mut ctx = SearchContext::new(nodes);
    let mut cf = SearchContext::new(nodes);
    let mut cb = SearchContext::new(nodes);

    let mut state: u64 = 0xDEADBEEFCAFEF00D;
    let mut next = |m: usize| -> u32 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((state >> 33) as usize % m) as u32
    };
    let mut pairs = Vec::with_capacity(n_pairs);
    while pairs.len() < n_pairs {
        let s = next(nodes);
        let g = next(nodes);
        if s != g {
            pairs.push((s, g));
        }
    }

    let k = snaps.len();
    let verbose = std::env::var("ALT_EVAL_VERBOSE").is_ok();
    let reps: usize = std::env::var("ALT_EVAL_REPS").ok().and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
    println!("timing: min of {reps} run(s) per (pair, snapshot, engine)");
    // results[pair][2 * snapshot + engine] = (found, cost, pops, micros); engine 0 = uni.
    let mut results: Vec<Vec<(bool, f32, u32, u64)>> = vec![vec![(false, 0.0, 0, 0); 2 * k]; pairs.len()];

    for pass in 0..2 {
        let timed = pass == 1;
        let t_pass = Instant::now();
        for (pi, &(s, g)) in pairs.iter().enumerate() {
            for r in 0..k {
                let si = (pi + r) % k;
                let view = &views[si];
                for e in 0..2 {
                    let params = || SearchParams {
                        start: s,
                        goal: g,
                        macro_filter: None,
                        seed: None,
                        max_pops: None,
                        cancel: None,
                        bucket_ms: 0.0,
                    };
                    // Timed pass: min over ALT_EVAL_REPS runs (pops are deterministic;
                    // the min filters interference from other load on the host).
                    let mut best_us = u64::MAX;
                    let mut res = None;
                    for _ in 0..if timed { reps } else { 1 } {
                        let t = Instant::now();
                        let r = if e == 0 {
                            view.astar(params(), &mut ctx)
                        } else {
                            view.astar_bidir(&bp, params(), &mut cf, &mut cb)
                        };
                        best_us = best_us.min(t.elapsed().as_micros() as u64);
                        res = Some(r);
                    }
                    let res = res.unwrap();
                    if timed {
                        results[pi][2 * si + e] = (res.found, res.cost, res.pops, best_us);
                    }
                }
            }
        }
        println!("pass {pass} ({}) done in {:?}", if timed { "timed" } else { "warm-up" }, t_pass.elapsed());
    }

    // Cost cross-check: every snapshot and engine must agree (optimality). Within one
    // engine the f32 cost must also be bit-identical; across engines 1e-4 relative
    // (bidir sums g_f + g_b, a different rounding than uni's chained sum).
    let mut mismatch_exact = 0usize;
    let mut mismatch_tol = 0usize;
    let mut found_ct = 0usize;
    for (pi, &(s, g)) in pairs.iter().enumerate() {
        let row = &results[pi];
        let (f0, c0, _, _) = row[0];
        if f0 {
            found_ct += 1;
        }
        if verbose {
            let pops: Vec<u32> = row.iter().map(|r| r.2).collect();
            println!(
                "pair {pi} {s}->{g} start={} goal={} found={f0} cost={c0} pops(uni,bidir per snapshot)={pops:?}",
                CLASS[class_of(&st, s as usize)],
                CLASS[class_of(&st, g as usize)]
            );
        }
        for (i, &(f, c, _, _)) in row.iter().enumerate() {
            let (fr, cr, _, _) = row[i % 2];
            if !(f == fr && (!f || c.to_bits() == cr.to_bits())) {
                mismatch_exact += 1;
            }
            if !(f == f0 && (!f || (c - c0).abs() <= 1e-4 * c0.abs().max(1.0))) {
                mismatch_tol += 1;
                println!(
                    "COST MISMATCH pair {pi} {s}->{g}: {} {} found={f} cost={c} vs {} uni found={f0} cost={c0}",
                    paths[i / 2],
                    if i % 2 == 0 { "uni" } else { "bidir" },
                    paths[0]
                );
            }
        }
    }
    println!(
        "\n{} pairs ({} found) x {} snapshots x 2 engines; cost mismatches: {} beyond 1e-4 rel, {} not bit-identical to the same engine on the first snapshot",
        pairs.len(),
        found_ct,
        k,
        mismatch_tol,
        mismatch_exact
    );
    let mut ccount = [0u64; 5];
    for (pi, &(_, g)) in pairs.iter().enumerate() {
        if results[pi][0].0 {
            ccount[bench_class(&st, g as usize)] += 1;
        }
    }
    println!("found pairs by goal class (tables below: found pairs only):");
    for c in 0..5 {
        println!("  {:<11} {}", BCLASS[c], ccount[c]);
    }
    println!(
        "\n{:<24} {:<6} {:>11} {:>9} {:>8} {:>8} {:>9} {:>9}   pops by goal class: {:>9} {:>9} {:>10} {:>9} {:>8}",
        "snapshot", "engine", "total_pops", "total_ms", "p50_us", "p95_us", "p50_pops", "p95_pops",
        BCLASS[0], BCLASS[1], BCLASS[2], BCLASS[3], BCLASS[4]
    );
    let mut ms_rows = Vec::new();
    for si in 0..k {
        for e in 0..2 {
            let mut pops_c = [0u64; 5];
            let mut us_c = [0u64; 5];
            let mut us_v = Vec::new();
            let mut pops_v = Vec::new();
            for (pi, &(_, g)) in pairs.iter().enumerate() {
                if !results[pi][0].0 {
                    continue;
                }
                let (_, _, pops, us) = results[pi][2 * si + e];
                let c = bench_class(&st, g as usize);
                pops_c[c] += pops as u64;
                us_c[c] += us;
                us_v.push(us);
                pops_v.push(pops as u64);
            }
            let name = std::path::Path::new(&paths[si]).file_name().unwrap().to_string_lossy().to_string();
            let eng = if e == 0 { "uni" } else { "bidir" };
            println!(
                "{:<24} {:<6} {:>11} {:>9.1} {:>8} {:>8} {:>9} {:>9}                       {:>9} {:>9} {:>10} {:>9} {:>8}",
                name,
                eng,
                pops_c.iter().sum::<u64>(),
                us_c.iter().sum::<u64>() as f64 / 1000.0,
                pct(&mut us_v, 0.5),
                pct(&mut us_v, 0.95),
                pct(&mut pops_v, 0.5),
                pct(&mut pops_v, 0.95),
                pops_c[0],
                pops_c[1],
                pops_c[2],
                pops_c[3],
                pops_c[4]
            );
            ms_rows.push(format!(
                "{:<24} {:<6} {:>9.1} {:>9.1} {:>10.1} {:>9.1} {:>8.1}",
                name,
                eng,
                us_c[0] as f64 / 1000.0,
                us_c[1] as f64 / 1000.0,
                us_c[2] as f64 / 1000.0,
                us_c[3] as f64 / 1000.0,
                us_c[4] as f64 / 1000.0
            ));
        }
    }
    println!(
        "\nwall ms by goal class:\n{:<24} {:<6} {:>9} {:>9} {:>10} {:>9} {:>8}",
        "snapshot", "engine", BCLASS[0], BCLASS[1], BCLASS[2], BCLASS[3], BCLASS[4]
    );
    for r in ms_rows {
        println!("{r}");
    }
    // Unreachable pairs, for completeness (never searched in production).
    let unreach: Vec<usize> = (0..pairs.len()).filter(|&pi| !results[pi][0].0).collect();
    if !unreach.is_empty() {
        let per: Vec<String> = (0..2 * k)
            .map(|i| format!("{}", unreach.iter().map(|&pi| results[pi][i].2 as u64).sum::<u64>()))
            .collect();
        println!(
            "unreachable pairs: {} — total pops per (snapshot, engine) in order uni,bidir: {}",
            unreach.len(),
            per.join(" ")
        );
    }
    if mismatch_tol > 0 {
        std::process::exit(1);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("stats") if args.len() >= 2 => stats(&args[1..]),
        Some("bench") if args.len() >= 3 => bench(args[1].parse().expect("N pairs"), &args[2..]),
        Some("nodes") if args.len() >= 3 => {
            // alt_eval nodes SNAP id... : coordinates, SCC rank/size and WCC of nodes.
            let snap = Snapshot::open(&args[1]).expect("open snapshot");
            let st = structure_of(&snap);
            for a in &args[2..] {
                let v: usize = a.parse().expect("node id");
                let s = st.scc_of(v) as usize;
                println!(
                    "node {v} at {:?}: scc rank {s} (size {}), wcc {} (size {}), class {}",
                    snap.node_coord(v as u32),
                    st.scc_size[s],
                    st.wcc_of(v),
                    st.wcc_size[st.wcc_of(v) as usize],
                    CLASS[class_of(&st, v)]
                );
            }
        }
        _ => {
            eprintln!("usage: alt_eval stats SNAP... | alt_eval bench N SNAP...");
            std::process::exit(2);
        }
    }
}
