//! N-way golden replay comparator — the correctness gate for every future engine or
//! format optimization (docs/optimization_roadmap_v2.md §9.1).
//!
//! For every corpus entry it runs the PRODUCTION search paths (through
//! `engine_adapter`, with real per-request MacroFilters and fairy eligibility):
//! unidirectional, bidirectional, and — when the start coordinate is absent from the
//! snapshot — the multi-source virtual-start path; each under seeds [None, 1, 12345].
//! Assertions per run:
//!   1. found flag matches the golden expectation (all engines, all seeds);
//!   2. the reported cost matches an INDEPENDENT re-costing of the returned path
//!      (walk/macro/global/fairy base weights + exact edge jitter, f32-accumulated in
//!      path order) — catches engines whose reported cost and path disagree;
//!   3. unseeded cost equals the golden cost (bit-exact on the blessed snapshot;
//!      1e-4 relative on other snapshots, e.g. the invariance sweep's 24-landmark
//!      build, where equal-cost tie paths may differ in final-ulp summation);
//!   4. seeded cost lies in [golden, golden + 0.1ms x unseeded_path_edges] — the
//!      provable jitter envelope;
//!   5. uni and bidir agree per seed (1e-3 relative);
//!   6. admissibility oracle: h_active(start) <= cost (heuristic overstatement is the
//!      historically worst bug class here);
//!   7. pops <= pops_max x --pops-slack (plateau/regression alarm: the v8 138->2.3k
//!      short-route pop regression would have tripped this).
//!
//! Usage:
//!   cargo run --release -p navpath-service --example replay              # check
//!   cargo run --release -p navpath-service --example replay -- --regen  # bless
//!   ... [--pops-slack=8] [path/to/golden_corpus.json]
//!
//! Snapshot from SNAPSHOT_PATH or NAVPATH_BENCH_SNAPSHOT (default ../../graph.snapshot).
//! Exits 0 with a notice when no snapshot exists (so CI without the data skips).

use std::collections::HashMap;
use std::sync::Arc;

use navpath_core::eligibility::EligibilityMask;
use navpath_core::engine::heuristics::{active_landmarks, LandmarkHeuristic};
use navpath_core::engine::search::{edge_jitter, SearchContext};
use navpath_core::{NeighborProvider, SearchResult, Snapshot};
use navpath_service::engine_adapter::{
    build_canonical_grid, build_component_graph, build_fairy_rings, build_neighbor_provider,
    build_profile_artifacts, goal_reachable, run_route_with_requirements_and_fairy_rings,
    run_route_with_requirements_virtual_start, FairyRing, GlobalTeleport,
};
use serde::{Deserialize, Serialize};

const SEEDS: [Option<u64>; 3] = [None, Some(1), Some(12345)];

#[derive(Serialize, Deserialize)]
struct Corpus {
    /// blake3 tail hash of the snapshot the expectations were blessed on. Bit-exact
    /// cost / entry-id / pops assertions apply only when the loaded snapshot matches;
    /// other snapshots (invariance sweeps) get tolerance-based cost checks.
    #[serde(default)]
    snapshot_hash: Option<String>,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    name: String,
    /// [x, y, plane]. A coordinate absent from the snapshot makes this a
    /// virtual-start entry, exactly as the service treats it.
    start: [i32; 3],
    goal: [i32; 3],
    /// "all" = every requirement tag satisfied; "none" = none satisfied (edges with
    /// empty requirement lists remain eligible — fail-open, matching production).
    profile: String,
    #[serde(default)]
    quick_tele: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expect: Option<Expect>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Expect {
    found: bool,
    /// Absent for found=false entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cost_unseeded: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entry: Option<u32>,
    pops_max: u32,
}

struct Ctx {
    snap: Arc<Snapshot>,
    provider: Arc<NeighborProvider>,
    provider_rev: Arc<NeighborProvider>,
    globals: Arc<Vec<GlobalTeleport>>,
    fairy_rings: Arc<Vec<FairyRing>>,
}

/// Per-(profile, quick_tele) eligible-edge maps for the independent path re-coster.
struct Recoster {
    /// dst -> min eligible global teleport cost (quick-tele effective).
    global_cost: HashMap<u32, f32>,
    /// (src, dst) -> min eligible macro effective weight.
    macro_w: HashMap<(u32, u32), f32>,
    /// eligible fairy source nodes.
    fairy_sources: std::collections::HashSet<u32>,
    /// eligible fairy dst -> hop cost.
    fairy_cost: HashMap<u32, f32>,
}

impl Recoster {
    fn build(ctx: &Ctx, mask: &EligibilityMask, quick_tele: bool) -> Self {
        let mut global_cost: HashMap<u32, f32> = HashMap::new();
        for g in ctx.globals.iter() {
            if g.reqs.iter().any(|&idx| !mask.is_satisfied(idx)) {
                continue;
            }
            let cost = if quick_tele && g.kind_first == 2 { 2400.0 } else { g.cost };
            let e = global_cost.entry(g.dst).or_insert(f32::INFINITY);
            if cost < *e {
                *e = cost;
            }
        }
        // Walk the forward macro adjacency slot by slot (slot order matches the
        // provider the searches use, but this map is keyed on (src, dst) so it is
        // independent of slot bookkeeping).
        let mut macro_w: HashMap<(u32, u32), f32> = HashMap::new();
        let adj = &ctx.provider.macro_edges;
        for u in 0..adj.nodes as u32 {
            let (s, e) = (adj.offsets[u as usize], adj.offsets[u as usize + 1]);
            for slot in s..e {
                let data = &ctx.provider.macro_data[slot];
                if data.reqs.iter().any(|&idx| !mask.is_satisfied(idx)) {
                    continue;
                }
                let w = if quick_tele && data.kind_first == 2 { 2400.0 } else { adj.w[slot] };
                let key = (u, adj.dst[slot]);
                let entry = macro_w.entry(key).or_insert(f32::INFINITY);
                if w < *entry {
                    *entry = w;
                }
            }
        }
        let mut fairy_sources = std::collections::HashSet::new();
        let mut fairy_cost = HashMap::new();
        for ring in ctx.fairy_rings.iter() {
            if ring.req_tag_idxs.iter().any(|&idx| !mask.is_satisfied(idx)) {
                continue;
            }
            fairy_sources.insert(ring.node);
            fairy_cost.insert(ring.node, ring.cost_ms);
        }
        Recoster { global_cost, macro_w, fairy_sources, fairy_cost }
    }

    /// Re-sum the path's cost exactly as the engine accumulates it: per edge, the
    /// cheapest eligible base weight plus the deterministic jitter, f32-added in path
    /// order. Virtual paths start at the entry's (unjittered) global seed cost.
    fn recost(&self, snap: &Snapshot, path: &[u32], seed: Option<u64>, virtual_start: bool) -> Result<f32, String> {
        if path.is_empty() {
            return Err("empty path".into());
        }
        let mut g: f32 = 0.0;
        if virtual_start {
            g = *self
                .global_cost
                .get(&path[0])
                .ok_or_else(|| format!("virtual entry {} is not an eligible global dst", path[0]))?;
        }
        for (i, w) in path.windows(2).enumerate() {
            let (u, v) = (w[0], w[1]);
            let mut base = f32::INFINITY;
            if let Some(ww) = snap.walk_edge_weight(u, v) {
                base = base.min(ww);
            }
            if let Some(&ww) = self.macro_w.get(&(u, v)) {
                base = base.min(ww);
            }
            // Globals relax only from the on-graph origin (path[0] when not virtual).
            if i == 0 && !virtual_start {
                if let Some(&c) = self.global_cost.get(&v) {
                    base = base.min(c);
                }
            }
            if u != v && self.fairy_sources.contains(&u) {
                if let Some(&c) = self.fairy_cost.get(&v) {
                    base = base.min(c);
                }
            }
            if !base.is_finite() {
                return Err(format!("no eligible edge {u}->{v} (step {i})"));
            }
            let wj = match seed {
                Some(s) => base + edge_jitter(s, u, v),
                None => base,
            };
            g += wj;
        }
        Ok(g)
    }
}

fn profile_mask(snap: &Snapshot, profile: &str) -> EligibilityMask {
    let ntags = snap.req_tags().len() / 4;
    let satisfied = match profile {
        "all" => vec![true; ntags],
        "none" => vec![false; ntags],
        other => {
            eprintln!("unknown profile {other:?} (expected \"all\" or \"none\")");
            std::process::exit(2);
        }
    };
    EligibilityMask { satisfied }
}

struct RunOut {
    label: &'static str,
    seed: Option<u64>,
    res: SearchResult,
    entry: Option<u32>,
}

fn main() {
    // The comparator wants full, budget-free searches: pops then measure true effort,
    // truncation statuses never appear, and the retry path stays quiescent. The
    // weak-backward demotion is also disabled so the "bidir" runs really are
    // bidirectional — the harness measures pure engines, not the routing policy.
    std::env::set_var("NAVPATH_MAX_POPS", "0");
    std::env::set_var("NAVPATH_BIDIR_MIN_HB_RATIO", "0");

    let mut regen = false;
    let mut pops_slack: f64 = 1.0;
    let mut bucket_ms: f32 = 0.0;
    let mut corpus_path: Option<String> = None;
    for arg in std::env::args().skip(1) {
        if arg == "--regen" {
            regen = true;
        } else if let Some(v) = arg.strip_prefix("--pops-slack=") {
            pops_slack = v.parse().expect("--pops-slack=<float>");
        } else if let Some(v) = arg.strip_prefix("--bucket-ms=") {
            // Rollout gate for the plateau tie-break (roadmap 3.4): runs the whole
            // corpus with bucketing forced on for BOTH seeded and unseeded searches
            // and relaxes the cost assertions to the provable +bucket envelope.
            bucket_ms = v.parse().expect("--bucket-ms=<float>");
        } else {
            corpus_path = Some(arg);
        }
    }
    if bucket_ms > 0.0 {
        std::env::set_var("NAVPATH_TIEBREAK_BUCKET_MS", bucket_ms.to_string());
        std::env::set_var("NAVPATH_TIEBREAK_UNSEEDED", "1");
    }
    let corpus_path = corpus_path
        .unwrap_or_else(|| format!("{}/../../tools/golden_corpus.json", env!("CARGO_MANIFEST_DIR")));

    let snap_path = std::env::var("SNAPSHOT_PATH")
        .or_else(|_| std::env::var("NAVPATH_BENCH_SNAPSHOT"))
        .unwrap_or_else(|_| format!("{}/../../graph.snapshot", env!("CARGO_MANIFEST_DIR")));
    if !std::path::Path::new(&snap_path).exists() {
        println!("replay: no snapshot at {snap_path}; skipping (exit 0)");
        return;
    }

    let snap = Snapshot::open(&snap_path).expect("open snapshot");
    let snap_hash = navpath_service::read_tail_hash_hex(&std::path::PathBuf::from(&snap_path));
    let (provider, provider_rev, globals, _lookup) = build_neighbor_provider(&snap);
    let (fairy_rings, _node_to_ring) = build_fairy_rings(&snap);
    let ctx = Ctx {
        snap: Arc::new(snap),
        provider: Arc::new(provider),
        provider_rev: Arc::new(provider_rev),
        globals: Arc::new(globals),
        fairy_rings: Arc::new(fairy_rings),
    };
    let lm = LandmarkHeuristic {
        nodes: ctx.snap.counts().nodes as usize,
        landmarks: ctx.snap.counts().landmarks as usize,
        tab: ctx.snap.lm_tab(),
        quantum: ctx.snap.manifest().alt_quantum_ms,
    };

    let raw = std::fs::read_to_string(&corpus_path)
        .unwrap_or_else(|e| panic!("cannot read corpus {corpus_path}: {e}"));
    let mut corpus: Corpus = serde_json::from_str(&raw).expect("parse corpus json");
    let blessed_snapshot = corpus.snapshot_hash.is_some() && corpus.snapshot_hash == snap_hash;
    println!(
        "replay: snapshot {} ({} nodes, {} landmarks), {} entries, blessed_snapshot={}",
        snap_path,
        ctx.snap.counts().nodes,
        ctx.snap.counts().landmarks,
        corpus.entries.len(),
        blessed_snapshot
    );

    let mut failures: Vec<String> = Vec::new();
    // One reusable context pair for every search, mirroring the service's pool.
    let mut ctxs = (SearchContext::new(0), SearchContext::new(0));
    // The component precheck is an EXACT reachability decision, and replay searches are
    // budget-free — so its verdict must equal the engine's found flag on every entry.
    let comp_graph = build_component_graph(&ctx.snap, &ctx.globals, &ctx.fairy_rings);
    // Production default: canonical pruning on (NAVPATH_CANONICAL=0 for A/B runs).
    let canonical = build_canonical_grid(&ctx.snap);
    let t0 = std::time::Instant::now();

    for entry in corpus.entries.iter_mut() {
        let name = entry.name.clone();
        let mut fail = |msg: String| failures.push(format!("[{name}] {msg}"));

        let Some(gid) = ctx.snap.find_node(entry.goal[0], entry.goal[1], entry.goal[2]) else {
            fail(format!("goal {:?} not in snapshot — fix the corpus entry", entry.goal));
            continue;
        };
        let sid = ctx.snap.find_node(entry.start[0], entry.start[1], entry.start[2]);
        let virtual_start = sid.is_none();
        let mask = profile_mask(&ctx.snap, &entry.profile);

        let recoster = Recoster::build(&ctx, &mask, entry.quick_tele);

        // Per-profile artifacts, built exactly as the service builds/caches them
        // (roadmap 5.4). One set serves both engines: the "uni" runs pass no reversed
        // provider, so the pre-built reversed filter is simply unused there.
        let artifacts = build_profile_artifacts(
            &ctx.provider,
            Some(&ctx.provider_rev),
            &ctx.globals,
            &ctx.fairy_rings,
            &mask,
            entry.quick_tele,
        );

        // --- run every engine x seed ---
        let mut runs: Vec<RunOut> = Vec::new();
        for seed in SEEDS {
            if let Some(sid) = sid {
                for (label, rev) in [
                    ("uni", None),
                    ("bidir", Some(ctx.provider_rev.clone())),
                ] {
                    let out = run_route_with_requirements_and_fairy_rings(
                        ctx.snap.clone(),
                        ctx.provider.clone(),
                        rev,
                        sid,
                        gid,
                        &mask,
                        seed,
                        None,
                        &artifacts,
                        canonical.clone(),
                        &mut ctxs,
                    );
                    runs.push(RunOut { label, seed, res: out.res, entry: None });
                }
            } else {
                for (label, rev) in [
                    ("multi", None),
                    ("multi_bidir", Some(ctx.provider_rev.clone())),
                ] {
                    let (out, ventry) = run_route_with_requirements_virtual_start(
                        ctx.snap.clone(),
                        ctx.provider.clone(),
                        rev,
                        gid,
                        seed,
                        None,
                        &artifacts,
                        canonical.clone(),
                        &mut ctxs,
                    );
                    runs.push(RunOut { label, seed, res: out.res, entry: ventry });
                }
            }
        }

        let primary_unseeded = runs
            .iter()
            .find(|r| r.seed.is_none() && (r.label == "uni" || r.label == "multi"))
            .expect("unseeded primary run");
        let found = primary_unseeded.res.found;
        let cost_unseeded = primary_unseeded.res.cost;
        {
            let comps = ctx.snap.comp_ids();
            let start_comp = sid.map(|s| comps[s as usize]);
            let reachable = goal_reachable(&comp_graph, &mask, start_comp, comps[gid as usize]);
            if reachable != found {
                fail(format!(
                    "component precheck verdict ({reachable}) != engine found ({found}) — \
                     the condensed reachability graph or the engine is wrong"
                ));
            }
        }
        let unseeded_edges = primary_unseeded.res.path.len().saturating_sub(1) as f32;
        let max_pops_observed = runs.iter().map(|r| r.res.pops).max().unwrap_or(0);

        if regen {
            entry.expect = Some(Expect {
                found,
                cost_unseeded: if found { Some(cost_unseeded as f64) } else { None },
                entry: if virtual_start { primary_unseeded.entry } else { None },
                pops_max: ((max_pops_observed as u64 * 3 / 2 + 999) / 1000 * 1000).max(5000) as u32,
            });
        }

        let Some(expect) = entry.expect.clone() else {
            fail("no golden expectation — run with --regen first".into());
            continue;
        };
        let golden = expect.cost_unseeded.unwrap_or(f64::INFINITY) as f32;

        // Admissibility oracle. h_active bounds distances over walk/macro/fairy edges
        // only — global teleports are origin seeds the ALT tables deliberately do NOT
        // cover — so any found route satisfies
        //   cost >= min( h(start), min over eligible globals (w_g + h(dst_g)) ):
        // it either leaves the start over covered edges (h(start) applies) or takes a
        // global first (w_g plus a covered remainder from dst_g).
        let h0 = sid.map(|sid| {
            let active = lm.select_active(sid, gid, active_landmarks());
            let mut bound = lm.h_active(sid, &active);
            for (&dst, &w) in &recoster.global_cost {
                bound = bound.min(w + lm.h_active(dst, &active));
            }
            bound
        });

        for run in &runs {
            let tag = format!("{}/seed={:?}", run.label, run.seed);
            if run.res.found != expect.found {
                fail(format!("{tag}: found={} but golden found={}", run.res.found, expect.found));
                continue;
            }
            let slacked = (expect.pops_max as f64 * pops_slack) as u32;
            if run.res.pops > slacked {
                fail(format!("{tag}: pops {} > pops_max {} (slack {pops_slack})", run.res.pops, slacked));
            }
            if !run.res.found {
                continue;
            }
            let cost = run.res.cost;
            // (2) reported cost must equal the path's independent re-cost.
            match recoster.recost(&ctx.snap, &run.res.path, run.seed, virtual_start) {
                Ok(rc) => {
                    let tol = if run.label.contains("bidir") { 1e-3 * cost.max(1.0) } else { 0.01 + 1e-5 * cost };
                    if (rc - cost).abs() > tol {
                        fail(format!("{tag}: reported cost {cost} != path recost {rc}"));
                    }
                }
                Err(e) => fail(format!("{tag}: path does not re-cost: {e}")),
            }
            // (6) admissibility oracle.
            if let Some(h0) = h0 {
                if h0 > cost + 1e-3 {
                    fail(format!("{tag}: ADMISSIBILITY h_active(start)={h0} > cost={cost}"));
                }
            }
            match run.seed {
                // (3) unseeded golden cost. Bit-exact applies only to the engine that
                // DEFINES the golden (uni / multi) on the blessed snapshot; bidir may
                // return an equal-cost tie path whose f32 sum differs in final ulps,
                // and is pinned by the tolerance check + cross-engine parity instead.
                None => {
                    let primary = !run.label.contains("bidir");
                    if bucket_ms > 0.0 {
                        // Bucketed rollout gate: within the provable +bucket envelope.
                        // The lower edge gets the same relative slack as exact mode —
                        // bidir tie paths legitimately sum a few ulps below golden.
                        let lo = golden - 1e-4 * golden.max(1.0);
                        if cost < lo || cost > golden + bucket_ms + 1e-3 {
                            fail(format!(
                                "{tag}: bucketed unseeded cost {cost} outside [{golden}, {golden} + {bucket_ms}]"
                            ));
                        }
                    } else {
                        let exact = cost == golden;
                        let close = (cost - golden).abs() <= 1e-4 * golden.max(1.0);
                        if (blessed_snapshot && primary && !exact) || !close {
                            fail(format!(
                                "{tag}: unseeded cost {cost} != golden {golden} (blessed={blessed_snapshot})"
                            ));
                        }
                    }
                }
                // (4) seeded jitter envelope.
                Some(_) => {
                    let hi = golden + 0.1 * unseeded_edges + bucket_ms + 1e-3;
                    if cost < golden - 1e-3 || cost > hi {
                        fail(format!("{tag}: seeded cost {cost} outside [{golden}, {hi}]"));
                    }
                }
            }
            // (unseeded, blessed) virtual entry pin — primary engine only (the bidir
            // variant may legitimately pick a different equal-cost entry).
            if virtual_start && run.seed.is_none() && blessed_snapshot && run.label == "multi" && bucket_ms == 0.0 {
                if run.entry != expect.entry {
                    fail(format!("{tag}: virtual entry {:?} != golden {:?}", run.entry, expect.entry));
                }
            }
        }

        // (5) per-seed cross-engine parity (on-graph entries run two engines).
        for seed in SEEDS {
            let costs: Vec<(&str, f32, bool)> = runs
                .iter()
                .filter(|r| r.seed == seed)
                .map(|r| (r.label, r.res.cost, r.res.found))
                .collect();
            if costs.len() == 2 && costs[0].2 && costs[1].2 {
                let (a, b) = (costs[0].1, costs[1].1);
                if (a - b).abs() > 1e-3 * a.max(1.0) + bucket_ms {
                    fail(format!(
                        "seed={seed:?}: engine disagreement {}={a} vs {}={b}",
                        costs[0].0, costs[1].0
                    ));
                }
            }
        }

        println!(
            "  {name}: {} cost={:.3} pops(max)={} [{}{}]",
            if found { "found" } else { "not-found" },
            cost_unseeded,
            max_pops_observed,
            if virtual_start { "virtual, " } else { "" },
            entry.profile
        );
    }

    if regen {
        corpus.snapshot_hash = snap_hash;
        let json = serde_json::to_string_pretty(&corpus).expect("serialize corpus");
        std::fs::write(&corpus_path, json + "\n").expect("write corpus");
        println!("replay: regenerated golden expectations into {corpus_path}");
    }

    println!("replay: {} entries in {:?}, {} failure(s)", corpus.entries.len(), t0.elapsed(), failures.len());
    for f in &failures {
        eprintln!("FAIL {f}");
    }
    std::process::exit(if failures.is_empty() { 0 } else { 1 });
}
