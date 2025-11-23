use crate::eligibility::EligibilityMask;

pub struct Adjacency {
    pub nodes: usize,
    pub offsets: Vec<usize>,
    pub dst: Vec<u32>,
    pub w: Vec<f32>,
}

impl Adjacency {
    pub fn build(nodes: usize, src: &[u32], dst: &[u32], w: &[f32]) -> Self {
        Self::build_with_data::<()>(nodes, src, dst, w, None).0
    }

    pub fn build_with_data<T: Clone + Default>(nodes: usize, src: &[u32], dst: &[u32], w: &[f32], data: Option<&[T]>) -> (Self, Vec<T>) {
        let mut counts = vec![0usize; nodes];
        for &s in src { counts[s as usize] += 1; }
        let mut offsets = vec![0usize; nodes + 1];
        for i in 0..nodes { offsets[i + 1] = offsets[i] + counts[i]; }
        let mut cur = offsets[..nodes].to_vec();
        
        let len = dst.len();
        let mut adst = vec![0u32; len];
        let mut aw = vec![0f32; len];
        let mut adata = if data.is_some() { vec![T::default(); len] } else { Vec::new() };

        for i in 0..src.len() {
            let s = src[i] as usize;
            let p = cur[s];
            adst[p] = dst[i];
            aw[p] = w[i];
            if let Some(d) = data {
                adata[p] = d[i].clone();
            }
            cur[s] += 1;
        }
        
        for u in 0..nodes {
            let start = offsets[u];
            let end = offsets[u + 1];
            let slice = &mut adst[start..end];
            let ws = &mut aw[start..end];
            
            let mut idx: Vec<usize> = (0..slice.len()).collect();
            idx.sort_unstable_by(|&i, &j| {
                let a = slice[i];
                let b = slice[j];
                if a != b { return a.cmp(&b); }
                ws[i].partial_cmp(&ws[j]).unwrap_or(std::cmp::Ordering::Equal)
            });
            
            let mut nd = Vec::with_capacity(slice.len());
            let mut nw = Vec::with_capacity(ws.len());
            for &k in &idx { nd.push(slice[k]); nw.push(ws[k]); }
            slice.copy_from_slice(&nd);
            ws.copy_from_slice(&nw);
            
            if !adata.is_empty() {
                let ds = &mut adata[start..end];
                let mut n_data = Vec::with_capacity(ds.len());
                for &k in &idx { n_data.push(ds[k].clone()); }
                for (i, d) in n_data.into_iter().enumerate() {
                    ds[i] = d;
                }
            }
        }
        
        (Adjacency { nodes, offsets, dst: adst, w: aw }, adata)
    }

    pub fn neighbors(&self, u: u32) -> (&[u32], &[f32]) {
        let u = u as usize;
        if u >= self.nodes { return (&[], &[]); }
        let s = self.offsets[u];
        let e = self.offsets[u + 1];
        (&self.dst[s..e], &self.w[s..e])
    }
}

pub struct NeighborProvider {
    pub walk: Adjacency,
    pub macro_edges: Adjacency,
    pub macro_reqs: Vec<Vec<usize>>,
}

impl NeighborProvider {
    pub fn new(nodes: usize, walk_src: &[u32], walk_dst: &[u32], walk_w: &[f32], macro_src: &[u32], macro_dst: &[u32], macro_w: &[f32]) -> Self {
        let (walk, _) = Adjacency::build_with_data::<()>(nodes, walk_src, walk_dst, walk_w, None);
        let (macro_edges, _) = Adjacency::build_with_data::<()>(nodes, macro_src, macro_dst, macro_w, None);
        NeighborProvider { walk, macro_edges, macro_reqs: Vec::new() }
    }

    pub fn new_with_reqs(nodes: usize, walk_src: &[u32], walk_dst: &[u32], walk_w: &[f32], macro_src: &[u32], macro_dst: &[u32], macro_w: &[f32], macro_reqs: &[Vec<usize>]) -> Self {
        let (walk, _) = Adjacency::build_with_data::<()>(nodes, walk_src, walk_dst, walk_w, None);
        let (macro_edges, sorted_reqs) = Adjacency::build_with_data(nodes, macro_src, macro_dst, macro_w, Some(macro_reqs));
        NeighborProvider { walk, macro_edges, macro_reqs: sorted_reqs }
    }

    pub fn all_neighbors<'a>(&'a self, u: u32, mask: Option<&'a EligibilityMask>) -> impl Iterator<Item = (u32, f32)> + 'a {
        let (wd, ww) = self.walk.neighbors(u);
        let (md, mw) = self.macro_edges.neighbors(u);
        
        let reqs = if self.macro_reqs.is_empty() {
            None
        } else {
            let u = u as usize;
            if u < self.macro_edges.offsets.len() - 1 {
                 let s = self.macro_edges.offsets[u];
                 let e = self.macro_edges.offsets[u + 1];
                 Some(&self.macro_reqs[s..e])
            } else {
                None
            }
        };

        let walk_iter = wd.iter().copied().zip(ww.iter().copied());
        
        let macro_iter = md.iter().copied().zip(mw.iter().copied()).enumerate().filter_map(move |(i, (d, w))| {
            if let Some(m) = mask {
                if let Some(rs) = reqs {
                    if let Some(r) = rs.get(i) {
                        for &idx in r {
                            if !m.is_satisfied(idx) { return None; }
                        }
                    }
                }
            }
            Some((d, w))
        });

        MergeNeighbors {
            left: walk_iter,
            right: macro_iter,
            next_left: None,
            next_right: None,
        }
    }
}

pub struct MergeNeighbors<I1, I2> {
    left: I1,
    right: I2,
    next_left: Option<(u32, f32)>,
    next_right: Option<(u32, f32)>,
}

impl<I1, I2> Iterator for MergeNeighbors<I1, I2>
where I1: Iterator<Item = (u32, f32)>, I2: Iterator<Item = (u32, f32)>
{
    type Item = (u32, f32);
    fn next(&mut self) -> Option<Self::Item> {
        if self.next_left.is_none() { self.next_left = self.left.next(); }
        if self.next_right.is_none() { self.next_right = self.right.next(); }

        match (self.next_left, self.next_right) {
            (Some(l), Some(r)) => {
                if l.0 <= r.0 {
                    self.next_left = None;
                    Some(l)
                } else {
                    self.next_right = None;
                    Some(r)
                }
            }
            (Some(l), None) => {
                self.next_left = None;
                Some(l)
            }
            (None, Some(r)) => {
                self.next_right = None;
                Some(r)
            }
            (None, None) => None
        }
    }
}
