use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;

use crate::snapshot::Snapshot;
use crate::eligibility::EligibilityMask;

use super::heuristics::{LandmarkHeuristic, OctileCoords, octile};
use super::neighbors::NeighborProvider;

pub struct SearchParams<'a, C: OctileCoords> {
    pub start: u32,
    pub goal: u32,
    pub coords: Option<&'a C>,
    pub mask: Option<&'a EligibilityMask>,
    pub quick_tele: bool,
    /// Optional seed for path randomization. If Some, adds deterministic jitter to edge weights.
    pub seed: Option<u64>,
}

/// Simple hash function for deterministic jitter based on edge and seed
#[inline]
fn edge_jitter(seed: u64, from: u32, to: u32) -> f32 {
    // FNV-1a inspired hash combining seed, from, and to
    let mut h = seed;
    h ^= from as u64;
    h = h.wrapping_mul(0x100000001b3);
    h ^= to as u64;
    h = h.wrapping_mul(0x100000001b3);
    // Convert to small jitter in range [0, 0.1)
    ((h & 0xFFFF) as f32) / 655360.0
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub found: bool,
    pub path: Vec<u32>,
    pub cost: f32,
}

#[derive(Clone, Copy)]
pub struct Key { pub f: f32, pub g: f32, pub id: u32 }

impl PartialEq for Key { fn eq(&self, other: &Self) -> bool { self.f == other.f && self.g == other.g && self.id == other.id } }
impl Eq for Key {}
impl PartialOrd for Key { fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) } }
impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        let a = self.f.partial_cmp(&other.f).unwrap_or(Ordering::Equal).reverse();
        if a != Ordering::Equal { return a; }
        let b = self.g.partial_cmp(&other.g).unwrap_or(Ordering::Equal).reverse();
        if b != Ordering::Equal { return b; }
        self.id.cmp(&other.id).reverse()
    }
}

pub struct SearchContext {
    pub g: Vec<f32>,
    pub parent: Vec<u32>,
    pub in_open: Vec<bool>,
    pub visited_gen: Vec<u32>,
    pub generation: u32,
    pub open: BinaryHeap<Key>,
}

impl SearchContext {
    pub fn new(nodes: usize) -> Self {
        Self {
            g: vec![f32::INFINITY; nodes],
            parent: vec![u32::MAX; nodes],
            in_open: vec![false; nodes],
            visited_gen: vec![0; nodes],
            generation: 1,
            open: BinaryHeap::with_capacity(1024),
        }
    }

    pub fn reset(&mut self, nodes: usize) {
        if self.g.len() != nodes {
            *self = Self::new(nodes);
        } else {
            self.generation = self.generation.wrapping_add(1);
            if self.generation == 0 {
                 self.visited_gen.fill(0);
                 self.generation = 1;
            }
            self.open.clear();
        }
    }

    #[inline(always)]
    pub fn get_g(&self, u: usize) -> f32 {
        if self.visited_gen[u] == self.generation { self.g[u] } else { f32::INFINITY }
    }

    #[inline(always)]
    pub fn set_g(&mut self, u: usize, val: f32) {
        if self.visited_gen[u] != self.generation {
            self.visited_gen[u] = self.generation;
            self.in_open[u] = false;
        }
        self.g[u] = val;
    }

    #[inline(always)]
    pub fn set_parent(&mut self, u: usize, p: u32) {
        // Assume set_g was called first to init generation
        self.parent[u] = p;
    }

    #[inline(always)]
    pub fn get_parent(&self, u: usize) -> u32 {
        if self.visited_gen[u] == self.generation { self.parent[u] } else { u32::MAX }
    }
    
    #[inline(always)]
    pub fn is_in_open(&self, u: usize) -> bool {
        if self.visited_gen[u] == self.generation { self.in_open[u] } else { false }
    }

    #[inline(always)]
    pub fn set_in_open(&mut self, u: usize, val: bool) {
        // Assume set_g was called first
        self.in_open[u] = val;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::LeSliceF32;

    struct NoCoords;
    impl OctileCoords for NoCoords { fn coords(&self, _node: u32) -> (i32,i32,i32) { (0,0,0) } }

    #[test]
    fn astar_simple_line_prefers_walk_edges() {
        let nodes = 3usize;
        let walk_src = [0u32, 1u32];
        let walk_dst = [1u32, 2u32];
        let walk_w = [1.0f32, 1.0f32];
        let macro_src = [0u32];
        let macro_dst = [2u32];
        let macro_w = [3.5f32];
        let lm_fw = LeSliceF32 { bytes: &[] };
        let lm_bw = LeSliceF32 { bytes: &[] };
        let view = EngineView::from_parts(
            nodes,
            &walk_src, &walk_dst, &walk_w,
            &macro_src, &macro_dst, &macro_w,
            lm_fw, lm_bw, 0,
        );
        let mut ctx = SearchContext::new(nodes);
        let res = view.astar(SearchParams { start: 0, goal: 2, coords: Some(&NoCoords), mask: None, quick_tele: false, seed: None }, &mut ctx);
        assert!(res.found);
        assert_eq!(res.path, vec![0,1,2]);
        assert!((res.cost - 2.0).abs() < 1e-6);
    }
}

pub struct EngineView<'a> {
    pub nodes: usize,
    pub neighbors: Arc<NeighborProvider>,
    pub lm: LandmarkHeuristic<'a>,
    pub extra: Option<Box<dyn Fn(u32) -> Vec<(u32, f32)>>>,
}

impl<'a> EngineView<'a> {
    pub fn from_snapshot(s: &'a Snapshot) -> Self {
        let nodes = s.counts().nodes as usize;
        let walk_src: Vec<u32> = s.walk_src().iter().collect();
        let walk_dst: Vec<u32> = s.walk_dst().iter().collect();
        let walk_w: Vec<f32> = s.walk_w().iter().collect();
        let macro_src: Vec<u32> = s.macro_src().iter().collect();
        let macro_dst: Vec<u32> = s.macro_dst().iter().collect();
        let macro_w: Vec<f32> = s.macro_w().iter().collect();
        let neighbors = NeighborProvider::new(
            nodes,
            &walk_src, &walk_dst, &walk_w,
            &macro_src, &macro_dst, &macro_w,
        );
        let lm = LandmarkHeuristic { nodes, landmarks: s.counts().landmarks as usize, lm_fw: s.lm_fw(), lm_bw: s.lm_bw() };
        EngineView { nodes, neighbors: Arc::new(neighbors), lm, extra: None }
    }

    pub fn from_parts(nodes: usize, walk_src: &'a [u32], walk_dst: &'a [u32], walk_w: &'a [f32], macro_src: &'a [u32], macro_dst: &'a [u32], macro_w: &'a [f32], lm_fw: crate::snapshot::LeSliceF32<'a>, lm_bw: crate::snapshot::LeSliceF32<'a>, landmarks: usize) -> Self {
        let neighbors = NeighborProvider::new(nodes, walk_src, walk_dst, walk_w, macro_src, macro_dst, macro_w);
        let lm = LandmarkHeuristic { nodes, landmarks, lm_fw, lm_bw };
        EngineView { nodes, neighbors: Arc::new(neighbors), lm, extra: None }
    }

    pub fn astar<C: OctileCoords>(&self, params: SearchParams<C>, ctx: &mut SearchContext) -> SearchResult {
        let n = self.nodes;
        let start = params.start as usize;
        let goal = params.goal as usize;
        
        ctx.reset(n);
        ctx.set_g(start, 0.0);
        
        let h0 = self.lm.h(params.start, params.goal) + params.coords.map(|c| octile(c, params.start, params.goal)).unwrap_or(0.0);
        ctx.open.push(Key { f: h0, g: 0.0, id: params.start });
        ctx.set_in_open(start, true);

        while let Some(Key { f: _, g: gcur, id }) = ctx.open.pop() {
            let u = id as usize;
            if u == goal { break; }
            ctx.set_in_open(u, false);
            
            let mut main_iter = self.neighbors.all_neighbors(id, params.mask, params.quick_tele);
            let mut next_main = main_iter.next();
            
            let extra_vec = if let Some(cb) = &self.extra {
                let mut ex = cb(id);
                ex.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal)));
                Some(ex)
            } else {
                None
            };
            let mut extra_iter = extra_vec.as_ref().map(|v| v.iter());
            let mut next_extra = extra_iter.as_mut().and_then(|i| i.next());

            loop {
                let (v_id, w) = match (next_main, next_extra) {
                    (Some(m), Some(e)) => {
                        if m.0 <= e.0 {
                            next_main = main_iter.next();
                            m
                        } else {
                            next_extra = extra_iter.as_mut().unwrap().next();
                            *e
                        }
                    }
                    (Some(m), None) => {
                        next_main = main_iter.next();
                        m
                    }
                    (None, Some(e)) => {
                        next_extra = extra_iter.as_mut().unwrap().next();
                        *e
                    }
                    (None, None) => break,
                };

                let v = v_id as usize;
                // Add deterministic jitter if seed is provided
                let w_jittered = match params.seed {
                    Some(seed) => w + edge_jitter(seed, id, v_id),
                    None => w,
                };
                let ng = gcur + w_jittered;
                if ng < ctx.get_g(v) {
                    ctx.set_g(v, ng);
                    ctx.set_parent(v, id);
                    let h = self.lm.h(v_id, params.goal) + params.coords.map(|c| octile(c, v_id, params.goal)).unwrap_or(0.0);
                    let f = ng + h;
                    let key = Key { f, g: ng, id: v_id };
                    ctx.open.push(key);
                    ctx.set_in_open(v, true);
                }
            }
        }
        if ctx.get_g(goal) == f32::INFINITY {
            return SearchResult { found: false, path: Vec::new(), cost: f32::INFINITY };
        }
        let mut path = Vec::new();
        let mut cur = params.goal;
        while cur != u32::MAX && cur != params.start {
            path.push(cur);
            cur = ctx.get_parent(cur as usize);
        }
        path.push(params.start);
        path.reverse();
        SearchResult { found: true, path, cost: ctx.get_g(goal) }
    }
}
