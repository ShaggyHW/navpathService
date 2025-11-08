use std::sync::Arc;

use navpath_core::{EngineView, SearchParams, SearchResult, Snapshot};
use serde_json::Value as JsonValue;
use navpath_core::eligibility::{build_mask_from_u32, ClientValue};

pub fn run_route(snapshot: Arc<Snapshot>, start_id: u32, goal_id: u32) -> SearchResult {
    run_route_with_requirements(snapshot, start_id, goal_id, &[])
}

pub fn run_route_with_requirements(
    snapshot: Arc<Snapshot>,
    start_id: u32,
    goal_id: u32,
    client_reqs: &[(String, serde_json::Value)],
) -> SearchResult {
    // Build eligibility mask from snapshot req tags
    let req_words: Vec<u32> = snapshot.req_tags().iter().collect();
    // Build client iterator
    let mut client_pairs: Vec<(String, ClientValue)> = Vec::new();
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
    // Map requirement id -> tag index in mask
    let mut id_to_idx: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    let mut i = 0usize;
    while i + 3 < req_words.len() {
        let req_id = req_words[i];
        id_to_idx.insert(req_id, i / 4);
        i += 4;
    }

    // Filter macro edges by requirements satisfaction
    let msrc = snapshot.macro_src();
    let mdst = snapshot.macro_dst();
    let mw = snapshot.macro_w();
    let mut f_src: Vec<u32> = Vec::new();
    let mut f_dst: Vec<u32> = Vec::new();
    let mut f_w: Vec<f32> = Vec::new();
    for idx in 0..msrc.len() {
        let s = msrc.get(idx).unwrap_or(0);
        let d = mdst.get(idx).unwrap_or(0);
        if s == 0 && d == 0 { continue; } // skip synthetic global carrier
        let w = mw.get(idx).unwrap_or(f32::INFINITY);
        let mut allowed = true;
        if let Some(bytes) = snapshot.macro_meta_at(idx) {
            if let Ok(val) = serde_json::from_slice::<JsonValue>(bytes) {
                if let Some(reqs) = val.get("requirements").and_then(|v| v.as_array()) {
                    for ridv in reqs {
                        if let Some(rid) = ridv.as_u64() {
                            if let Some(&tag_idx) = id_to_idx.get(&(rid as u32)) {
                                if !mask.is_satisfied(tag_idx) { allowed = false; break; }
                            } else { allowed = false; break; }
                        }
                    }
                }
            }
        }
        if allowed {
            f_src.push(s);
            f_dst.push(d);
            f_w.push(w);
        }
    }

    // Build EngineView from parts using filtered macro edges
    let nodes = snapshot.counts().nodes as usize;
    let walk_src: Vec<u32> = snapshot.walk_src().iter().collect();
    let walk_dst: Vec<u32> = snapshot.walk_dst().iter().collect();
    let walk_w: Vec<f32> = snapshot.walk_w().iter().collect();
    let mut view = EngineView::from_parts(
        nodes,
        &walk_src, &walk_dst, &walk_w,
        &f_src, &f_dst, &f_w,
        snapshot.lm_fw(), snapshot.lm_bw(), snapshot.counts().landmarks as usize,
    );

    // Eligible global teleports
    let mut globals: Vec<(u32, f32)> = Vec::new();
    for (idx, (s, d)) in msrc.iter().zip(mdst.iter()).enumerate() {
        if s == 0 && d == 0 {
            if let Some(bytes) = snapshot.macro_meta_at(idx) {
                if let Ok(val) = serde_json::from_slice::<JsonValue>(bytes) {
                    if let Some(arr) = val.get("global").and_then(|v| v.as_array()) {
                        for g in arr {
                            let dst = g.get("dst").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                            let cost = g.get("cost_ms").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                            let mut allowed = true;
                            if let Some(reqs) = g.get("requirements").and_then(|v| v.as_array()) {
                                for ridv in reqs {
                                    if let Some(rid) = ridv.as_u64() {
                                        if let Some(&tag_idx) = id_to_idx.get(&(rid as u32)) {
                                            if !mask.is_satisfied(tag_idx) { allowed = false; break; }
                                        } else { allowed = false; break; }
                                    }
                                }
                            }
                            if dst != 0 && cost.is_finite() && allowed { globals.push((dst, cost)); }
                        }
                    }
                }
            }
            break;
        }
    }
    if !globals.is_empty() {
        let globals_clone = globals.clone();
        view.extra = Some(Box::new(move |_u: u32| -> Vec<(u32, f32)> { globals_clone.clone() }));
    }

    view.astar(SearchParams { start: start_id, goal: goal_id, coords: None::<&DummyCoords> })
}

// Placeholder coords type (octile ignored when None)
struct DummyCoords;
impl navpath_core::OctileCoords for DummyCoords {
    fn coords(&self, _node: u32) -> (i32, i32, i32) { (0, 0, 0) }
}
