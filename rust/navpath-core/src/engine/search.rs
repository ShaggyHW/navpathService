use std::cmp::Ordering;
use std::sync::Arc;
use aligned_vec::AVec;

use crate::snapshot::Snapshot;
use crate::eligibility::EligibilityMask;

use super::heuristics::{NativeLandmarkHeuristic, OctileCoords, octile};
use super::neighbors::NeighborProvider;
use super::bucket_queue::BucketQueue;
use super::bucket_queue::Key;

pub struct SearchParams<'a, C: OctileCoords> {
    pub start: u32,
    pub goal: u32,
    pub coords: Option<&'a C>,
    pub mask: Option<&'a EligibilityMask>,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub found: bool,
    pub path: Vec<u32>,
    pub cost: f32,
}

pub struct SearchContext {
    pub g: AVec<f32>,
    pub parent: AVec<u32>,
    pub in_open: AVec<bool>,
    pub visited_gen: AVec<u32>,
    pub generation: u32,
    pub open: BucketQueue,
}

impl SearchContext {
    pub fn new(nodes: usize) -> Self {
        Self {
            g: AVec::__from_elem(64, f32::INFINITY, nodes),
            parent: AVec::__from_elem(64, u32::MAX, nodes),
            in_open: AVec::__from_elem(64, false, nodes),
            visited_gen: AVec::__from_elem(64, 0, nodes),
            generation: 1,
            open: BucketQueue::default(),
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
        let res = view.astar(SearchParams { start: 0, goal: 2, coords: Some(&NoCoords), mask: None }, &mut ctx);
        assert!(res.found);
        assert_eq!(res.path, vec![0,1,2]);
        assert!((res.cost - 2.0).abs() < 1e-6);
    }
}

pub struct EngineView {
    pub nodes: usize,
    pub neighbors: Arc<NeighborProvider>,
    pub lm: NativeLandmarkHeuristic,
    pub extra: Option<Box<dyn Fn(u32) -> Vec<(u32, f32)>>>,
}

impl EngineView {
    pub fn from_snapshot(s: &Snapshot) -> Self {
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
        let lm = NativeLandmarkHeuristic::from_le_slices(nodes, s.counts().landmarks as usize, s.lm_fw(), s.lm_bw());
        EngineView { nodes, neighbors: Arc::new(neighbors), lm, extra: None }
    }

    pub fn from_parts(nodes: usize, walk_src: &[u32], walk_dst: &[u32], walk_w: &[f32], macro_src: &[u32], macro_dst: &[u32], macro_w: &[f32], lm_fw: crate::snapshot::LeSliceF32, lm_bw: crate::snapshot::LeSliceF32, landmarks: usize) -> Self {
        let neighbors = NeighborProvider::new(nodes, walk_src, walk_dst, walk_w, macro_src, macro_dst, macro_w);
        let lm = NativeLandmarkHeuristic::from_le_slices(nodes, landmarks, lm_fw, lm_bw);
        EngineView { nodes, neighbors: Arc::new(neighbors), lm, extra: None }
    }

    pub fn astar<C: OctileCoords>(&self, params: SearchParams<C>, ctx: &mut SearchContext) -> SearchResult {
        let n = self.nodes;
        let start = params.start as usize;
        let goal = params.goal as usize;
        
        ctx.reset(n);
        ctx.set_g(start, 0.0);
        
        let h0 = if cfg!(feature = "simd") {
            self.lm.h_simd(params.start, params.goal)
        } else {
            self.lm.h(params.start, params.goal)
        } + params.coords.map(|c| octile(c, params.start, params.goal)).unwrap_or(0.0);
        ctx.open.push(Key::new(h0, 0.0, params.start));
        ctx.set_in_open(start, true);

        while let Some(key) = ctx.open.pop() {
            let Key { f: _, g: gcur, id } = key;
            let u = id as usize;
            if u == goal { break; }
            ctx.set_in_open(u, false);
            
            let mut main_iter = self.neighbors.all_neighbors(id, params.mask);
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
                let ng = gcur + w;
                if ng < ctx.get_g(v) {
                    ctx.set_g(v, ng);
                    ctx.set_parent(v, id);
                    let h = if cfg!(feature = "simd") {
                        self.lm.h_simd(v_id, params.goal)
                    } else {
                        self.lm.h(v_id, params.goal)
                    } + params.coords.map(|c| octile(c, v_id, params.goal)).unwrap_or(0.0);
                    let f = ng + h;
                    let key = Key::new(f, ng, v_id);
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
