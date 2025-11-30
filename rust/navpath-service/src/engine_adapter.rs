use std::sync::Arc;
use std::cell::RefCell;
use std::collections::HashMap;

use navpath_core::{EngineView, SearchParams, SearchResult, Snapshot, NeighborProvider};
use navpath_core::engine::search::SearchContext;
use serde_json::Value as JsonValue;
use navpath_core::eligibility::{build_mask_from_u32, ClientValue};
use navpath_core::engine::heuristics::{NativeLandmarkHeuristic, OctileCoords};
use navpath_core::snapshot::LeSliceI32;

// Pre-parsed macro metadata structures
#[derive(Debug, Clone)]
pub struct DbRowData {
    pub tile_inside: Option<(i32, i32, i32)>,
    pub tile_outside: Option<(i32, i32, i32)>,
    pub raw_data: JsonValue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MacroKind {
    Door,
    Lodestone,
    Npc,
    Object,
    Item,
    Ifslot,
    Teleport,
}

#[derive(Debug, Clone)]
pub struct GlobalDest {
    pub dst: u32,
    pub cost: f32,
    pub reqs: Vec<usize>,
    pub raw_data: JsonValue,
}

#[derive(Debug, Clone)]
pub struct MacroEdgeMeta {
    pub kind: MacroKind,
    pub node_id: u32,
    pub requirements: Vec<usize>,
    pub db_row: Option<DbRowData>,
    pub raw_data: JsonValue,
    pub is_global: bool,
    pub global_dests: Vec<GlobalDest>,
}

thread_local! {
    static SEARCH_CONTEXT: RefCell<SearchContext> = RefCell::new(SearchContext::new(0));
}

// Helper function to extract [x,y,p] tuple from JSON
fn extract_tuple(value: Option<&JsonValue>) -> Option<(i32, i32, i32)> {
    let arr = value?.as_array()?;
    if arr.len() != 3 { return None; }
    let x = arr[0].as_i64()? as i32;
    let y = arr[1].as_i64()? as i32;
    let p = arr[2].as_i64()? as i32;
    Some((x, y, p))
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

pub fn build_neighbor_provider(snapshot: &Snapshot) -> (NeighborProvider, Vec<(u32, f32, Vec<usize>)>, HashMap<(u32, u32), u32>, Vec<u32>, Vec<Option<MacroEdgeMeta>>) {
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
    let mut macro_meta: Vec<Option<MacroEdgeMeta>> = Vec::with_capacity(len);
    
    let msrc_vec: Vec<u32> = msrc.iter().collect();
    let mdst_vec: Vec<u32> = snapshot.macro_dst().iter().collect();
    let mw_vec: Vec<f32> = snapshot.macro_w().iter().collect();

    for idx in 0..len {
        let mut reqs = Vec::new();
        let mut is_global = false;
        let mut parsed_meta: Option<MacroEdgeMeta> = None;

        if let Some(bytes) = snapshot.macro_meta_at(idx) {
            if let Ok(val) = serde_json::from_slice::<JsonValue>(bytes) {
                // Parse macro kind
                let kind = match snapshot.macro_kind_first().get(idx).unwrap_or(0) {
                    1 => MacroKind::Door,
                    2 => MacroKind::Lodestone,
                    3 => MacroKind::Npc,
                    4 => MacroKind::Object,
                    5 => MacroKind::Item,
                    6 => MacroKind::Ifslot,
                    _ => MacroKind::Teleport,
                };
                
                let node_id = snapshot.macro_id_first().get(idx).unwrap_or(0);
                
                // Parse db_row if present
                let db_row = val.get("db_row").map(|db_row| DbRowData {
                    tile_inside: extract_tuple(db_row.get("tile_inside")),
                    tile_outside: extract_tuple(db_row.get("tile_outside")),
                    raw_data: db_row.clone(),
                });
                
                // Check for global def
                let mut global_dests = Vec::new();
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
                                 globals.push((dst, cost, g_reqs.clone()));
                                 global_dests.push(GlobalDest {
                                     dst,
                                     cost,
                                     reqs: g_reqs,
                                     raw_data: g.clone(),
                                 });
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
                
                parsed_meta = Some(MacroEdgeMeta {
                    kind,
                    node_id,
                    requirements: reqs.clone(),
                    db_row,
                    raw_data: val,
                    is_global,
                    global_dests,
                });
            }
        }
        macro_reqs.push(reqs);
        macro_meta.push(parsed_meta);
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
    
    (provider, globals, macro_lookup, req_words, macro_meta)
}

pub fn run_route(
    snapshot: Arc<Snapshot>,
    neighbors: Arc<NeighborProvider>,
    globals: Arc<Vec<(u32, f32, Vec<usize>)>>,
    req_words: Arc<Vec<u32>>,
    start_id: u32, 
    goal_id: u32
) -> SearchResult {
    run_route_with_requirements(snapshot, neighbors, globals, req_words, start_id, goal_id, &[])
}

pub fn run_route_with_requirements(
    snapshot: Arc<Snapshot>,
    neighbors: Arc<NeighborProvider>,
    globals: Arc<Vec<(u32, f32, Vec<usize>)>>,
    req_words: Arc<Vec<u32>>,
    start_id: u32,
    goal_id: u32,
    client_reqs: &[(String, serde_json::Value)],
) -> SearchResult {
    // Build eligibility mask from pre-computed req words
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
    let lm = NativeLandmarkHeuristic::from_le_slices(
        nodes, 
        snapshot.counts().landmarks as usize, 
        snapshot.lm_fw(), 
        snapshot.lm_bw() 
    );
    
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
        view.astar(SearchParams { start: start_id, goal: goal_id, coords: Some(&coords), mask: Some(&mask) }, &mut ctx)
    })
}
