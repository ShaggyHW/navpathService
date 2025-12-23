use std::sync::Arc;
use std::cell::RefCell;
use std::collections::HashMap;

use navpath_core::{EngineView, SearchParams, SearchResult, Snapshot, NeighborProvider};
use navpath_core::engine::search::SearchContext;
use serde_json::Value as JsonValue;
use navpath_core::eligibility::{build_mask_from_u32, fnv1a32, ClientValue};
use navpath_core::engine::heuristics::{LandmarkHeuristic, OctileCoords};
use navpath_core::snapshot::LeSliceI32;
use tracing::{info, warn};

thread_local! {
    static SEARCH_CONTEXT: RefCell<SearchContext> = RefCell::new(SearchContext::new(0));
}

struct SnapshotCoords<'a> {
    x: LeSliceI32<'a>,
    y: LeSliceI32<'a>,
    p: LeSliceI32<'a>,
}

impl<'a> OctileCoords for SnapshotCoords<'a> {
    fn coords(&self, node: u32) -> (i32, i32, i32) {
        let i = node as usize;
        (
            self.x.get(i).unwrap_or(0),
            self.y.get(i).unwrap_or(0),
            self.p.get(i).unwrap_or(0)
        )
    }
}

#[derive(Clone, Debug)]
pub struct GlobalTeleport {
    pub dst: u32,
    pub cost: f32,
    pub reqs: Vec<usize>,
    pub kind_first: u32,
}

fn kind_code(kind: &str) -> u32 {
    match kind {
        "door" => 1,
        "lodestone" => 2,
        "npc" => 3,
        "object" => 4,
        "item" => 5,
        "ifslot" => 6,
        _ => 0,
    }
}

fn has_quick_tele(client_reqs: &[(String, serde_json::Value)]) -> bool {
    for (k, v) in client_reqs {
        if k.trim().eq_ignore_ascii_case("hasQuickTele") {
            if v.as_i64() == Some(1) || v.as_u64() == Some(1) {
                return true;
            }
            if v.as_bool() == Some(true) {
                return true;
            }
            if v.as_str().map(|s| s.trim()) == Some("1") {
                return true;
            }
        }
    }
    false
}

pub fn build_neighbor_provider(snapshot: &Snapshot) -> (NeighborProvider, Vec<GlobalTeleport>, HashMap<(u32, u32), u32>) {
    // 1. Build map of req_id -> tag_index
    let req_words: Vec<u32> = snapshot.req_tags().iter().collect();
    let mut id_to_idx = std::collections::HashMap::new();
    let mut i = 0;
    while i + 3 < req_words.len() {
        let req_id = req_words[i];
        id_to_idx.insert(req_id, i / 4);
        i += 4;
    }

    // 2. Iterate macro edges and parse requirements
    let msrc = snapshot.macro_src();
    let len = msrc.len();
    let mut macro_reqs: Vec<Vec<usize>> = Vec::with_capacity(len);
    let mut globals: Vec<GlobalTeleport> = Vec::new();
    let mut macro_lookup: HashMap<(u32, u32), u32> = HashMap::with_capacity(len);
    
    let msrc_vec: Vec<u32> = msrc.iter().collect();
    let mdst_vec: Vec<u32> = snapshot.macro_dst().iter().collect();
    let mw_vec: Vec<f32> = snapshot.macro_w().iter().collect();
    let mkind_vec: Vec<u32> = snapshot.macro_kind_first().iter().collect();

    let mut missing_req_ids: u64 = 0;

    for idx in 0..len {
        let mut reqs = Vec::new();
        let mut is_global = false;

        if let Some(bytes) = snapshot.macro_meta_at(idx) {
            if let Ok(val) = serde_json::from_slice::<JsonValue>(bytes) {
                // Check for global def
                if msrc_vec[idx] == 0 && mdst_vec[idx] == 0 {
                    is_global = true;
                    if let Some(arr) = val.get("global").and_then(|v| v.as_array()) {
                         for g in arr {
                             let dst = g.get("dst").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                             let cost = g.get("cost_ms").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                             let mut g_reqs = Vec::new();
                             let kind_first = g
                                 .get("steps")
                                 .and_then(|v| v.as_array())
                                 .and_then(|a| a.first())
                                 .and_then(|s| s.get("kind"))
                                 .and_then(|v| v.as_str())
                                 .map(kind_code)
                                 .unwrap_or(0);
                             if let Some(r_arr) = g.get("requirements").and_then(|v| v.as_array()) {
                                 for ridv in r_arr {
                                     if let Some(rid) = ridv.as_u64() {
                                         if let Some(&tag_idx) = id_to_idx.get(&(rid as u32)) {
                                             g_reqs.push(tag_idx);
                                         } else {
                                             // Fail-closed: unknown requirement id means the edge can never be satisfied
                                             g_reqs.push(usize::MAX);
                                             missing_req_ids += 1;
                                         }
                                     }
                                 }
                             }
                             if dst != 0 {
                                 globals.push(GlobalTeleport { dst, cost, reqs: g_reqs, kind_first });
                             }
                         }
                    }
                }

                if !is_global {
                    if let Some(arr) = val.get("requirements").and_then(|v| v.as_array()) {
                        for ridv in arr {
                            if let Some(rid) = ridv.as_u64() {
                                if let Some(&tag_idx) = id_to_idx.get(&(rid as u32)) {
                                    reqs.push(tag_idx);
                                } else {
                                    // Fail-closed: unknown requirement id means the edge can never be satisfied
                                    reqs.push(usize::MAX);
                                    missing_req_ids += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        macro_reqs.push(reqs);
        macro_lookup.insert((msrc_vec[idx], mdst_vec[idx]), idx as u32);
    }

    if missing_req_ids > 0 {
        warn!(missing_req_ids, "snapshot macro metadata referenced unknown requirement ids (will be treated as unsatisfied)");
    }

    // 3. Build NeighborProvider
    let nodes = snapshot.counts().nodes as usize;
    let walk_src: Vec<u32> = snapshot.walk_src().iter().collect();
    let walk_dst: Vec<u32> = snapshot.walk_dst().iter().collect();
    let walk_w: Vec<f32> = snapshot.walk_w().iter().collect();

    let provider = NeighborProvider::new_with_reqs(
        nodes,
        &walk_src, &walk_dst, &walk_w,
        &msrc_vec, &mdst_vec, &mw_vec,
        &mkind_vec,
        &macro_reqs
    );
    
    (provider, globals, macro_lookup)
}

pub fn run_route(
    snapshot: Arc<Snapshot>,
    neighbors: Arc<NeighborProvider>,
    globals: Arc<Vec<GlobalTeleport>>,
    start_id: u32, 
    goal_id: u32
) -> SearchResult {
    run_route_with_requirements(snapshot, neighbors, globals, start_id, goal_id, &[])
}

pub fn run_route_with_requirements(
    snapshot: Arc<Snapshot>,
    neighbors: Arc<NeighborProvider>,
    globals: Arc<Vec<GlobalTeleport>>,
    start_id: u32,
    goal_id: u32,
    client_reqs: &[(String, serde_json::Value)],
) -> SearchResult {
    // Build eligibility mask from snapshot req tags
    let req_words: Vec<u32> = snapshot.req_tags().iter().collect();
    let mut client_pairs: Vec<(String, ClientValue)> = Vec::with_capacity(client_reqs.len());
    for (k, v) in client_reqs.iter() {
        if let Some(n) = v.as_i64() {
            client_pairs.push((k.clone(), ClientValue::Num(n)));
        } else if let Some(u) = v.as_u64() {
            client_pairs.push((k.clone(), ClientValue::Num(u as i64)));
        } else if let Some(f) = v.as_f64() {
            client_pairs.push((k.clone(), ClientValue::Num(f as i64)));
        } else if let Some(s) = v.as_str() {
            client_pairs.push((k.clone(), ClientValue::Str(s)));
        }
    }
    let mask = build_mask_from_u32(
        &req_words,
        client_pairs.iter().map(|(k, cv)| {
            match cv {
                ClientValue::Num(n) => (k.as_str(), ClientValue::Num(*n)),
                ClientValue::Str(s) => (k.as_str(), ClientValue::Str(s)),
            }
        }),
    );

    if let Some((k, v)) = client_reqs
        .iter()
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("hasGamesNeck"))
    {
        info!(key = %k, value = %v, "client requirement");
    }

    // Diagnostics: show computed satisfaction for requirement id 78 (expected key=hasGamesNeck, value=1)
    let mut i = 0usize;
    while i + 3 < req_words.len() {
        if req_words[i] == 78 {
            let tag_idx = i / 4;
            let key_id = req_words[i + 1];
            let opbits = req_words[i + 2];
            let rhs_val = req_words[i + 3];
            let expected_key_id = fnv1a32("hasgamesneck");
            let key_matches = key_id == expected_key_id;
            info!(tag_idx, key_id, expected_key_id, key_matches, opbits, rhs_val, satisfied = mask.is_satisfied(tag_idx), "req_id 78 evaluation");
            break;
        }
        i += 4;
    }

    let quick_tele = has_quick_tele(client_reqs);

    // Eligible global teleports
    let mut eligible_globals: Vec<(u32, f32)> = Vec::new();
    for g in globals.iter() {
        let mut allowed = true;
        for &idx in &g.reqs {
            if !mask.is_satisfied(idx) { allowed = false; break; }
        }
        if allowed {
            let mut cost = g.cost;
            if quick_tele && g.kind_first == 2 {
                cost = 2400.0;
            }
            eligible_globals.push((g.dst, cost));
        }
    }

    let nodes = snapshot.counts().nodes as usize;
    let lm = LandmarkHeuristic { 
        nodes, 
        landmarks: snapshot.counts().landmarks as usize, 
        lm_fw: snapshot.lm_fw(), 
        lm_bw: snapshot.lm_bw() 
    };
    
    let mut view = EngineView {
        nodes,
        neighbors,
        lm,
        extra: None,
    };

    if !eligible_globals.is_empty() {
        view.extra = Some(Box::new(move |_u: u32| -> Vec<(u32, f32)> { eligible_globals.clone() }));
    }

    let coords = SnapshotCoords {
        x: snapshot.nodes_x(),
        y: snapshot.nodes_y(),
        p: snapshot.nodes_plane(),
    };

    SEARCH_CONTEXT.with(|ctx_cell| {
        let mut ctx = ctx_cell.borrow_mut();
        view.astar(SearchParams { start: start_id, goal: goal_id, coords: Some(&coords), mask: Some(&mask), quick_tele }, &mut ctx)
    })
}

pub fn run_route_with_requirements_virtual_start(
    snapshot: Arc<Snapshot>,
    neighbors: Arc<NeighborProvider>,
    globals: Arc<Vec<GlobalTeleport>>,
    goal_id: u32,
    client_reqs: &[(String, serde_json::Value)],
) -> (SearchResult, Option<u32>) {
    let req_words: Vec<u32> = snapshot.req_tags().iter().collect();
    let mut client_pairs: Vec<(String, ClientValue)> = Vec::with_capacity(client_reqs.len());
    for (k, v) in client_reqs.iter() {
        if let Some(n) = v.as_i64() {
            client_pairs.push((k.clone(), ClientValue::Num(n)));
        } else if let Some(u) = v.as_u64() {
            client_pairs.push((k.clone(), ClientValue::Num(u as i64)));
        } else if let Some(f) = v.as_f64() {
            client_pairs.push((k.clone(), ClientValue::Num(f as i64)));
        } else if let Some(s) = v.as_str() {
            client_pairs.push((k.clone(), ClientValue::Str(s)));
        }
    }
    let mask = build_mask_from_u32(
        &req_words,
        client_pairs.iter().map(|(k, cv)| {
            match cv {
                ClientValue::Num(n) => (k.as_str(), ClientValue::Num(*n)),
                ClientValue::Str(s) => (k.as_str(), ClientValue::Str(s)),
            }
        }),
    );

    let quick_tele = has_quick_tele(client_reqs);

    let mut eligible_globals: Vec<(u32, f32)> = Vec::new();
    for g in globals.iter() {
        let mut allowed = true;
        for &idx in &g.reqs {
            if !mask.is_satisfied(idx) {
                allowed = false;
                break;
            }
        }
        if allowed {
            let mut cost = g.cost;
            if quick_tele && g.kind_first == 2 {
                cost = 2400.0;
            }
            eligible_globals.push((g.dst, cost));
        }
    }

    let nodes = snapshot.counts().nodes as usize;
    let lm = LandmarkHeuristic {
        nodes,
        landmarks: snapshot.counts().landmarks as usize,
        lm_fw: snapshot.lm_fw(),
        lm_bw: snapshot.lm_bw(),
    };
    let mut view = EngineView {
        nodes,
        neighbors,
        lm,
        extra: None,
    };

    if !eligible_globals.is_empty() {
        let eligible_globals_for_extra = eligible_globals.clone();
        view.extra = Some(Box::new(move |_u: u32| -> Vec<(u32, f32)> { eligible_globals_for_extra.clone() }));
    }

    let coords = SnapshotCoords {
        x: snapshot.nodes_x(),
        y: snapshot.nodes_y(),
        p: snapshot.nodes_plane(),
    };

    if eligible_globals.is_empty() {
        return (SearchResult { found: false, path: Vec::new(), cost: f32::INFINITY }, None);
    }

    SEARCH_CONTEXT.with(|ctx_cell| {
        let mut ctx = ctx_cell.borrow_mut();
        let mut best: Option<(SearchResult, u32)> = None;

        for (dst, tele_cost) in eligible_globals.into_iter() {
            let res = view.astar(
                SearchParams { start: dst, goal: goal_id, coords: Some(&coords), mask: Some(&mask), quick_tele },
                &mut ctx,
            );
            if !res.found {
                continue;
            }
            let total_cost = tele_cost + res.cost;
            let mut combined = res.clone();
            combined.cost = total_cost;

            match &mut best {
                None => best = Some((combined, dst)),
                Some((cur_best, cur_dst)) => {
                    if total_cost < cur_best.cost {
                        *cur_best = combined;
                        *cur_dst = dst;
                    }
                }
            }
        }

        match best {
            Some((res, dst)) => (res, Some(dst)),
            None => (SearchResult { found: false, path: Vec::new(), cost: f32::INFINITY }, None),
        }
    })
}
