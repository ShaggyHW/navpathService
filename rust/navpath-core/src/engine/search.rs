use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::snapshot::Snapshot;

use super::heuristics::{LandmarkHeuristic, OctileCoords, octile};
use super::neighbors::NeighborProvider;

pub struct SearchParams<'a, C: OctileCoords> {
    pub start: u32,
    pub goal: u32,
    pub coords: Option<&'a C>,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub found: bool,
    pub path: Vec<u32>,
    pub cost: f32,
}

#[derive(Clone, Copy)]
struct Key { f: f32, g: f32, id: u32 }

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
        let res = view.astar(SearchParams { start: 0, goal: 2, coords: Some(&NoCoords) });
        assert!(res.found);
        assert_eq!(res.path, vec![0,1,2]);
        assert!((res.cost - 2.0).abs() < 1e-6);
    }
}

pub struct EngineView<'a> {
    pub nodes: usize,
    pub neighbors: NeighborProvider,
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
        EngineView { nodes, neighbors, lm, extra: None }
    }

    pub fn from_parts(nodes: usize, walk_src: &'a [u32], walk_dst: &'a [u32], walk_w: &'a [f32], macro_src: &'a [u32], macro_dst: &'a [u32], macro_w: &'a [f32], lm_fw: crate::snapshot::LeSliceF32<'a>, lm_bw: crate::snapshot::LeSliceF32<'a>, landmarks: usize) -> Self {
        let neighbors = NeighborProvider::new(nodes, walk_src, walk_dst, walk_w, macro_src, macro_dst, macro_w);
        let lm = LandmarkHeuristic { nodes, landmarks, lm_fw, lm_bw };
        EngineView { nodes, neighbors, lm, extra: None }
    }

    pub fn astar<C: OctileCoords>(&self, params: SearchParams<C>) -> SearchResult {
        let n = self.nodes;
        let start = params.start as usize;
        let goal = params.goal as usize;
        let mut open = BinaryHeap::new();
        let mut in_open = vec![false; n];
        let mut g = vec![f32::INFINITY; n];
        let mut parent = vec![u32::MAX; n];
        g[start] = 0.0;
        let h0 = self.lm.h(params.start, params.goal) + params.coords.map(|c| octile(c, params.start, params.goal)).unwrap_or(0.0);
        open.push(Key { f: h0, g: 0.0, id: params.start });
        in_open[start] = true;
        while let Some(Key { f: _, g: gcur, id }) = open.pop() {
            let u = id as usize;
            if u == goal { break; }
            in_open[u] = false;
            let mut combined: Vec<(u32, f32)> = self.neighbors.all_neighbors(id).collect();
            if let Some(cb) = &self.extra {
                let mut extra = cb(id);
                extra.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal)));
                combined.extend_from_slice(&extra);
            }
            combined.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal)));
            for (v_id, w) in combined.into_iter() {
                let v = v_id as usize;
                let ng = gcur + w;
                if ng < g[v] {
                    g[v] = ng;
                    parent[v] = id;
                    let h = self.lm.h(v_id, params.goal) + params.coords.map(|c| octile(c, v_id, params.goal)).unwrap_or(0.0);
                    let f = ng + h;
                    let key = Key { f, g: ng, id: v_id };
                    open.push(key);
                    in_open[v] = true;
                }
            }
        }
        if g[goal] == f32::INFINITY {
            return SearchResult { found: false, path: Vec::new(), cost: f32::INFINITY };
        }
        let mut path = Vec::new();
        let mut cur = params.goal;
        while cur != u32::MAX && cur != params.start {
            path.push(cur);
            cur = parent[cur as usize];
        }
        path.push(params.start);
        path.reverse();
        SearchResult { found: true, path, cost: g[goal] }
    }
}
