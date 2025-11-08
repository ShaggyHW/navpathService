pub struct Adjacency {
    pub nodes: usize,
    pub offsets: Vec<usize>,
    pub dst: Vec<u32>,
    pub w: Vec<f32>,
}

impl Adjacency {
    pub fn build(nodes: usize, src: &[u32], dst: &[u32], w: &[f32]) -> Self {
        let mut counts = vec![0usize; nodes];
        for &s in src { counts[s as usize] += 1; }
        let mut offsets = vec![0usize; nodes + 1];
        for i in 0..nodes { offsets[i + 1] = offsets[i] + counts[i]; }
        let mut cur = offsets[..nodes].to_vec();
        let mut adst = vec![0u32; dst.len()];
        let mut aw = vec![0f32; w.len()];
        for i in 0..src.len() {
            let s = src[i] as usize;
            let p = cur[s];
            adst[p] = dst[i];
            aw[p] = w[i];
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
            for k in idx { nd.push(slice[k]); nw.push(ws[k]); }
            slice.copy_from_slice(&nd);
            ws.copy_from_slice(&nw);
        }
        Adjacency { nodes, offsets, dst: adst, w: aw }
    }

    pub fn neighbors(&self, u: u32) -> (&[u32], &[f32]) {
        let u = u as usize;
        let s = self.offsets[u];
        let e = self.offsets[u + 1];
        (&self.dst[s..e], &self.w[s..e])
    }
}

pub struct NeighborProvider {
    pub walk: Adjacency,
    pub macro_edges: Adjacency,
}

impl NeighborProvider {
    pub fn new(nodes: usize, walk_src: &[u32], walk_dst: &[u32], walk_w: &[f32], macro_src: &[u32], macro_dst: &[u32], macro_w: &[f32]) -> Self {
        let walk = Adjacency::build(nodes, walk_src, walk_dst, walk_w);
        let macro_edges = Adjacency::build(nodes, macro_src, macro_dst, macro_w);
        NeighborProvider { walk, macro_edges }
    }

    pub fn all_neighbors(&self, u: u32) -> impl Iterator<Item = (u32, f32)> + '_ {
        let (wd, ww) = self.walk.neighbors(u);
        let (md, mw) = self.macro_edges.neighbors(u);
        wd.iter().copied().zip(ww.iter().copied()).chain(md.iter().copied().zip(mw.iter().copied()))
    }
}
