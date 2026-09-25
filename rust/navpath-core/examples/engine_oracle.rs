//! Engine regression + performance oracle.
//!
//! Records, for a fixed set of (start, goal) pairs and every engine configuration the
//! service can run (uni, canonical uni, JPS, bidir, canonical bidir, seeded, virtual
//! start), the exact result of each search (status, cost bits, path, pops) plus the
//! best-of-R wall time. Two recordings are then compared:
//!
//!   cargo run --release -p navpath-core --example engine_oracle -- record base.tsv
//!   # ... change the engine, rebuild ...
//!   cargo run --release -p navpath-core --example engine_oracle -- record new.tsv
//!   cargo run --release -p navpath-core --example engine_oracle -- compare base.tsv new.tsv
//!
//! `compare` reports per configuration the number of results that differ (strict: cost
//! bits, status, pops and path must all match; `--loose` only requires found/status to
//! match and costs to agree within 1e-4 relative — for changes that legitimately move
//! pops or equal-cost tie choices, such as new landmarks or node renumbering) and the
//! speed ratio (total and geometric mean of per-pair best times).
//!
//! Pairs are stored as COORDINATES in a pairs file (default
//! `target/tmp/engine_oracle_pairs.txt`, generated on first use), and paths are hashed
//! by coordinate, so recordings stay comparable across snapshots whose node numbering
//! differs.
//!
//! Env: NAVPATH_BENCH_SNAPSHOT (snapshot path), NAVPATH_HARNESS_COLD=1 (skip populate).
//! Flags: --pairs N (default 240), --reps R (default 3), --pairs-file PATH,
//!        --only cfg1,cfg2 (subset of configurations), --pack-alt (evaluate against an
//!        in-memory clustered u8 encoding of a plain ALT table), --cold (page the ALT
//!        table out of the page cache before each single timed run).

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use navpath_core::engine::canonical::CanonicalGrid;
use navpath_core::engine::neighbors::NeighborProvider;
use navpath_core::engine::search::{BidirParams, SearchContext, SearchParams};
use navpath_core::{EngineView, SearchResult, Snapshot};


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

const CONFIGS: [&str; 9] = [
    "uni",
    "uni_canon",
    "jps",
    "bidir",
    "bidir_canon",
    "uni_seed",
    "bidir_seed",
    "multi_canon",
    "bidir_multi_canon",
];

#[derive(Clone)]
struct Row {
    pair: usize,
    cfg: String,
    found: bool,
    status: String,
    cost_bits: u32,
    pops: u32,
    pops_f: u32,
    pops_b: u32,
    path_len: usize,
    path_hash: u64,
    ns: u64,
}

fn path_hash(snap: &Snapshot, path: &[u32]) -> u64 {
    // FNV-1a over coordinates (numbering-independent).
    let mut h: u64 = 0xcbf29ce484222325;
    for &id in path {
        let (x, y, p) = snap.node_coord(id);
        for v in [x, y, p] {
            h ^= v as u32 as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

fn load_or_make_pairs(snap: &Snapshot, path: &str, n: usize) -> Vec<((i32, i32, i32), (i32, i32, i32))> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let mut out = Vec::new();
        for line in text.lines() {
            let v: Vec<i32> = line.split_whitespace().filter_map(|t| t.parse().ok()).collect();
            if v.len() == 6 {
                out.push(((v[0], v[1], v[2]), (v[3], v[4], v[5])));
            }
        }
        if out.len() >= n {
            out.truncate(n);
            return out;
        }
    }
    let nodes = snap.counts().nodes as usize;
    let comp = snap.comp_ids();
    // Largest walk component: most pairs live there (real traffic); a few arbitrary
    // pairs keep the unreachable / cross-component paths covered.
    let mut sizes: HashMap<u16, usize> = HashMap::new();
    for &c in comp {
        *sizes.entry(c).or_default() += 1;
    }
    let main = sizes.iter().max_by_key(|(_, &s)| s).map(|(&c, _)| c).unwrap_or(0);
    let mut state: u64 = 0x0DDBA11CAFEF00D5;
    let mut next = |m: usize| -> usize {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((state >> 33) as usize) % m
    };
    let mut out = Vec::new();
    while out.len() < n {
        let s = next(nodes);
        let g = next(nodes);
        if s == g {
            continue;
        }
        let arbitrary = out.len() % 20 == 19;
        if !arbitrary && (comp[s] != main || comp[g] != main) {
            continue;
        }
        out.push((snap.node_coord(s as u32), snap.node_coord(g as u32)));
    }
    if let Some(dir) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut f = std::fs::File::create(path).expect("write pairs file");
    for (a, b) in &out {
        writeln!(f, "{} {} {} {} {} {}", a.0, a.1, a.2, b.0, b.1, b.2).unwrap();
    }
    out
}

fn status_str(r: &SearchResult) -> String {
    format!("{:?}", r.status)
}

fn record(args: &[String]) {
    let out_path = args.first().expect("record <out.tsv>").clone();
    let mut n_pairs = 240usize;
    let mut reps = 3usize;
    let mut pairs_file = format!("{}/../../target/tmp/engine_oracle_pairs.txt", env!("CARGO_MANIFEST_DIR"));
    let mut only: Option<Vec<String>> = None;
    let mut pack_alt = false;
    let mut cold = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--pairs" => { n_pairs = args[i + 1].parse().unwrap(); i += 1; }
            "--reps" => { reps = args[i + 1].parse().unwrap(); i += 1; }
            "--pairs-file" => { pairs_file = args[i + 1].clone(); i += 1; }
            "--only" => { only = Some(args[i + 1].split(',').map(|s| s.to_string()).collect()); i += 1; }
            "--pack-alt" => { pack_alt = true; }
            "--cold" => { cold = true; }
            other => panic!("unknown flag {other}"),
        }
        i += 1;
    }
    let path = std::env::var("NAVPATH_BENCH_SNAPSHOT")
        .unwrap_or_else(|_| format!("{}/../../graph.snapshot", env!("CARGO_MANIFEST_DIR")));
    let t_open = Instant::now();
    let snap = Snapshot::open(&path).expect("open snapshot");
    if std::env::var("NAVPATH_HARNESS_COLD").ok().as_deref() != Some("1") {
        snap.populate();
    }
    eprintln!("opened {path} in {:?}", t_open.elapsed());
    let nodes = snap.counts().nodes as usize;

    let pairs = load_or_make_pairs(&snap, &pairs_file, n_pairs);

    // --pack-alt: evaluate against the clustered u8 encoding of the snapshot's plain
    // ALT table (what snapshot v9 alt_format=1 stores), built in memory.
    let packed_data: Vec<u8> = if pack_alt && snap.lm_packed().is_none() {
        let t = Instant::now();
        let (d, _) = navpath_core::snapshot::alt_pack::pack_alt(snap.lm_tab(), nodes, snap.counts().landmarks as usize);
        eprintln!("packed ALT table: {} -> {} bytes in {:?}", snap.lm_tab().len() * 2, d.len(), t.elapsed());
        d
    } else {
        Vec::new()
    };
    let globals = parse_globals(&snap);
    let lm_of = || {
        if packed_data.is_empty() {
            navpath_core::LandmarkHeuristic::from_snapshot(&snap)
        } else {
            navpath_core::LandmarkHeuristic::new_packed(nodes, snap.counts().landmarks as usize, &packed_data, snap.manifest().alt_quantum_ms)
        }
    };
    // The service aggregates the eligible globals' backward-landmark bounds once per
    // profile (T3.9); mirror that so bidirectional timings match production.
    let globals_in_range: Vec<(u32, f32)> = globals.iter().copied().filter(|&(d, _)| (d as usize) < nodes).collect();
    let rev_base = lm_of().rev_base(&globals_in_range);
    let mut view = EngineView::from_snapshot(&snap);
    view.lm = lm_of();
    view.extra.global_rev_base = Some(&rev_base);
    view.extra.global = globals.clone().into();
    let mut sources: Vec<u32> = snap.fairy_nodes().to_vec();
    sources.sort_unstable();
    sources.dedup();
    let mut dests: Vec<(u32, f32)> = snap
        .fairy_nodes()
        .iter()
        .zip(snap.fairy_cost_ms().iter())
        .map(|(&n, &c)| (n, c))
        .collect();
    dests.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.partial_cmp(&b.1).unwrap()));
    view.extra.fairy_sources = sources.into();
    view.extra.fairy_dests = dests.into();

    let t_cg = Instant::now();
    let mut cg = CanonicalGrid::build(
        nodes,
        snap.coords_packed(),
        snap.walk_offsets(),
        snap.walk_dst(),
        snap.macro_src(),
        snap.macro_dst(),
        snap.macro_w(),
    )
    .expect("canonical grid");
    cg.add_stop_nodes(snap.fairy_nodes());
    cg.build_jump_tables(snap.walk_offsets(), snap.walk_dst(), snap.coords_packed());
    let cg = Arc::new(cg);
    eprintln!("canonical grid in {:?}", t_cg.elapsed());

    let macros_rev = NeighborProvider::new(nodes, snap.macro_dst(), snap.macro_src(), snap.macro_w());
    let bp = BidirParams { macros_rev: &macros_rev, macro_filter_rev: None };

    let mut ctx = SearchContext::new(nodes);
    let mut cf = SearchContext::new(nodes);
    let mut cb = SearchContext::new(nodes);
    ctx.prefault();
    cf.prefault();
    cb.prefault();

    let mut rows: Vec<Row> = Vec::new();
    let t_all = Instant::now();
    let cfgs: Vec<&str> = CONFIGS
        .iter()
        .copied()
        .filter(|c| only.as_ref().is_none_or(|o| o.iter().any(|x| x == c)))
        .collect();
    let mut skipped = 0usize;
    for (pi, &(a, b)) in pairs.iter().enumerate() {
        let (Some(s), Some(g)) = (snap.find_node(a.0, a.1, a.2), snap.find_node(b.0, b.1, b.2)) else {
            skipped += 1;
            continue;
        };
        for &cfg in &cfgs {
            let seed = if cfg.ends_with("_seed") { Some(1u64) } else { None };
            let canon = cfg.contains("canon") || cfg == "jps";
            view.canonical = if canon { Some(cg.clone()) } else { None };
            view.jps = cfg == "jps";
            let params = || SearchParams {
                start: s,
                goal: g,
                macro_filter: None,
                seed,
                max_pops: Some(2_000_000),
                cancel: None,
                bucket_ms: 0.0,
            };
            let mut best = u64::MAX;
            let mut res: Option<SearchResult> = None;
            // --cold: page the ALT table out before a single timed run, i.e. the
            // steady state of a host under memory pressure (head kept warm).
            let reps = if cold {
                if let Err(e) = snap.evict(true) {
                    eprintln!("evict failed: {e}");
                }
                1
            } else {
                reps
            };
            for _ in 0..reps {
                let t = Instant::now();
                let r = match cfg {
                    "uni" | "uni_canon" | "jps" | "uni_seed" => view.astar(params(), &mut ctx),
                    "bidir" | "bidir_canon" | "bidir_seed" => view.astar_bidir(&bp, params(), &mut cf, &mut cb),
                    "multi_canon" => view.astar_multi(&globals, params(), &mut ctx),
                    "bidir_multi_canon" => view.astar_bidir_multi(&globals, &bp, params(), &mut cf, &mut cb),
                    _ => unreachable!(),
                };
                best = best.min(t.elapsed().as_nanos() as u64);
                res = Some(r);
            }
            let r = res.unwrap();
            rows.push(Row {
                pair: pi,
                cfg: cfg.to_string(),
                found: r.found,
                status: status_str(&r),
                cost_bits: r.cost.to_bits(),
                pops: r.pops,
                pops_f: r.pops_f,
                pops_b: r.pops_b,
                path_len: r.path.len(),
                path_hash: path_hash(&snap, &r.path),
                ns: best,
            });
        }
    }
    eprintln!("ran {} searches ({} pairs skipped) in {:?}", rows.len() * reps, skipped, t_all.elapsed());
    let mut f = std::fs::File::create(&out_path).expect("create out");
    writeln!(f, "pair\tcfg\tfound\tstatus\tcost_bits\tcost\tpops\tpops_f\tpops_b\tpath_len\tpath_hash\tns").unwrap();
    for r in &rows {
        writeln!(
            f,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:016x}\t{}",
            r.pair, r.cfg, r.found, r.status, r.cost_bits, f32::from_bits(r.cost_bits), r.pops, r.pops_f, r.pops_b, r.path_len, r.path_hash, r.ns
        )
        .unwrap();
    }
    // Per-config totals for a quick look.
    for &cfg in &cfgs {
        let sel: Vec<&Row> = rows.iter().filter(|r| r.cfg == cfg).collect();
        let t: u64 = sel.iter().map(|r| r.ns).sum();
        let p: u64 = sel.iter().map(|r| r.pops as u64).sum();
        eprintln!("{cfg:>18}: total {:>9.1} ms  pops {:>10}", t as f64 / 1e6, p);
    }
}

fn read_rows(path: &str) -> Vec<Row> {
    let text = std::fs::read_to_string(path).expect("read tsv");
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let c: Vec<&str> = line.split('\t').collect();
        if c.len() < 12 {
            continue;
        }
        out.push(Row {
            pair: c[0].parse().unwrap(),
            cfg: c[1].to_string(),
            found: c[2] == "true",
            status: c[3].to_string(),
            cost_bits: c[4].parse().unwrap(),
            pops: c[6].parse().unwrap(),
            pops_f: c[7].parse().unwrap(),
            pops_b: c[8].parse().unwrap(),
            path_len: c[9].parse().unwrap(),
            path_hash: u64::from_str_radix(c[10], 16).unwrap(),
            ns: c[11].parse().unwrap(),
        });
    }
    out
}

fn compare(args: &[String]) {
    let a = read_rows(&args[0]);
    let b = read_rows(&args[1]);
    let loose = args.iter().any(|x| x == "--loose");
    let bmap: HashMap<(usize, String), Row> = b.into_iter().map(|r| ((r.pair, r.cfg.clone()), r)).collect();
    let mut by_cfg: Vec<String> = Vec::new();
    for r in &a {
        if !by_cfg.contains(&r.cfg) {
            by_cfg.push(r.cfg.clone());
        }
    }
    let mut total_bad = 0usize;
    println!(
        "{:>18} {:>6} {:>6} {:>11} {:>11} {:>8} {:>8} {:>12} {:>12}",
        "cfg", "n", "diff", "ms_a", "ms_b", "tot_x", "geo_x", "pops_a", "pops_b"
    );
    for cfg in &by_cfg {
        let mut n = 0usize;
        let mut bad = 0usize;
        let (mut ta, mut tb) = (0u64, 0u64);
        let (mut pa, mut pb) = (0u64, 0u64);
        let mut log_sum = 0f64;
        let mut log_n = 0usize;
        for ra in a.iter().filter(|r| &r.cfg == cfg) {
            let Some(rb) = bmap.get(&(ra.pair, cfg.clone())) else { continue };
            n += 1;
            let same = if loose {
                let (ca, cb) = (f32::from_bits(ra.cost_bits), f32::from_bits(rb.cost_bits));
                ra.found == rb.found
                    && ra.status == rb.status
                    && (!ra.found || (ca - cb).abs() <= 1e-4 * ca.abs().max(1.0))
            } else {
                ra.found == rb.found
                    && ra.status == rb.status
                    && ra.cost_bits == rb.cost_bits
                    && ra.pops == rb.pops
                    && ra.pops_f == rb.pops_f
                    && ra.pops_b == rb.pops_b
                    && ra.path_len == rb.path_len
                    && ra.path_hash == rb.path_hash
            };
            if !same {
                bad += 1;
                if bad <= 5 {
                    println!(
                        "  DIFF pair {} {}: a found={} {} cost={} pops={}({}/{}) len={} | b found={} {} cost={} pops={}({}/{}) len={}",
                        ra.pair, cfg,
                        ra.found, ra.status, f32::from_bits(ra.cost_bits), ra.pops, ra.pops_f, ra.pops_b, ra.path_len,
                        rb.found, rb.status, f32::from_bits(rb.cost_bits), rb.pops, rb.pops_f, rb.pops_b, rb.path_len
                    );
                }
            }
            ta += ra.ns;
            tb += rb.ns;
            pa += ra.pops as u64;
            pb += rb.pops as u64;
            if ra.ns > 20_000 && rb.ns > 0 {
                log_sum += (ra.ns as f64 / rb.ns as f64).ln();
                log_n += 1;
            }
        }
        total_bad += bad;
        let geo = if log_n > 0 { (log_sum / log_n as f64).exp() } else { f64::NAN };
        println!(
            "{:>18} {:>6} {:>6} {:>11.1} {:>11.1} {:>8.3} {:>8.3} {:>12} {:>12}",
            cfg, n, bad, ta as f64 / 1e6, tb as f64 / 1e6, ta as f64 / tb.max(1) as f64, geo, pa, pb
        );
    }
    println!("(x > 1 means b is faster; geo_x over pairs where a took > 20 µs)");
    std::process::exit(if total_bad == 0 { 0 } else { 1 });
}

/// Heuristic-value comparison of the plain table vs its packed encoding: how often and
/// by how much packing weakens h (forward and backward) on random (node, goal) pairs.
fn hstats() {
    let path = std::env::var("NAVPATH_BENCH_SNAPSHOT")
        .unwrap_or_else(|_| format!("{}/../../graph.snapshot", env!("CARGO_MANIFEST_DIR")));
    let snap = Snapshot::open(&path).expect("open snapshot");
    let nodes = snap.counts().nodes as usize;
    let l = snap.counts().landmarks as usize;
    let q = snap.manifest().alt_quantum_ms;
    let (packed, _) = navpath_core::snapshot::alt_pack::pack_alt(snap.lm_tab(), nodes, l);
    let plain = navpath_core::LandmarkHeuristic::new(nodes, l, snap.lm_tab(), q);
    let pk = navpath_core::LandmarkHeuristic::new_packed(nodes, l, &packed, q);
    let mut state: u64 = 42;
    let mut next = |m: usize| -> u32 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((state >> 33) as usize % m) as u32
    };
    let (mut n, mut weaker, mut inf_lost) = (0u64, 0u64, 0u64);
    let (mut sum_p, mut sum_k) = (0f64, 0f64);
    for _ in 0..300 {
        let goal = next(nodes);
        let a = plain.select_active(goal, goal, usize::MAX);
        let b = pk.select_active(goal, goal, usize::MAX);
        for _ in 0..3000 {
            let u = next(nodes);
            let (hp, hk) = (plain.h_active(u, &a), pk.h_active(u, &b));
            n += 1;
            if hp.is_infinite() {
                if !hk.is_infinite() { inf_lost += 1; }
                continue;
            }
            assert!(hk <= hp, "packed stronger than plain: {hk} > {hp}");
            if hk < hp { weaker += 1; }
            sum_p += hp as f64;
            sum_k += hk as f64;
        }
    }
    println!("evals {n}: packed weaker on {:.2}%, INF lost on {:.2}%, mean h plain {:.0} ms vs packed {:.0} ms ({:.2}%)",
        100.0 * weaker as f64 / n as f64, 100.0 * inf_lost as f64 / n as f64, sum_p / n as f64, sum_k / n as f64, 100.0 * sum_k / sum_p);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("hstats") => hstats(),
        Some("record") => record(&args[1..]),
        Some("compare") => compare(&args[1..]),
        _ => {
            eprintln!("usage: engine_oracle record <out.tsv> [--pairs N] [--reps R] [--pairs-file P] [--only a,b]\n       engine_oracle compare <a.tsv> <b.tsv> [--loose]");
            std::process::exit(2);
        }
    }
}
