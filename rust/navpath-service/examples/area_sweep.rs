//! Route-to-area acceptance sweep on the real snapshot (`/route_area` engine paths).
//!
//! For random (start, 17x17 area) pairs — areas around random tiles on any plane, every
//! 4th around an eligible teleport landing, every 10th start off the snapshot (virtual
//! start) — it checks, for the unidirectional engine (JPS when `NAVPATH_JPS=1` and
//! unseeded) and the bidirectional one:
//!   - area cost == min over EVERY area tile of its single-goal cost (so the
//!     boundary/landing reduction of `routes::entry_tiles` is validated too);
//!   - the path ends on an area tile and holds no earlier area tile;
//!   - the same request returns the same path (determinism);
//!
//! It also reports area latency against a single-goal route to the area's centre tile.
//!
//!   cargo run --release -p navpath-service --example area_sweep -- [pairs=60] [--seeded]
//!       [--profile=all|none] [--size=17]

use std::sync::Arc;

use navpath_core::engine::search::SearchContext;
use navpath_core::{GoalSet, SearchResult, SearchStatus, Snapshot};
use navpath_service::engine_adapter::{
    build_profile_artifacts, run_area_route, run_area_route_virtual_start, run_route_with_requirements_and_fairy_rings,
    run_route_with_requirements_virtual_start, EngineChoice, ProfileArtifacts,
};
use navpath_service::routes::{entry_tiles, resolve_area_goals, GoalArea};
use navpath_service::SnapshotState;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct Ctx<'a> {
    st: &'a SnapshotState,
    arts: &'a ProfileArtifacts,
    mask: &'a navpath_core::eligibility::EligibilityMask,
    seed: Option<u64>,
}

fn single(c: &Ctx, ctxs: &mut (SearchContext, SearchContext), start: Option<u32>, goal: u32, engine: EngineChoice) -> (SearchResult, f64) {
    let st = c.st;
    let t = std::time::Instant::now();
    let res = match start {
        Some(s) => run_route_with_requirements_and_fairy_rings(
            st.snapshot.clone().unwrap(), st.neighbors.clone().unwrap(), st.neighbors_rev.clone(), s, goal, c.mask, c.seed,
            None, c.arts, st.canonical_grid.clone(), engine, ctxs,
        ),
        None => run_route_with_requirements_virtual_start(
            st.snapshot.clone().unwrap(), st.neighbors.clone().unwrap(), st.neighbors_rev.clone(), goal, c.seed, None, c.arts,
            st.canonical_grid.clone(), engine, ctxs,
        )
        .0,
    }
    .res;
    (res, t.elapsed().as_secs_f64() * 1e3)
}

fn area(c: &Ctx, ctxs: &mut (SearchContext, SearchContext), start: Option<u32>, goals: &GoalSet, rep: u32, engine: EngineChoice) -> (SearchResult, &'static str, f64) {
    let st = c.st;
    let t = std::time::Instant::now();
    let out = match start {
        Some(s) => run_area_route(
            st.snapshot.clone().unwrap(), st.neighbors.clone().unwrap(), st.neighbors_rev.clone(), s, goals, rep, c.mask,
            c.seed, None, c.arts, st.canonical_grid.clone(), engine, ctxs,
        ),
        None => run_area_route_virtual_start(
            st.snapshot.clone().unwrap(), st.neighbors.clone().unwrap(), st.neighbors_rev.clone(), goals, rep, c.seed, None,
            c.arts, st.canonical_grid.clone(), engine, ctxs,
        )
        .0,
    };
    (out.res, out.engine, t.elapsed().as_secs_f64() * 1e3)
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let (mut n_pairs, mut seeded, mut profile, mut size) = (60usize, false, "all".to_string(), 17i32);
    for a in std::env::args().skip(1) {
        if a == "--seeded" {
            seeded = true;
        } else if let Some(v) = a.strip_prefix("--profile=") {
            profile = v.to_string();
        } else if let Some(v) = a.strip_prefix("--size=") {
            size = v.parse().expect("--size=<tiles>");
        } else {
            n_pairs = a.parse().expect("pair count");
        }
    }
    let snap_path = std::env::var("SNAPSHOT_PATH")
        .or_else(|_| std::env::var("NAVPATH_BENCH_SNAPSHOT"))
        .unwrap_or_else(|_| format!("{}/../../graph.snapshot", env!("CARGO_MANIFEST_DIR")));
    let snap = Snapshot::open(&snap_path).expect("open snapshot");
    snap.populate();
    let st = SnapshotState::build(snap_path.clone().into(), snap, None);
    let snap: Arc<Snapshot> = st.snapshot.clone().unwrap();
    let nodes = snap.counts().nodes as usize;
    let mask = navpath_core::eligibility::EligibilityMask { satisfied: vec![profile == "all"; snap.req_tags().len() / 4] };
    let arts = build_profile_artifacts(
        st.neighbors.as_ref().unwrap(), st.neighbors_rev.as_deref(), &st.globals, &st.fairy_rings, &mask, false, false,
    );
    let cg = st.comp_graph.clone().unwrap();
    let comps = snap.comp_ids();
    let mut ctxs = (SearchContext::new(nodes), SearchContext::new(nodes));
    let coords = snap.coords_packed();

    let mut state: u64 = 0xA2EA_0000_0000_0017;
    let mut next = |m: usize| -> u32 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((state >> 33) as usize % m) as u32
    };
    let (mut checked, mut tried, mut failures) = (0usize, 0usize, 0usize);
    let (mut ratios_uni, mut ratios_bi) = (Vec::new(), Vec::new());
    let (mut sum_area_uni, mut sum_single_uni, mut sum_area_bi, mut sum_single_bi) = (0.0, 0.0, 0.0, 0.0);
    let (mut sizes, mut interior_wins, mut planes_hi) = (Vec::new(), 0usize, 0usize);
    let mut uni_name = "";
    let (mut max_rel_uni, mut max_rel_bi) = (0f64, 0f64);
    while checked < n_pairs {
        tried += 1;
        let start = if tried % 10 == 0 { None } else { Some(next(nodes)) };
        let centre = if tried % 4 == 0 && !arts.eligible_globals.is_empty() {
            arts.eligible_globals[next(arts.eligible_globals.len()) as usize].0
        } else {
            next(nodes)
        };
        let (cx, cy, cp) = navpath_core::snapshot::unpack_coord(coords[centre as usize]);
        let h = size / 2;
        let ga = GoalArea { min_wx: cx - h, min_wy: cy - h, max_wx: cx - h + size - 1, max_wy: cy - h + size - 1, plane: cp };
        let Ok(resolved) = resolve_area_goals(&snap, Some(ga), None) else { continue };
        let reach = arts.reach(&cg, &mask).reachable_comps(start.map(|s| comps[s as usize]));
        let reachable: Vec<u32> = resolved.iter().copied().filter(|&n| reach(comps[n as usize])).collect();
        let mut targets = entry_tiles(&snap, &st.globals, &resolved, start);
        targets.retain(|&n| reach(comps[n as usize]));
        if reachable.is_empty() || targets.is_empty() {
            continue;
        }
        let seed = if seeded { Some(0x5EED ^ tried as u64) } else { None };
        let c = Ctx { st: &st, arts: &arts, mask: &mask, seed };

        // Brute force over EVERY reachable area tile (not just the reduced set).
        let mut best = f32::INFINITY;
        let mut cost_of = std::collections::HashMap::new();
        for &g in &reachable {
            let (r, _) = single(&c, &mut ctxs, start, g, EngineChoice::Uni);
            assert_eq!(r.status, SearchStatus::Found, "single {start:?}->{g} not proven: {:?}", r.status);
            cost_of.insert(g, r.cost);
            best = best.min(r.cost);
        }
        let goals = GoalSet::new(targets.iter().copied());
        let rep = if goals.contains(centre) { centre } else { goals.nodes()[0] };
        // f32 accumulation order differs between engines (bidir sums its halves apart):
        // ~1e-6 relative on 1000-edge routes; the engine oracle's loose mode uses 1e-4.
        let tol = 0.02 + 2e-5 * best;
        for engine in [EngineChoice::Uni, EngineChoice::Bidir] {
            let _ = area(&c, &mut ctxs, start, &goals, rep, engine); // warm
            let (r1, name, t1) = area(&c, &mut ctxs, start, &goals, rep, engine);
            let (r2, _, t2) = area(&c, &mut ctxs, start, &goals, rep, engine);
            let _ = single(&c, &mut ctxs, start, centre, engine);
            let (s1, ts1) = single(&c, &mut ctxs, start, centre, engine);
            let (_, ts2) = single(&c, &mut ctxs, start, centre, engine);
            let (ta, ts) = (t1.min(t2), ts1.min(ts2));
            let mut fail = |what: String| {
                failures += 1;
                eprintln!("FAIL [{name}] start={start:?} area={ga:?} seed={seed:?}: {what}");
            };
            if r1.status != SearchStatus::Found {
                fail(format!("status {:?}", r1.status));
                continue;
            }
            let rel = ((r1.cost - best) / best.max(1.0)).abs() as f64;
            if engine == EngineChoice::Uni { max_rel_uni = max_rel_uni.max(rel) } else { max_rel_bi = max_rel_bi.max(rel) }
            if (r1.cost - best).abs() > tol {
                fail(format!("cost {} != brute-force min {best}", r1.cost));
            }
            let last = *r1.path.last().unwrap();
            if resolved.binary_search(&last).is_err() {
                fail("path ends off the area".into());
            }
            if let Some(i) = r1.path.iter().position(|n| resolved.binary_search(n).is_ok()) {
                if i + 1 != r1.path.len() {
                    fail(format!("area tile at path index {i} of {}", r1.path.len()));
                }
            }
            if cost_of.get(&last).is_none_or(|&cg| (cg - r1.cost).abs() > tol) {
                fail(format!("reached tile's own cost {:?} != {}", cost_of.get(&last), r1.cost));
            }
            if r1.path != r2.path || r1.cost.to_bits() != r2.cost.to_bits() {
                fail("non-deterministic".into());
            }
            if engine == EngineChoice::Uni {
                uni_name = name;
                let (lx, ly, _) = navpath_core::snapshot::unpack_coord(coords[last as usize]);
                if lx > ga.min_wx && lx < ga.max_wx && ly > ga.min_wy && ly < ga.max_wy {
                    interior_wins += 1;
                }
                if s1.found {
                    ratios_uni.push(ta / ts.max(1e-3));
                    sum_area_uni += ta;
                    sum_single_uni += ts;
                }
            } else if s1.found {
                ratios_bi.push(ta / ts.max(1e-3));
                sum_area_bi += ta;
                sum_single_bi += ts;
            }
        }
        sizes.push((resolved.len(), targets.len()));
        if cp > 0 {
            planes_hi += 1;
        }
        checked += 1;
    }
    let (res_sum, tgt_sum) = sizes.iter().fold((0, 0), |a, s| (a.0 + s.0, a.1 + s.1));
    println!(
        "area_sweep: {checked} pairs ({tried} tried), size {size}x{size}, seeded={seeded}, profile={profile}, uni engine={uni_name}"
    );
    println!(
        "  area tiles resolved {res_sum}, searched after entry-tile reduction {tgt_sum} ({:.0}%); {planes_hi} areas on planes 1-3; {interior_wins} routes ended on an interior tile",
        100.0 * tgt_sum as f64 / res_sum.max(1) as f64
    );
    println!(
        "  uni:   area/single latency median {:.2}x, sum {:.1} ms vs {:.1} ms ({:.2}x)",
        median(&mut ratios_uni), sum_area_uni, sum_single_uni, sum_area_uni / sum_single_uni.max(1e-9)
    );
    println!(
        "  bidir: area/single latency median {:.2}x, sum {:.1} ms vs {:.1} ms ({:.2}x)",
        median(&mut ratios_bi), sum_area_bi, sum_single_bi, sum_area_bi / sum_single_bi.max(1e-9)
    );
    println!("  max |area cost - brute force| / cost: uni {max_rel_uni:.1e}, bidir {max_rel_bi:.1e}");
    println!("  failures: {failures}");
    if failures > 0 {
        std::process::exit(1);
    }
}
