use crate::eligibility::EligibilityMask;
use crate::snapshot::{walk_diagonal_ms, Snapshot, WALK_CARDINAL_MS};

/// Per-request macro-edge eligibility, folded once from the request's requirement mask
/// and quick-tele flag: the search then pays one bool index and one f32 load per macro
/// edge instead of walking each edge's requirement list on every expansion. Slots are in
/// macro-CSR order.
pub struct MacroFilter {
    pub allowed: Vec<bool>,
    pub w: Vec<f32>,
}

#[derive(Clone, Default)]
pub struct MacroEdgeData {
    pub reqs: Vec<usize>,
    pub kind_first: u32,
}

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
        // Counting-sort placement groups each node's neighbors contiguously. No per-node
        // ordering is imposed: A* relaxation is order-independent, and the input edge
        // order (builder emission order) keeps results deterministic.
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

/// The walk grid, either borrowed zero-copy from the snapshot's CSR sections (service
/// path) or owned (tests / ad-hoc graphs). Weights are derived: cardinal 300ms,
/// diagonal 300*sqrt(2), selected by the per-slot diagonal bitmap.
pub enum WalkGraph<'a> {
    Csr {
        offsets: &'a [u32],
        dst: &'a [u32],
        diag: &'a [u8],
    },
    Owned(Adjacency),
}

impl<'a> WalkGraph<'a> {
    pub fn from_snapshot(s: &'a Snapshot) -> Self {
        WalkGraph::Csr { offsets: s.walk_offsets(), dst: s.walk_dst(), diag: s.walk_diag() }
    }

    pub fn from_edges(nodes: usize, src: &[u32], dst: &[u32], w: &[f32]) -> Self {
        WalkGraph::Owned(Adjacency::build(nodes, src, dst, w))
    }

    #[inline]
    pub fn neighbors(&self, u: u32) -> WalkIter<'_> {
        match self {
            WalkGraph::Csr { offsets, dst, diag } => {
                let u = u as usize;
                if u + 1 >= offsets.len() {
                    return WalkIter::Owned { dst: &[], w: &[], i: 0 };
                }
                let (s, e) = (offsets[u] as usize, offsets[u + 1] as usize);
                WalkIter::Csr { dst: &dst[s..e], diag, base: s, i: 0, diag_w: walk_diagonal_ms() }
            }
            WalkGraph::Owned(adj) => {
                let (d, w) = adj.neighbors(u);
                WalkIter::Owned { dst: d, w, i: 0 }
            }
        }
    }
}

pub enum WalkIter<'a> {
    Csr { dst: &'a [u32], diag: &'a [u8], base: usize, i: usize, diag_w: f32 },
    Owned { dst: &'a [u32], w: &'a [f32], i: usize },
}

impl Iterator for WalkIter<'_> {
    type Item = (u32, f32);
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            WalkIter::Csr { dst, diag, base, i, diag_w } => {
                if *i >= dst.len() { return None; }
                let d = dst[*i];
                let slot = *base + *i;
                let is_diag = diag[slot / 8] & (1 << (slot % 8)) != 0;
                *i += 1;
                Some((d, if is_diag { *diag_w } else { WALK_CARDINAL_MS }))
            }
            WalkIter::Owned { dst, w, i } => {
                if *i >= dst.len() { return None; }
                let out = (dst[*i], w[*i]);
                *i += 1;
                Some(out)
            }
        }
    }
}

/// Macro-edge graph (doors/teleport chains) plus per-edge requirement data. The walk
/// grid lives in [`WalkGraph`]; this provider only carries the ~1k macro edges, so
/// building it at load is trivial.
pub struct NeighborProvider {
    pub macro_edges: Adjacency,
    pub macro_data: Vec<MacroEdgeData>,
}

impl NeighborProvider {
    pub fn new(nodes: usize, macro_src: &[u32], macro_dst: &[u32], macro_w: &[f32]) -> Self {
        let (macro_edges, _) = Adjacency::build_with_data::<()>(nodes, macro_src, macro_dst, macro_w, None);
        NeighborProvider { macro_edges, macro_data: Vec::new() }
    }

    pub fn new_with_reqs(nodes: usize, macro_src: &[u32], macro_dst: &[u32], macro_w: &[f32], macro_kind_first: &[u32], macro_reqs: &[Vec<usize>]) -> Self {
        let data: Vec<MacroEdgeData> = macro_reqs
            .iter()
            .zip(macro_kind_first.iter())
            .map(|(r, &k)| MacroEdgeData { reqs: r.clone(), kind_first: k })
            .collect();
        let (macro_edges, sorted_data) = Adjacency::build_with_data(nodes, macro_src, macro_dst, macro_w, Some(&data));
        NeighborProvider { macro_edges, macro_data: sorted_data }
    }

    /// Fold the request's eligibility mask and quick-tele flag into per-slot allow flags
    /// and effective weights. Called once per request, so the search's inner loop never
    /// touches requirement lists.
    pub fn macro_filter(&self, mask: &EligibilityMask, quick_tele: bool) -> MacroFilter {
        let n = self.macro_edges.dst.len();
        let mut allowed = vec![true; n];
        let mut w = self.macro_edges.w.clone();
        for (i, ed) in self.macro_data.iter().enumerate() {
            if ed.reqs.iter().any(|&idx| !mask.is_satisfied(idx)) {
                allowed[i] = false;
            }
            // Hardcoded: lodestone cost when hasQuickTele=1
            if quick_tele && ed.kind_first == 2 {
                w[i] = 2400.0;
            }
        }
        MacroFilter { allowed, w }
    }

    pub fn macro_neighbors<'a>(&'a self, u: u32, macro_filter: Option<&'a MacroFilter>) -> impl Iterator<Item = (u32, f32)> + 'a {
        let (md, _raw_w) = self.macro_edges.neighbors(u);
        let base = {
            let u = u as usize;
            if u < self.macro_edges.offsets.len() - 1 { self.macro_edges.offsets[u] } else { 0 }
        };
        let raw_w = &self.macro_edges.w;
        md.iter().copied().enumerate().filter_map(move |(i, d)| {
            let slot = base + i;
            match macro_filter {
                Some(f) => {
                    if f.allowed[slot] {
                        Some((d, f.w[slot]))
                    } else {
                        None
                    }
                }
                None => Some((d, raw_w[slot])),
            }
        })
    }
}
