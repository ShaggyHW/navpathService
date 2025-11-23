use std::sync::Arc;
use std::cell::RefCell;
use std::collections::HashMap;

use navpath_core::{EngineView, SearchParams, SearchResult, Snapshot, NeighborProvider};
use navpath_core::engine::search::SearchContext;
use serde_json::Value as JsonValue;
use navpath_core::eligibility::{build_mask_from_u32, ClientValue};
use navpath_core::engine::heuristics::LandmarkHeuristic;

thread_local! {
    static SEARCH_CONTEXT: RefCell<SearchContext> = RefCell::new(SearchContext::new(0));
}

pub fn build_neighbor_provider(snapshot: &Snapshot) -> (NeighborProvider, Vec<(u32, f32, Vec<usize>)>, HashMap<(u32, u32), u32>) {
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
    let mut globals: Vec<(u32, f32, Vec<usize>)> = Vec::new();
    let mut macro_lookup: HashMap<(u32, u32), u32> = HashMap::with_capacity(len);
    
    let msrc_vec: Vec<u32> = msrc.iter().collect();
    let mdst_vec: Vec<u32> = snapshot.macro_dst().iter().collect();
    let mw_vec: Vec<f32> = snapshot.macro_w().iter().collect();

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
                             if let Some(r_arr) = g.get("requirements").and_then(|v| v.as_array()) {
                                 for ridv in r_arr {
                                     if let Some(rid) = ridv.as_u64() {
                                         if let Some(&tag_idx) = id_to_idx.get(&(rid as u32)) {
                                             g_reqs.push(tag_idx);
                                         }
                                     }
                                 }
                             }
                             if dst != 0 {
                                 globals.push((dst, cost, g_reqs));
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

    // 3. Build NeighborProvider
    let nodes = snapshot.counts().nodes as usize;
    let walk_src: Vec<u32> = snapshot.walk_src().iter().collect();
    let walk_dst: Vec<u32> = snapshot.walk_dst().iter().collect();
    let walk_w: Vec<f32> = snapshot.walk_w().iter().collect();

    let provider = NeighborProvider::new_with_reqs(
        nodes,
        &walk_src, &walk_dst, &walk_w,
        &msrc_vec, &mdst_vec, &mw_vec,
        &macro_reqs
    );
    
    (provider, globals, macro_lookup)
}

pub fn run_route(
    snapshot: Arc<Snapshot>,
    neighbors: Arc<NeighborProvider>,
    globals: Arc<Vec<(u32, f32, Vec<usize>)>>,
    start_id: u32, 
    goal_id: u32
) -> SearchResult {
    run_route_with_requirements(snapshot, neighbors, globals, start_id, goal_id, &[])
}

pub fn run_route_with_requirements(
    snapshot: Arc<Snapshot>,
    neighbors: Arc<NeighborProvider>,
    globals: Arc<Vec<(u32, f32, Vec<usize>)>>,
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

    // Eligible global teleports
    let mut eligible_globals: Vec<(u32, f32)> = Vec::new();
    for (dst, cost, reqs) in globals.iter() {
        let mut allowed = true;
        for &idx in reqs {
            if !mask.is_satisfied(idx) { allowed = false; break; }
        }
        if allowed {
            eligible_globals.push((*dst, *cost));
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

    SEARCH_CONTEXT.with(|ctx_cell| {
        let mut ctx = ctx_cell.borrow_mut();
        view.astar(SearchParams { start: start_id, goal: goal_id, coords: None::<&DummyCoords>, mask: Some(&mask) }, &mut ctx)
    })
}

// Placeholder coords type (octile ignored when None)
struct DummyCoords;
impl navpath_core::OctileCoords for DummyCoords {
    fn coords(&self, _node: u32) -> (i32, i32, i32) { (0, 0, 0) }
}
