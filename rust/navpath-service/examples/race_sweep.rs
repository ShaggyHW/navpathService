//! Per-pair uni-vs-bidir sweep for tuning the hedged race (T3.2): runs both engines
//! through the PRODUCTION adapter paths (`EngineChoice::Uni` — JPS when `NAVPATH_JPS=1`
//! and unseeded — and `EngineChoice::Bidir`) on LCG pairs, records the per-pair wall
//! times and the race hint (`engine_adapter::race_hint`), then replays hedge policies
//! over the measured times:
//!
//!   single engine | race both at once (the pre-T3.2 behaviour) | delayed hedge with
//!   primary P and delay D | the predictive gate in front of either
//!
//! reporting total/percentile latency and total CPU (both arms' run time) per policy.
//! The simulation charges a delayed hedge `D + timer_slack` before it starts (tokio's
//! timer has 1 ms granularity; `--slack-ms`, default 0.5) and a 0.03 ms task hop for a
//! second arm; it ignores memory contention between concurrent arms, which only makes
//! racing look better than it is.
//!
//!   cargo run --release -p navpath-service --example race_sweep -- [pairs=400] [--seeded]
//!       [--profile=all|none] [--csv=out.csv] [--slack-ms=0.5]
//!
//! Each engine is timed as the min of two runs in alternating order after an untimed
//! warm-up of both (T5.10: fixed order flatters the second engine).

use std::sync::Arc;

use navpath_core::engine::search::SearchContext;
use navpath_core::Snapshot;
use navpath_service::engine_adapter::{
    build_profile_artifacts, race_hint, run_route_with_requirements_and_fairy_rings,
    run_route_with_requirements_virtual_start, EngineChoice, ProfileArtifacts, RaceHint,
};
use navpath_service::SnapshotState;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct Pair {
    start: Option<u32>,
    goal: u32,
    cost: f32,
    t_uni: f64,
    t_bi: f64,
    hint: RaceHint,
    uni_engine: &'static str,
}

#[allow(clippy::too_many_arguments)]
fn run(
    st: &SnapshotState,
    arts: &ProfileArtifacts,
    mask: &navpath_core::eligibility::EligibilityMask,
    start: Option<u32>,
    goal: u32,
    seed: Option<u64>,
    engine: EngineChoice,
    ctxs: &mut (SearchContext, SearchContext),
) -> (navpath_core::SearchResult, &'static str, f64) {
    let snap = st.snapshot.clone().unwrap();
    let t = std::time::Instant::now();
    let out = match start {
        Some(s) => run_route_with_requirements_and_fairy_rings(
            snap, st.neighbors.clone().unwrap(), st.neighbors_rev.clone(), s, goal, mask, seed, None, arts,
            st.canonical_grid.clone(), engine, ctxs,
        ),
        None => run_route_with_requirements_virtual_start(
            snap, st.neighbors.clone().unwrap(), st.neighbors_rev.clone(), goal, seed, None, arts,
            st.canonical_grid.clone(), engine, ctxs,
        )
        .0,
    };
    let ms = t.elapsed().as_secs_f64() * 1e3;
    (out.res, out.engine, ms)
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i]
}

fn main() {
    let mut n_pairs = 400usize;
    let mut seeded = false;
    let mut profile = "all".to_string();
    let mut csv: Option<String> = None;
    let mut slack = 0.5f64;
    for a in std::env::args().skip(1) {
        if a == "--seeded" {
            seeded = true;
        } else if let Some(v) = a.strip_prefix("--profile=") {
            profile = v.to_string();
        } else if let Some(v) = a.strip_prefix("--csv=") {
            csv = Some(v.to_string());
        } else if let Some(v) = a.strip_prefix("--slack-ms=") {
            slack = v.parse().expect("--slack-ms=<float>");
        } else {
            n_pairs = a.parse().expect("pair count");
        }
    }
    let snap_path = std::env::var("SNAPSHOT_PATH")
        .or_else(|_| std::env::var("NAVPATH_BENCH_SNAPSHOT"))
        .unwrap_or_else(|_| format!("{}/../../graph.snapshot", env!("CARGO_MANIFEST_DIR")));
    let snap = Snapshot::open(&snap_path).expect("open snapshot");
    if !matches!(std::env::var("NAVPATH_HARNESS_COLD").ok().as_deref(), Some("1")) {
        let t = std::time::Instant::now();
        let bytes = snap.populate();
        eprintln!("race_sweep: populated {} MiB in {:?}", bytes >> 20, t.elapsed());
    }
    let st = SnapshotState::build(snap_path.clone().into(), snap, None);
    let snap: Arc<Snapshot> = st.snapshot.clone().unwrap();
    let nodes = snap.counts().nodes as usize;
    let ntags = snap.req_tags().len() / 4;
    let mask = navpath_core::eligibility::EligibilityMask { satisfied: vec![profile == "all"; ntags] };
    let arts = build_profile_artifacts(
        st.neighbors.as_ref().unwrap(), st.neighbors_rev.as_deref(), &st.globals, &st.fairy_rings, &mask, false,
    );
    let cg = st.comp_graph.clone().unwrap();
    let reach = arts.reach(&cg, &mask);
    let comps = snap.comp_ids();
    let mut ctxs = (SearchContext::new(nodes), SearchContext::new(nodes));

    let mut state: u64 = 0x5EED_0000_0000_0042;
    let mut next = |m: usize| -> u32 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((state >> 33) as usize % m) as u32
    };
    let mut pairs: Vec<Pair> = Vec::new();
    let mut tried = 0usize;
    let t0 = std::time::Instant::now();
    while pairs.len() < n_pairs {
        tried += 1;
        let g = next(nodes);
        // Every 10th pair is a virtual (off-graph) start, like a bot teleporting in.
        let start = if tried.is_multiple_of(10) { None } else { Some(next(nodes)) };
        if start == Some(g) || !reach.reachable(start.map(|s| comps[s as usize]), comps[g as usize]) {
            continue;
        }
        let seed = if seeded { Some(0x5EED ^ tried as u64) } else { None };
        // Untimed warm-up of both engines (page cache, h-cache lines), then two timed
        // runs each in alternating order.
        let (r0, _, _) = run(&st, &arts, &mask, start, g, seed, EngineChoice::Uni, &mut ctxs);
        if !r0.found || r0.status != navpath_core::SearchStatus::Found {
            continue;
        }
        let _ = run(&st, &arts, &mask, start, g, seed, EngineChoice::Bidir, &mut ctxs);
        let (ru, uni_engine, tu1) = run(&st, &arts, &mask, start, g, seed, EngineChoice::Uni, &mut ctxs);
        let (rb, _, tb1) = run(&st, &arts, &mask, start, g, seed, EngineChoice::Bidir, &mut ctxs);
        let (_, _, tb2) = run(&st, &arts, &mask, start, g, seed, EngineChoice::Bidir, &mut ctxs);
        let (_, _, tu2) = run(&st, &arts, &mask, start, g, seed, EngineChoice::Uni, &mut ctxs);
        if !rb.found || (ru.cost - rb.cost).abs() > 1e-3 * ru.cost.max(1.0) {
            eprintln!("engine disagreement {start:?}->{g}: uni {} bidir {}", ru.cost, rb.cost);
        }
        let hint = race_hint(&snap, &arts, start, g);
        pairs.push(Pair { start, goal: g, cost: ru.cost, t_uni: tu1.min(tu2), t_bi: tb1.min(tb2), hint, uni_engine });
    }
    eprintln!("race_sweep: {} pairs ({} tried) in {:?}, profile={profile}, seeded={seeded}, uni engine={}",
        pairs.len(), tried, t0.elapsed(), pairs.first().map_or("-", |p| p.uni_engine));

    if let Some(path) = csv {
        let mut s = String::from("virtual,cost_s,t_uni_ms,t_bidir_ms,h_start,h_teleport,teleport_dominated,blind,sx,sy,sp,gx,gy,gp\n");
        for p in &pairs {
            // Virtual starts get an off-graph start coordinate, as a client would send.
            let (sx, sy, sp) = p.start.map_or((30000, 20000, 0), |s| snap.node_coord(s));
            let (gx, gy, gp) = snap.node_coord(p.goal);
            s += &format!(
                "{},{:.1},{:.4},{:.4},{},{},{},{},{sx},{sy},{sp},{gx},{gy},{gp}\n",
                p.start.is_none(), p.cost / 1000.0, p.t_uni, p.t_bi, p.hint.h_start, p.hint.h_teleport,
                p.hint.teleport_dominated(), p.hint.blind
            );
        }
        std::fs::write(&path, s).expect("write csv");
    }

    // ---- policy simulation ----
    const HOP: f64 = 0.03; // extra spawn_blocking hop for a second arm
    #[derive(Clone, Copy)]
    enum Pol {
        Single(bool),                  // true = uni
        RaceNow,                       // both at once
        Hedge { uni_primary: bool, d: f64 },
    }
    let sim = |pol: Pol, p: &Pair| -> (f64, f64, bool) {
        // (latency, cpu, hedge started)
        match pol {
            Pol::Single(uni) => {
                let t = if uni { p.t_uni } else { p.t_bi };
                (t, t, false)
            }
            Pol::RaceNow => {
                // Both arms start together; the loser runs until the winner cancels it.
                let w = p.t_uni.min(p.t_bi);
                (w, 2.0 * w, true)
            }
            Pol::Hedge { uni_primary, d } => {
                let (tp, th) = if uni_primary { (p.t_uni, p.t_bi) } else { (p.t_bi, p.t_uni) };
                let start = d + slack + HOP;
                if tp <= start {
                    (tp, tp, false)
                } else {
                    let l = tp.min(start + th);
                    (l, l + (l - start), true)
                }
            }
        }
    };
    // The service's default primary (`NAVPATH_RACE_PRIMARY=auto`): JPS when it applies.
    let auto_uni = !seeded && pairs.first().is_some_and(|p| p.uni_engine == "jps");
    let mut policies: Vec<(String, Pol, bool)> = vec![
        ("uni only".into(), Pol::Single(true), false),
        ("bidir only".into(), Pol::Single(false), false),
        ("race now (old)".into(), Pol::RaceNow, false),
        (format!("gate({}) + race now", if auto_uni { "uni" } else { "bidir" }), Pol::RaceNow, true),
    ];
    for &uni_primary in &[false, true] {
        for &d in &[1.0, 2.0, 5.0, 10.0] {
            let name = format!("{} first, hedge {d}ms", if uni_primary { "uni" } else { "bidir" });
            policies.push((name.clone(), Pol::Hedge { uni_primary, d }, false));
            policies.push((format!("gate + {name}"), Pol::Hedge { uni_primary, d }, true));
        }
    }
    println!(
        "{:32} {:>9} {:>7} {:>7} {:>7} {:>8} {:>8} {:>9} {:>7}",
        "policy", "sum_ms", "p50", "p90", "p99", "max", "cpu_ms", "cpu/min", "hedges"
    );
    let min_sum: f64 = pairs.iter().map(|p| p.t_uni.min(p.t_bi)).sum();
    for (name, pol, gated) in &policies {
        let mut lat = Vec::with_capacity(pairs.len());
        let (mut cpu, mut hedges) = (0.0, 0usize);
        for p in &pairs {
            // The gate judges against the primary: the hedge policies name theirs; the
            // race-now row uses the service default.
            let primary_uni = match pol {
                Pol::Hedge { uni_primary, .. } => *uni_primary,
                _ => auto_uni,
            };
            let pol = if *gated && !p.hint.worth_racing(primary_uni) {
                // Gate says "not worth it": the primary runs alone.
                Pol::Single(primary_uni)
            } else {
                *pol
            };
            let (l, c, h) = sim(pol, p);
            lat.push(l);
            cpu += c;
            hedges += h as usize;
        }
        let sum: f64 = lat.iter().sum();
        let (p50, p90, p99) = (pct(&mut lat, 0.5), pct(&mut lat, 0.9), pct(&mut lat, 0.99));
        let max = lat.iter().cloned().fold(0.0, f64::max);
        println!(
            "{:32} {:9.1} {:7.3} {:7.3} {:7.3} {:8.2} {:8.1} {:9.2} {:7}",
            name, sum, p50, p90, p99, max, cpu, cpu / min_sum, hedges
        );
    }
    let uni_wins = pairs.iter().filter(|p| p.t_uni < p.t_bi).count();
    let tele = pairs.iter().filter(|p| p.hint.teleport_dominated()).count();
    let blind = pairs.iter().filter(|p| p.hint.blind).count();
    let bidir_wins_blind = pairs.iter().filter(|p| p.hint.blind && p.t_bi < p.t_uni).count();
    println!(
        "pairs {} | uni faster on {} | teleport-dominated {} | blind {} (bidir faster on {}) | per-pair-min sum {:.1} ms",
        pairs.len(), uni_wins, tele, blind, bidir_wins_blind, min_sum
    );
}
