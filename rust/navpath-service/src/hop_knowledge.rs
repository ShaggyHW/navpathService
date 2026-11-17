//! What the launcher's clients learned about Surge/Dive, read from the shared folder
//! they export to (`Hoor2/launcher/payload/hop_knowledge`, README there).
//!
//! Two kinds of file, one per contributor, summed across contributors like the clients do:
//!
//! - **`contrib-<id>.jsonl`, hop edges** (`HopEdges` / `Evidence` in the launcher):
//!   `pe:x:y:plane:DIR` — a hop crosses this step although walking cannot;
//!   `se:x:y:plane:DIR` — a hop stops at this step although walking passes it. A key is
//!   used once its combined evidence passes the launcher's rules ([`Evidence::usable`]).
//!   Lines without a hop model `"m"`, or below [`DISCARD_BELOW`], are ignored. Loc-type
//!   keys (`pt:`) are not used: they need to know which loc blocks each edge, which the
//!   snapshot does not record.
//! - **`schedule-<id>.jsonl`, learned times** (`ScheduleStore` / `ScheduleLearning`):
//!   `dead:<interaction>` — the time from the last walk step before an interaction to the
//!   first one after it, i.e. how long the interaction really takes;
//!   `lock:<what landed the player>` — how long a landing holds the player before a walk
//!   or a hop goes. Weighted sums `w, s1, s2`; a key's weight is capped at 20 and its
//!   estimate is the mean with the client's prior counted as one sample.
//!
//! The folder is `NAVPATH_HOP_KNOWLEDGE_DIR`; a machine's own store (`~/.rs3Client/hops`) can be
//! added with `NAVPATH_HOP_LOCAL_DIR` ([`HopKnowledge::from_layers`]). Both unset = nothing learned and
//! every route is priced exactly as without this module. The files are re-read when a
//! name, size or mtime changes, checked every [`RELOAD_INTERVAL`] (as the clients do), so
//! a `git pull` of the launcher repo reaches the service without a restart. Payloads are
//! rebuilt per request, so no cache needs clearing on a reload.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use arc_swap::ArcSwap;
use rustc_hash::FxHashSet;
use tracing::{info, warn};

use crate::FxHashMap;

/// The launcher's `HopModel.DISCARD_BELOW`: learned edges from an older hop model are void.
pub const DISCARD_BELOW: i64 = 7;
/// A `dead:` / `lock:` key's samples weigh at most this much (`ScheduleStore.cap`).
pub const TIMING_CAP: f64 = 20.0;
/// The client's prior counts as this many samples (`ScheduleLearning.TIMING_PRIOR_WEIGHT`).
pub const TIMING_PRIOR_WEIGHT: f64 = 1.0;
/// The client's landing hold before anything is learned (`RouteWalker.LANDED_HOLD_MS`).
pub const LANDED_HOLD_PRIOR_MS: f64 = 1200.0;
/// How often the folder is checked for changes.
pub const RELOAD_INTERVAL: Duration = Duration::from_secs(30);

/// The client's default hop-free time per node type (`Interactions.DEFAULTS`): the prior
/// of a `dead:` estimate. Types not listed here are never timed by the client.
fn dead_prior(node_type: &str) -> Option<f64> {
    Some(match node_type {
        "DoorNode" => 1_800.0,
        "ObjectNode" | "ItemOnObjectNode" | "IFSlotNode" => 3_000.0,
        "ItemNode" | "POAItemNode" => 4_800.0,
        "FairyRingNode" => 6_000.0,
        "LodestoneNode" => 10_800.0,
        _ => return None,
    })
}

// ---- edges ----

/// The 8 step directions in a fixed order (index = packed direction).
const DIRS: [(i32, i32, &str); 8] =
    [(0, 1, "N"), (1, 1, "NE"), (1, 0, "E"), (1, -1, "SE"), (0, -1, "S"), (-1, -1, "SW"), (-1, 0, "W"), (-1, 1, "NW")];

/// The canonical packed edge of the step from (x, y, plane) by (dx, dy), as the launcher's
/// `HopEdges.pack`: a wall blocks both ways, so S, W, SW and NW steps are turned around
/// onto the neighbour. None for a non-unit step.
pub fn pack_edge(x: i32, y: i32, plane: i32, dx: i32, dy: i32) -> Option<u64> {
    let (x, y, dx, dy) = match (dx, dy) {
        (0, -1) | (-1, 0) | (-1, -1) | (-1, 1) => (x + dx, y + dy, -dx, -dy),
        _ => (x, y, dx, dy),
    };
    let d = DIRS.iter().position(|&(ddx, ddy, _)| ddx == dx && ddy == dy)? as u64;
    Some(((x as u64 & 0xFFFF) << 32) | ((y as u64 & 0xFFFF) << 16) | ((plane as u64 & 0xF) << 4) | d)
}

/// The packed edge of a `pe:` / `se:` key's tail (`x:y:plane:DIR`).
fn parse_edge(rest: &str) -> Option<u64> {
    let mut it = rest.split(':');
    let x = it.next()?.parse().ok()?;
    let y = it.next()?.parse().ok()?;
    let p = it.next()?.parse().ok()?;
    let name = it.next()?;
    if it.next().is_some() {
        return None;
    }
    let &(dx, dy, _) = DIRS.iter().find(|d| d.2 == name)?;
    pack_edge(x, y, p, dx, dy)
}

/// The counts behind one edge key, summed over contributors (`Evidence.add`).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Evidence {
    pub s: i64,
    pub w: i64,
    pub f: i64,
    pub sa: i64,
    pub wa: i64,
    pub xf: i64,
    pub ms: i64,
    pub mf: i64,
    pub msa: i64,
    pub mxf: i64,
}

impl Evidence {
    fn from_json(o: &serde_json::Value) -> Self {
        let g = |k: &str| o.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0);
        Self {
            s: g("s"),
            w: g("w"),
            f: g("f"),
            sa: g("sa"),
            wa: g("wa"),
            xf: g("xf"),
            ms: g("ms"),
            mf: g("mf"),
            msa: g("msa"),
            mxf: g("mxf"),
        }
    }

    /// Fold `o` in: counts add; "since the latest failure" follows the newer failure.
    fn add(&mut self, o: &Evidence) {
        self.s += o.s;
        self.w += o.w;
        self.f += o.f;
        if o.xf > self.xf {
            (self.sa, self.wa, self.xf) = (o.sa, o.wa, o.xf);
        } else if o.xf == self.xf {
            self.sa += o.sa;
            self.wa += o.wa;
        }
        self.ms += o.ms;
        self.mf += o.mf;
        if o.mxf > self.mxf {
            (self.msa, self.mxf) = (o.msa, o.mxf);
        } else if o.mxf == self.mxf {
            self.msa += o.msa;
        }
    }

    /// `Evidence.atLeast`: the same hops seen twice (this machine's local store and its own
    /// shared export) count once — the larger of each count, not the sum.
    fn at_least(&self, o: &Evidence) -> Evidence {
        let mut e = self.clone();
        e.s = self.s.max(o.s);
        e.w = self.w.max(o.w);
        e.f = self.f.max(o.f);
        if o.xf > self.xf {
            (e.sa, e.wa, e.xf) = (o.sa, o.wa, o.xf);
        } else if o.xf == self.xf {
            e.sa = self.sa.max(o.sa);
            e.wa = self.wa.max(o.wa);
        }
        e.ms = self.ms.max(o.ms);
        e.mf = self.mf.max(o.mf);
        if o.mxf > self.mxf {
            (e.msa, e.mxf) = (o.msa, o.mxf);
        } else if o.mxf == self.mxf {
            e.msa = self.msa.max(o.msa);
        }
        e
    }

    /// `Evidence.usableEdge`: the user's hand-cast hops decide once they have a say (one
    /// confirmed success, unless a manual failure came after it); else a score of 4
    /// (2 per strong, 1 per weak success) with no failure, or 6 since the latest failure.
    pub fn usable(&self) -> bool {
        if self.ms + self.mf > 0 {
            return self.ms > 0 && (self.mf == 0 || self.msa > 0);
        }
        if self.f == 0 {
            2 * self.s + self.w >= 4
        } else {
            2 * self.sa + self.wa >= 6
        }
    }
}

// ---- times ----

/// Weighted sums of one timing key's samples (`ScheduleStore.Sums`).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Sums {
    pub w: f64,
    pub s1: f64,
    pub s2: f64,
}

impl Sums {
    fn plus(self, o: Sums) -> Sums {
        Sums { w: self.w + o.w, s1: self.s1 + o.s1, s2: self.s2 + o.s2 }
    }

    fn capped(self, max: f64) -> Sums {
        if self.w <= max || self.w <= 0.0 {
            return self;
        }
        let k = max / self.w;
        Sums { w: self.w * k, s1: self.s1 * k, s2: self.s2 * k }
    }

    /// `ScheduleLearning.mean`: the samples' mean with `prior` counted as one sample.
    fn mean_with_prior(self, prior: f64) -> f64 {
        (self.s1 + TIMING_PRIOR_WEIGHT * prior) / (self.w + TIMING_PRIOR_WEIGHT)
    }
}

// ---- the loaded view ----

/// Everything learned, as read from the folder. Immutable; swapped whole on a reload.
#[derive(Debug, Default)]
pub struct HopKnowledge {
    pass: FxHashSet<u64>,
    stop: FxHashSet<u64>,
    timings: FxHashMap<String, Sums>,
    /// Edge keys read (usable or not) and contributor files, for the log line.
    pub edge_keys: usize,
    pub files: usize,
}

impl HopKnowledge {
    pub fn is_empty(&self) -> bool {
        self.pass.is_empty() && self.stop.is_empty() && self.timings.is_empty()
    }

    pub fn usable_pass_edges(&self) -> usize {
        self.pass.len()
    }

    pub fn usable_stop_edges(&self) -> usize {
        self.stop.len()
    }

    pub fn timing_keys(&self) -> usize {
        self.timings.len()
    }

    /// A hop may cross this step although walking cannot.
    pub fn passable(&self, x: i32, y: i32, plane: i32, dx: i32, dy: i32) -> bool {
        !self.pass.is_empty() && pack_edge(x, y, plane, dx, dy).is_some_and(|e| self.pass.contains(&e))
    }

    /// A hop stops at this step although walking passes it.
    pub fn stops(&self, x: i32, y: i32, plane: i32, dx: i32, dy: i32) -> bool {
        !self.stop.is_empty() && pack_edge(x, y, plane, dx, dy).is_some_and(|e| self.stop.contains(&e))
    }

    fn sums(&self, key: &str) -> Sums {
        self.timings.get(key).copied().unwrap_or_default()
    }

    /// How long an interaction really takes (ms): the learned `dead:` time for `key`
    /// (e.g. `ObjectNode:Climb-over`), else for its `node_type` (`ObjectNode`), as the
    /// client estimates it. None when nothing was learned for either, or the type is one
    /// the client never times (an NPC, a spell).
    pub fn interaction_ms(&self, key: &str, node_type: &str) -> Option<f64> {
        let prior = dead_prior(node_type)?;
        let by_key = self.sums(&format!("dead:{key}"));
        let s = if by_key.w > 0.0 { by_key } else { self.sums(&format!("dead:{node_type}")) };
        (s.w > 0.0).then(|| s.mean_with_prior(prior).round())
    }

    /// How long a landing sent by `key` (`LodestoneNode`, `NpcNode:Charter`, `teleport`)
    /// holds the player (ms). None when nothing was learned for it.
    pub fn landing_hold_ms(&self, key: &str) -> Option<f64> {
        let s = self.sums(&format!("lock:{key}"));
        (s.w > 0.0).then(|| s.mean_with_prior(LANDED_HOLD_PRIOR_MS).round())
    }

    /// Parse the shared folder's files: `(file name, body)` pairs.
    pub fn from_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self::from_layers(files, [], None)
    }

    /// Parse both layers, as the client combines them (`HopKnowledge.rebuildView`,
    /// `ScheduleStore.get`):
    /// - `shared`: the shared folder's `contrib-*.jsonl` / `schedule-*.jsonl`;
    /// - `local`: one machine's store (`~/.rs3Client/hops`), `knowledge.jsonl` (a
    ///   `{"meta":"hops","model":N}` header, then lines without `"m"`; ignored whole below
    ///   [`DISCARD_BELOW`]) and `schedule.jsonl`;
    /// - `me`: that machine's contributor id. Its own shared lines (`"c":me`) hold the same
    ///   hops and samples as its local store, so the two are merged by the larger count
    ///   (edges) or the larger weight (times), never summed. Everyone else's lines add up.
    pub fn from_layers<'a>(
        shared: impl IntoIterator<Item = (&'a str, &'a str)>,
        local: impl IntoIterator<Item = (&'a str, &'a str)>,
        me: Option<&str>,
    ) -> Self {
        let mut ev_others: FxHashMap<String, Evidence> = FxHashMap::default();
        let mut ev_own: FxHashMap<String, Evidence> = FxHashMap::default();
        let mut ev_local: FxHashMap<String, Evidence> = FxHashMap::default();
        let mut t_others: FxHashMap<String, Sums> = FxHashMap::default();
        let mut t_own: FxHashMap<String, Sums> = FxHashMap::default();
        let mut t_local: FxHashMap<String, Sums> = FxHashMap::default();
        let mut n_files = 0;
        let str_of = |o: &serde_json::Value, f: &str| o.get(f).and_then(serde_json::Value::as_str).map(str::to_owned);
        let timing = |o: &serde_json::Value| -> Option<Sums> {
            let g = |f: &str| o.get(f).and_then(serde_json::Value::as_f64);
            let (w, s1, s2) = (g("w")?, g("s1")?, g("s2")?);
            (w >= 0.0).then_some(Sums { w, s1, s2 })
        };
        let is_edge = |k: &str| k.starts_with("pe:") || k.starts_with("se:");
        let is_timing = |k: &str| k.starts_with("dead:") || k.starts_with("lock:");

        for (name, body) in shared {
            let contrib = name.starts_with("contrib-");
            if !(contrib || name.starts_with("schedule-")) || !name.ends_with(".jsonl") {
                continue;
            }
            n_files += 1;
            for line in body.lines() {
                let Ok(o) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                let Some(k) = str_of(&o, "k") else { continue };
                let mine = me.is_some() && str_of(&o, "c").as_deref() == me;
                if contrib {
                    if !is_edge(&k) || o.get("m").and_then(serde_json::Value::as_i64).is_none_or(|m| m < DISCARD_BELOW) {
                        continue;
                    }
                    let into = if mine { &mut ev_own } else { &mut ev_others };
                    into.entry(k).or_default().add(&Evidence::from_json(&o));
                } else if is_timing(&k) {
                    let Some(t) = timing(&o) else { continue };
                    let into = if mine { &mut t_own } else { &mut t_others };
                    let e = into.entry(k).or_default();
                    *e = e.plus(t);
                }
            }
        }

        for (name, body) in local {
            match name {
                "knowledge.jsonl" => {
                    let mut model = -1;
                    let mut lines: FxHashMap<String, Evidence> = FxHashMap::default();
                    for line in body.lines() {
                        let Ok(o) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                        if o.get("meta").is_some() {
                            model = o.get("model").and_then(serde_json::Value::as_i64).unwrap_or(-1);
                            continue;
                        }
                        let Some(k) = str_of(&o, "k") else { continue };
                        if is_edge(&k) {
                            lines.entry(k).or_default().add(&Evidence::from_json(&o));
                        }
                    }
                    n_files += 1;
                    // A store from an older hop model (or without a header) is void, as the
                    // client's `HopStore.discardIfOld` would move it away.
                    if model >= DISCARD_BELOW {
                        ev_local = lines;
                    }
                }
                "schedule.jsonl" => {
                    n_files += 1;
                    for line in body.lines() {
                        let Ok(o) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                        let Some(k) = str_of(&o, "k") else { continue };
                        if !is_timing(&k) {
                            continue;
                        }
                        let Some(t) = timing(&o) else { continue };
                        let e = t_local.entry(k).or_default();
                        *e = e.plus(t);
                    }
                }
                _ => {}
            }
        }

        // Edges: this machine's export and its store by the larger count, then everyone else's added.
        let mut evidence = ev_others;
        let mut mine = ev_own;
        for (k, l) in ev_local {
            let m = match mine.get(&k) {
                Some(o) => o.at_least(&l),
                None => l,
            };
            mine.insert(k, m);
        }
        for (k, m) in mine {
            evidence.entry(k).or_default().add(&m);
        }
        // Times: this machine's export or its store, whichever weighs more, plus everyone else's.
        let mut timings = t_others;
        let mut mine: FxHashMap<String, Sums> = t_own;
        for (k, l) in t_local {
            let o = mine.get(&k).copied().unwrap_or_default();
            mine.insert(k, if l.w >= o.w { l } else { o });
        }
        for (k, m) in mine {
            let e = timings.entry(k).or_default();
            *e = e.plus(m);
        }

        let mut out = HopKnowledge { edge_keys: evidence.len(), files: n_files, ..Default::default() };
        for (k, ev) in &evidence {
            if !ev.usable() {
                continue;
            }
            let Some(edge) = parse_edge(&k[3..]) else { continue };
            if k.starts_with("pe:") {
                out.pass.insert(edge);
            } else {
                out.stop.insert(edge);
            }
        }
        out.timings = timings.into_iter().map(|(k, s)| (k, s.capped(TIMING_CAP))).collect();
        out
    }

    /// Read the shared folder and a machine's local store (missing or unreadable = nothing).
    pub fn load(sources: &Sources) -> Self {
        let read = |files: Vec<(PathBuf, (u64, u128))>| -> Vec<(String, String)> {
            let mut out = Vec::new();
            for (path, _) in files {
                match std::fs::read_to_string(&path) {
                    Ok(b) => out.push((path.file_name().unwrap_or_default().to_string_lossy().into_owned(), b)),
                    Err(e) => warn!(path = %path.display(), error = %e, "hop knowledge: unreadable file skipped"),
                }
            }
            out
        };
        let shared = sources.shared.as_deref().map(|d| read(list_files(d, is_shared_file))).unwrap_or_default();
        let local = sources.local.as_deref().map(|d| read(list_files(d, is_local_file))).unwrap_or_default();
        Self::from_layers(
            shared.iter().map(|(n, b)| (n.as_str(), b.as_str())),
            local.iter().map(|(n, b)| (n.as_str(), b.as_str())),
            sources.contributor.as_deref(),
        )
    }

    /// Read only a shared folder.
    pub fn load_dir(dir: &Path) -> Self {
        Self::load(&Sources { shared: Some(dir.to_path_buf()), ..Default::default() })
    }
}

/// Where the learned data is read from.
#[derive(Debug, Default, Clone)]
pub struct Sources {
    /// The shared folder (`NAVPATH_HOP_KNOWLEDGE_DIR`).
    pub shared: Option<PathBuf>,
    /// One machine's local store (`NAVPATH_HOP_LOCAL_DIR`, e.g. `~/.rs3Client/hops`).
    pub local: Option<PathBuf>,
    /// That machine's contributor id, so its own shared lines are not counted twice.
    pub contributor: Option<String>,
}

impl Sources {
    fn signature(&self) -> Vec<(PathBuf, (u64, u128))> {
        let mut sig = self.shared.as_deref().map(|d| list_files(d, is_shared_file)).unwrap_or_default();
        sig.extend(self.local.as_deref().map(|d| list_files(d, is_local_file)).unwrap_or_default());
        sig
    }
}

fn is_shared_file(n: &str) -> bool {
    (n.starts_with("contrib-") || n.starts_with("schedule-")) && n.ends_with(".jsonl")
}

fn is_local_file(n: &str) -> bool {
    n == "knowledge.jsonl" || n == "schedule.jsonl"
}

/// A 16-hex contributor id, as the launcher hands its clients (`RS3_HOP_CONTRIBUTOR`).
fn valid_contributor(c: &str) -> bool {
    c.len() == 16 && c.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The local store's contributor: `NAVPATH_HOP_CONTRIBUTOR`, else the launcher's
/// `hop_contributor=` in the `config.txt` next to the store (`~/.rs3Client/config.txt`).
fn local_contributor(local: &Path) -> Option<String> {
    if let Some(c) = std::env::var("NAVPATH_HOP_CONTRIBUTOR").ok().map(|c| c.trim().to_lowercase()) {
        if valid_contributor(&c) {
            return Some(c);
        }
        if !c.is_empty() {
            warn!("NAVPATH_HOP_CONTRIBUTOR is not a 16-hex contributor id; ignored");
        }
    }
    let config = std::fs::read_to_string(local.parent()?.join("config.txt")).ok()?;
    config.lines().find_map(|l| {
        let v = l.trim().strip_prefix("hop_contributor=")?.trim().to_lowercase();
        valid_contributor(&v).then_some(v)
    })
}

/// A folder's wanted files with a (size, mtime) signature each, sorted by name.
fn list_files(dir: &Path, wanted: fn(&str) -> bool) -> Vec<(PathBuf, (u64, u128))> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(PathBuf, (u64, u128))> = rd
        .filter_map(Result::ok)
        .filter(|e| wanted(&e.file_name().to_string_lossy()))
        .map(|e| {
            let md = e.metadata().ok();
            let size = md.as_ref().map_or(0, |m| m.len());
            let mtime = md
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            (e.path(), (size, mtime))
        })
        .collect();
    out.sort();
    out
}

// ---- the process-wide current view ----

static CURRENT: OnceLock<ArcSwap<HopKnowledge>> = OnceLock::new();

fn cell() -> &'static ArcSwap<HopKnowledge> {
    CURRENT.get_or_init(|| ArcSwap::from_pointee(HopKnowledge::default()))
}

/// What is learned now (empty until [`start`] loads a folder).
pub fn current() -> Arc<HopKnowledge> {
    cell().load_full()
}

/// Replace the current view (tests, and the reload loop).
pub fn set_current(k: HopKnowledge) {
    cell().store(Arc::new(k));
}

fn log_loaded(src: &Sources, k: &HopKnowledge) {
    info!(
        shared = ?src.shared,
        local = ?src.local,
        contributor = src.contributor.as_deref().unwrap_or("-"),
        files = k.files,
        edge_keys = k.edge_keys,
        usable_pass = k.usable_pass_edges(),
        usable_stop = k.usable_stop_edges(),
        timing_keys = k.timing_keys(),
        "hop knowledge loaded"
    );
}

fn env_dir(var: &str) -> Option<PathBuf> {
    let v = std::env::var(var).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())?;
    let dir = PathBuf::from(v);
    if !dir.is_dir() {
        warn!(dir = %dir.display(), "{var} is not a directory; watching it anyway");
    }
    Some(dir)
}

/// Load `NAVPATH_HOP_KNOWLEDGE_DIR` (the shared folder) and `NAVPATH_HOP_LOCAL_DIR` (a
/// machine's own store) and keep them current: a background thread re-reads them when a
/// file's name, size or mtime changes. A no-op when neither variable is set.
pub fn start() {
    let shared = env_dir("NAVPATH_HOP_KNOWLEDGE_DIR");
    let local = env_dir("NAVPATH_HOP_LOCAL_DIR");
    if shared.is_none() && local.is_none() {
        info!("NAVPATH_HOP_KNOWLEDGE_DIR and NAVPATH_HOP_LOCAL_DIR unset: no learned hop data (hops and interaction times use the built-in model)");
        return;
    }
    let contributor = local.as_deref().and_then(local_contributor);
    if local.is_some() && shared.is_some() && contributor.is_none() {
        warn!("hop knowledge: the local store's contributor is unknown (set NAVPATH_HOP_CONTRIBUTOR); its own shared export is counted on top of it");
    }
    let src = Sources { shared, local, contributor };
    let mut sig = src.signature();
    let k = HopKnowledge::load(&src);
    log_loaded(&src, &k);
    set_current(k);
    let spawned = std::thread::Builder::new().name("hop-knowledge".into()).spawn(move || loop {
        std::thread::sleep(RELOAD_INTERVAL);
        let now = src.signature();
        if now != sig {
            sig = now;
            let k = HopKnowledge::load(&src);
            log_loaded(&src, &k);
            set_current(k);
        }
    });
    if let Err(e) = spawned {
        warn!(error = %e, "hop knowledge: could not start the reload thread; loaded once");
    }
}

/// The client's interaction key and node type for a navpath step kind and its action:
/// `("ObjectNode:Climb-over", "ObjectNode")`, `("DoorNode:Open", "DoorNode")`,
/// `("LodestoneNode", "LodestoneNode")` (`Interactions.key`). None for kinds the client
/// has no node for (a generic teleport).
pub fn client_key(kind: &str, action: Option<&str>) -> Option<(String, &'static str)> {
    let node_type = match kind {
        "door" => "DoorNode",
        "object" => "ObjectNode",
        "npc" => "NpcNode",
        "lodestone" => "LodestoneNode",
        "item" | "item_click" => "ItemNode",
        "poa_item" => "POAItemNode",
        "ifslot" => "IFSlotNode",
        "use_on" => "ItemOnObjectNode",
        "spell" => "SpellNode",
        "fairy_ring" => "FairyRingNode",
        _ => return None,
    };
    let action = action.map(str::trim).filter(|a| !a.is_empty());
    let key = match (node_type, action) {
        // A door is keyed by its open action, "Open" when the data names none.
        ("DoorNode", a) => format!("DoorNode:{}", a.unwrap_or("Open")),
        ("ObjectNode" | "NpcNode", Some(a)) => format!("{node_type}:{a}"),
        _ => node_type.to_string(),
    };
    Some((key, node_type))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_are_canonical_both_ways() {
        // A step and its reverse are one edge.
        assert_eq!(pack_edge(10, 10, 0, 1, 0), pack_edge(11, 10, 0, -1, 0));
        assert_eq!(pack_edge(10, 10, 0, 1, 1), pack_edge(11, 11, 0, -1, -1));
        assert_eq!(pack_edge(10, 10, 0, 1, -1), pack_edge(11, 9, 0, -1, 1));
        assert_eq!(pack_edge(10, 10, 0, 0, -1), pack_edge(10, 9, 0, 0, 1));
        assert_ne!(pack_edge(10, 10, 0, 1, 0), pack_edge(10, 10, 1, 1, 0));
        assert_eq!(parse_edge("3000:3200:0:E"), pack_edge(3001, 3200, 0, -1, 0));
        assert_eq!(parse_edge("3000:3200:0:SW"), pack_edge(3000, 3200, 0, -1, -1));
        assert_eq!(parse_edge("3000:3200:0:X"), None);
    }

    #[test]
    fn evidence_rules_match_the_launcher() {
        let ev = |s, w, f, sa, wa| Evidence { s, w, f, sa, wa, ..Default::default() };
        assert!(ev(2, 0, 0, 0, 0).usable());
        assert!(ev(1, 2, 0, 0, 0).usable());
        assert!(!ev(1, 1, 0, 0, 0).usable());
        assert!(!ev(9, 0, 1, 2, 1).usable(), "after a failure only the successes since count");
        assert!(ev(9, 0, 1, 3, 0).usable());
        // Manual evidence decides once there is any.
        assert!(Evidence { ms: 1, f: 5, ..Default::default() }.usable());
        assert!(!Evidence { s: 10, mf: 1, ..Default::default() }.usable());
        assert!(Evidence { ms: 1, mf: 1, msa: 1, ..Default::default() }.usable());
    }

    #[test]
    fn contributors_add_up_and_old_models_are_ignored() {
        let a = r#"{"k":"pe:100:200:0:E","c":"a","s":1,"w":0,"f":0,"m":7}
{"k":"se:100:201:0:N","c":"a","s":5,"w":0,"f":0,"m":6}
{"k":"pe:300:300:0:N","c":"a","s":3,"w":0,"f":0}
{"k":"pt:1234:10","c":"a","s":0,"w":0,"f":1,"m":7}
not json"#;
        let b = r#"{"k":"pe:100:200:0:E","c":"b","s":1,"w":0,"f":0,"m":7}"#;
        let k = HopKnowledge::from_files([("contrib-a.jsonl", a), ("contrib-b.jsonl", b), ("README.md", a)]);
        // 1 + 1 strong = score 4 across the two contributors.
        assert!(k.passable(100, 200, 0, 1, 0));
        assert!(k.passable(101, 200, 0, -1, 0), "the reverse step is the same edge");
        assert!(!k.stops(100, 201, 0, 0, 1), "model 6 is discarded");
        assert!(!k.passable(300, 300, 0, 0, 1), "no model = discarded");
        assert_eq!(k.files, 2);
        // One contributor alone is not enough.
        let k = HopKnowledge::from_files([("contrib-a.jsonl", a)]);
        assert!(!k.passable(100, 200, 0, 1, 0));
    }

    #[test]
    fn timings_follow_the_client_estimate() {
        let s = r#"{"k":"dead:DoorNode:Open","c":"a","w":20.0,"s1":40000.0,"s2":0}
{"k":"dead:ObjectNode","c":"a","w":3.0,"s1":12000.0,"s2":0}
{"k":"lock:LodestoneNode","c":"a","w":3.0,"s1":9000.0,"s2":0}
{"k":"ptry:1:2","c":"a","w":3.0,"s1":9.0,"s2":0,"m":7}"#;
        let s2 = r#"{"k":"dead:DoorNode:Open","c":"b","w":20.0,"s1":40000.0,"s2":0}"#;
        let k = HopKnowledge::from_files([("schedule-a.jsonl", s), ("schedule-b.jsonl", s2)]);
        // 40 samples of 2,000 ms capped to 20, plus the 1,800 ms prior as one sample.
        assert_eq!(k.interaction_ms("DoorNode:Open", "DoorNode"), Some(((40_000.0 + 1_800.0) / 21.0_f64).round()));
        // An action never seen falls back to its type: (12,000 + 3,000) / 4.
        assert_eq!(k.interaction_ms("ObjectNode:Climb-over", "ObjectNode"), Some(3_750.0));
        // Nothing for the key or the type: not learned.
        // (The client adds every sample to the type too; here the type has none.)
        assert_eq!(k.interaction_ms("DoorNode:Slash", "DoorNode"), None);
        assert_eq!(k.interaction_ms("IFSlotNode", "IFSlotNode"), None);
        assert_eq!(k.interaction_ms("NpcNode:Charter", "NpcNode"), None, "the client never times NPCs");
        // Landing hold: (9,000 + 1,200) / 4.
        assert_eq!(k.landing_hold_ms("LodestoneNode"), Some(2_550.0));
        assert_eq!(k.landing_hold_ms("teleport"), None);
    }

    #[test]
    fn the_local_store_and_its_own_export_count_once() {
        let me = "2ab1952211be1c17";
        // This machine exported 1 strong success; its store has since seen 2 (the same one and a new one).
        let shared = format!(
            r#"{{"k":"pe:100:200:0:E","c":"{me}","s":1,"w":0,"f":0,"m":7}}
{{"k":"pe:100:200:0:E","c":"other","s":0,"w":1,"f":0,"m":7}}"#
        );
        let store = r#"{"meta":"hops","model":7}
{"k":"pe:100:200:0:E","s":1,"w":0,"f":0}"#;
        // max(1, 1) strong + 1 weak from the other = 3 < 4: not usable (summing would give 5).
        let k = HopKnowledge::from_layers([("contrib-x.jsonl", shared.as_str())], [("knowledge.jsonl", store)], Some(me));
        assert!(!k.passable(100, 200, 0, 1, 0));
        let store2 = r#"{"meta":"hops","model":7}
{"k":"pe:100:200:0:E","s":2,"w":0,"f":0}"#;
        let k = HopKnowledge::from_layers([("contrib-x.jsonl", shared.as_str())], [("knowledge.jsonl", store2)], Some(me));
        assert!(k.passable(100, 200, 0, 1, 0), "max(1, 2) strong + 1 weak = 5");
        // Without knowing the contributor, the export adds to the store.
        let k = HopKnowledge::from_layers([("contrib-x.jsonl", shared.as_str())], [("knowledge.jsonl", store)], None);
        assert!(k.passable(100, 200, 0, 1, 0));
        // A store from an old model (or without a header) is void; other local names are ignored.
        let old = r#"{"meta":"hops","model":6}
{"k":"pe:100:200:0:E","s":9,"w":0,"f":0}"#;
        let k = HopKnowledge::from_layers([], [("knowledge.jsonl", old), ("knowledge.jsonl.bak", store2)], Some(me));
        assert!(!k.passable(100, 200, 0, 1, 0));
        let k = HopKnowledge::from_layers([], [("knowledge.jsonl", store2)], Some(me));
        assert!(k.passable(100, 200, 0, 1, 0), "the store alone counts");

        // Times: the heavier of the export and the store, plus everyone else.
        let sched = format!(
            r#"{{"k":"dead:DoorNode:Open","c":"{me}","w":2.0,"s1":4000.0,"s2":0,"m":7}}
{{"k":"dead:DoorNode:Open","c":"other","w":1.0,"s1":3000.0,"s2":0,"m":7}}"#
        );
        let local = r#"{"k":"dead:DoorNode:Open","w":3.0,"s1":9000.0,"s2":0,"m":7}"#;
        let k = HopKnowledge::from_layers([("schedule-x.jsonl", sched.as_str())], [("schedule.jsonl", local)], Some(me));
        // (9,000 + 3,000 + 1,800 prior) / (3 + 1 + 1).
        assert_eq!(k.interaction_ms("DoorNode:Open", "DoorNode"), Some(2_760.0));
    }

    #[test]
    fn contributor_ids() {
        assert!(valid_contributor("2ab1952211be1c17"));
        assert!(!valid_contributor("2AB1952211BE1C17"));
        assert!(!valid_contributor("2ab1952211be1c1"));
    }

    #[test]
    fn client_keys() {
        assert_eq!(client_key("door", None), Some(("DoorNode:Open".into(), "DoorNode")));
        assert_eq!(client_key("door", Some("Slash")), Some(("DoorNode:Slash".into(), "DoorNode")));
        assert_eq!(client_key("object", Some("Climb-over")), Some(("ObjectNode:Climb-over".into(), "ObjectNode")));
        assert_eq!(client_key("object", None), Some(("ObjectNode".into(), "ObjectNode")));
        assert_eq!(client_key("npc", Some("Charter")), Some(("NpcNode:Charter".into(), "NpcNode")));
        assert_eq!(client_key("lodestone", Some("Teleport")), Some(("LodestoneNode".into(), "LodestoneNode")));
        assert_eq!(client_key("teleport", None), None);
    }
}
