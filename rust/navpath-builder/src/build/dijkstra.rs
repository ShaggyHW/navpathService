//! The ALT table graph and its single-source shortest-path engine.
//!
//! The table graph is the SUPERSET of every edge the query engine can relax from a
//! non-origin node: the symmetric walk CSR, every macro edge (lodestone-first ones
//! floored at the quick-tele cost), and the full fairy-ring clique. See `main.rs` for
//! why each piece is there.
//!
//! # Exactness of the distances
//!
//! Dijkstra with non-negative f32 weights computes, for every node, the least fixed
//! point of `D(v) = min over in-edges (u, v, w) of fl(D(u) + w)` with `D(start) = 0`
//! (fl = round-to-nearest f32 addition). `fl(a + w)` is monotone in `a` and
//! `fl(a + w) >= a` for `w >= 0`, so every label-setting run settles nodes in
//! non-decreasing order with their final value, and any two such runs agree on every
//! node: take a node where they differ with the smallest value, follow the first run's
//! parent chain to the first strictly smaller ancestor (or the start) — the second run
//! agrees there, and monotonicity of fl carries that value down the chain. Hence the
//! values do not depend on heap tie-breaking, CSR edge order, or which heap is used:
//! the radix heap below produces the same f32 bits as the old `BinaryHeap` runs.

use navpath_core::snapshot::{walk_diagonal_ms, WALK_CARDINAL_MS};

/// Search direction over the table graph. The walk part is symmetric (the builder
/// bails before this stage otherwise) with direction-determined weights, so the reverse
/// walk graph IS the forward one; only the sparse extra edges differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Distances FROM the source: d(s, v).
    Fwd,
    /// Distances TO the source: d(v, s), i.e. a search over reversed edges.
    Rev,
}

/// Sparse adjacency for the few thousand extra (macro + fairy) edges, keyed by source.
/// A bitmap answers "does u have extra edges" on every pop without touching an
/// n-sized offsets array.
pub struct ExtraAdj {
    has: Vec<u64>,
    src: Vec<u32>,
    off: Vec<u32>,
    dst: Vec<u32>,
    w: Vec<f32>,
}

impl ExtraAdj {
    /// `edges` are (from, to, w) in the direction this adjacency will be walked.
    fn build(n: usize, mut edges: Vec<(u32, u32, f32)>) -> Self {
        edges.sort_by_key(|e| e.0);
        let mut has = vec![0u64; n.div_ceil(64)];
        let mut src = Vec::new();
        let mut off = Vec::new();
        let mut dst = Vec::with_capacity(edges.len());
        let mut w = Vec::with_capacity(edges.len());
        for (i, &(s, d, ew)) in edges.iter().enumerate() {
            if src.last() != Some(&s) {
                src.push(s);
                off.push(i as u32);
                has[s as usize / 64] |= 1 << (s % 64);
            }
            dst.push(d);
            w.push(ew);
        }
        off.push(edges.len() as u32);
        ExtraAdj { has, src, off, dst, w }
    }

    #[inline(always)]
    fn neighbors(&self, u: u32) -> Option<(&[u32], &[f32])> {
        if (self.has[u as usize / 64] >> (u % 64)) & 1 == 0 {
            return None;
        }
        let i = self.src.binary_search(&u).ok()?;
        let (a, b) = (self.off[i] as usize, self.off[i + 1] as usize);
        Some((&self.dst[a..b], &self.w[a..b]))
    }

    /// All (from, to) pairs, in adjacency order.
    pub fn pairs(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.src.iter().enumerate().flat_map(move |(i, &s)| {
            let (a, b) = (self.off[i] as usize, self.off[i + 1] as usize);
            self.dst[a..b].iter().map(move |&d| (s, d))
        })
    }
}

/// The ALT table graph: the snapshot's walk CSR (borrowed, weights implied by the
/// diagonal bitmap) plus the extra edges in both orientations.
pub struct AltGraph<'a> {
    pub n: usize,
    pub walk_off: &'a [u32],
    pub walk_dst: &'a [u32],
    pub walk_diag: &'a [u8],
    w_card: f32,
    w_diag: f32,
    pub fwd: ExtraAdj,
    pub rev: ExtraAdj,
}

impl<'a> AltGraph<'a> {
    /// Extra edges with an endpoint outside `0..n` are dropped, as the old CSR build did.
    pub fn new(
        n: usize,
        walk_off: &'a [u32],
        walk_dst: &'a [u32],
        walk_diag: &'a [u8],
        extra_src: &[u32],
        extra_dst: &[u32],
        extra_w: &[f32],
    ) -> Self {
        assert_eq!(walk_off.len(), n + 1);
        let mut f = Vec::with_capacity(extra_src.len());
        let mut r = Vec::with_capacity(extra_src.len());
        for i in 0..extra_src.len() {
            let (s, d, w) = (extra_src[i], extra_dst[i], extra_w[i]);
            if (s as usize) < n && (d as usize) < n {
                debug_assert!(w >= 0.0, "negative table-graph weight");
                f.push((s, d, w));
                r.push((d, s, w));
            }
        }
        AltGraph {
            n,
            walk_off,
            walk_dst,
            walk_diag,
            w_card: WALK_CARDINAL_MS,
            w_diag: walk_diagonal_ms(),
            fwd: ExtraAdj::build(n, f),
            rev: ExtraAdj::build(n, r),
        }
    }

    #[inline(always)]
    pub fn extra(&self, dir: Dir) -> &ExtraAdj {
        match dir {
            Dir::Fwd => &self.fwd,
            Dir::Rev => &self.rev,
        }
    }

    /// Walk neighbours of `u` (symmetric: successors == predecessors).
    #[inline(always)]
    pub fn walk_neighbors(&self, u: usize) -> &[u32] {
        &self.walk_dst[self.walk_off[u] as usize..self.walk_off[u + 1] as usize]
    }
}

/// Monotone radix heap over u32 keys (f32 bits of non-negative distances, whose unsigned
/// order equals their numeric order). Valid for Dijkstra because every pushed key is
/// >= the last popped one. Amortized O(log C) with tiny constants and no sift traffic.
pub struct RadixHeap {
    last: u32,
    len: usize,
    buckets: [Vec<(u32, u32)>; 33],
}

impl Default for RadixHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl RadixHeap {
    pub fn new() -> Self {
        RadixHeap { last: 0, len: 0, buckets: std::array::from_fn(|_| Vec::new()) }
    }

    pub fn clear(&mut self) {
        self.last = 0;
        self.len = 0;
        for b in self.buckets.iter_mut() {
            b.clear();
        }
    }

    #[inline(always)]
    fn bucket_of(k: u32, last: u32) -> usize {
        (32 - (k ^ last).leading_zeros()) as usize
    }

    #[inline(always)]
    pub fn push(&mut self, k: u32, v: u32) {
        debug_assert!(k >= self.last, "radix heap: non-monotone push");
        self.buckets[Self::bucket_of(k, self.last)].push((k, v));
        self.len += 1;
    }

    #[inline(always)]
    pub fn pop(&mut self) -> Option<(u32, u32)> {
        if self.buckets[0].is_empty() {
            if self.len == 0 {
                return None;
            }
            let mut i = 1;
            while self.buckets[i].is_empty() {
                i += 1;
            }
            let mut b = std::mem::take(&mut self.buckets[i]);
            let new_last = b.iter().map(|e| e.0).min().unwrap();
            self.last = new_last;
            // Every entry shares new_last's bits above bit i-1, so each lands strictly
            // below bucket i.
            for &(k, v) in &b {
                self.buckets[Self::bucket_of(k, new_last)].push((k, v));
            }
            b.clear();
            self.buckets[i] = b;
        }
        self.len -= 1;
        self.buckets[0].pop()
    }
}

/// Dijkstra from `start` over the table graph in direction `dir`.
///
/// `dist` holds the initial labels and receives the result. Relaxation only lowers a
/// label (`next < dist[v]`), so:
///  - with `dist` all-INFINITY this is a plain full Dijkstra;
///  - with `dist` = the pointwise min of earlier full runs' results (an upper envelope
///    that itself satisfies `dist[v] <= fl(dist[u] + w)` on every edge) it is the
///    PRUNED update used by farthest-point selection: afterwards
///    `dist = min(old, D_start)` exactly, having expanded only the nodes the new source
///    improves (its Voronoi cell). Proof: if `D(v) < old(v)`, every node p on v's
///    shortest-path chain has `D(p) < old(p)` — otherwise
///    `old(v) <= fl(... fl(old(p) + w) ...) <= D(v)` by monotonicity of fl — so the
///    chain is expanded with exact labels and v gets `D(v)`; conversely every label is
///    a real path length from `start` or an old label.
///
/// Returns the number of settled nodes (expansions).
pub fn dijkstra(g: &AltGraph, dir: Dir, start: usize, dist: &mut [f32], heap: &mut RadixHeap) -> usize {
    if start >= g.n {
        heap.clear();
        return 0;
    }
    dijkstra_multi(g, dir, &[start as u32], dist, heap)
}

/// [`dijkstra`] from a set of sources, all at distance 0 (distance from/to the SET).
pub fn dijkstra_multi(g: &AltGraph, dir: Dir, starts: &[u32], dist: &mut [f32], heap: &mut RadixHeap) -> usize {
    debug_assert_eq!(dist.len(), g.n);
    heap.clear();
    for &s in starts {
        let s = s as usize;
        if s >= g.n {
            continue;
        }
        if 0.0 < dist[s] {
            dist[s] = 0.0;
        }
        heap.push(0.0f32.to_bits(), s as u32);
    }
    let extra = g.extra(dir);
    let (w_card, w_diag) = (g.w_card, g.w_diag);
    let mut settled = 0usize;
    while let Some((k, uid)) = heap.pop() {
        let u = uid as usize;
        let cost = f32::from_bits(k);
        if cost > dist[u] {
            continue;
        }
        settled += 1;
        let (s, e) = (g.walk_off[u] as usize, g.walk_off[u + 1] as usize);
        for slot in s..e {
            let v = g.walk_dst[slot] as usize;
            let w = if (g.walk_diag[slot >> 3] >> (slot & 7)) & 1 != 0 { w_diag } else { w_card };
            let next = cost + w;
            if next < dist[v] {
                dist[v] = next;
                heap.push(next.to_bits(), v as u32);
            }
        }
        if let Some((ds, ws)) = extra.neighbors(uid) {
            for i in 0..ds.len() {
                let v = ds[i] as usize;
                let next = cost + ws[i];
                if next < dist[v] {
                    dist[v] = next;
                    // `next` is never -0.0 (sums start at +0.0), so its bits order
                    // exactly like its value.
                    heap.push(next.to_bits(), v as u32);
                }
            }
        }
    }
    settled
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;
    use std::collections::BinaryHeap;

    #[derive(Clone, Copy, Eq, PartialEq)]
    struct State {
        cost_bits: u32,
        node: u32,
    }
    impl Ord for State {
        fn cmp(&self, other: &Self) -> Ordering {
            other.cost_bits.cmp(&self.cost_bits).then_with(|| self.node.cmp(&other.node))
        }
    }
    impl PartialOrd for State {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    /// The pre-T5.3 BinaryHeap Dijkstra over an explicit (src, dst, w) edge list.
    fn reference(n: usize, edges: &[(u32, u32, f32)], start: usize) -> Vec<f32> {
        let mut adj: Vec<Vec<(u32, f32)>> = vec![Vec::new(); n];
        for &(s, d, w) in edges {
            adj[s as usize].push((d, w));
        }
        let mut dist = vec![f32::INFINITY; n];
        dist[start] = 0.0;
        let mut heap = BinaryHeap::new();
        heap.push(State { cost_bits: 0, node: start as u32 });
        while let Some(State { cost_bits, node }) = heap.pop() {
            let u = node as usize;
            let cost = f32::from_bits(cost_bits);
            if cost > dist[u] {
                continue;
            }
            for &(v, w) in &adj[u] {
                let next = cost + w;
                if next < dist[v as usize] {
                    dist[v as usize] = next;
                    heap.push(State { cost_bits: next.to_bits(), node: v });
                }
            }
        }
        dist
    }

    fn lcg(state: &mut u64) -> u64 {
        *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *state >> 33
    }

    /// Random grid-like symmetric walk CSR (+diagonals) plus random one-way extras with
    /// awkward weights (zeros, non-representable decimals, large values).
    fn random_graph(seed: u64, side: usize) -> (usize, Vec<u32>, Vec<u32>, Vec<u8>, Vec<(u32, u32, f32)>) {
        let mut st = seed;
        let n = side * side;
        let mut rows: Vec<Vec<(u32, bool)>> = vec![Vec::new(); n];
        for y in 0..side {
            for x in 0..side {
                let u = y * side + x;
                for (dx, dy, diag) in [(1i64, 0i64, false), (0, 1, false), (1, 1, true), (-1, 1, true)] {
                    let (nx, ny) = (x as i64 + dx, y as i64 + dy);
                    if nx < 0 || ny < 0 || nx >= side as i64 || ny >= side as i64 {
                        continue;
                    }
                    if lcg(&mut st) % 10 < 7 {
                        let v = ny as usize * side + nx as usize;
                        rows[u].push((v as u32, diag));
                        rows[v].push((u as u32, diag));
                    }
                }
            }
        }
        let mut off = vec![0u32];
        let mut dst = Vec::new();
        let mut diag_bits = Vec::new();
        for r in &mut rows {
            r.sort();
            for &(v, d) in r.iter() {
                dst.push(v);
                diag_bits.push(d);
            }
            off.push(dst.len() as u32);
        }
        let mut diag = vec![0u8; dst.len().div_ceil(8)];
        for (i, &d) in diag_bits.iter().enumerate() {
            if d {
                diag[i / 8] |= 1 << (i % 8);
            }
        }
        let mut extra = Vec::new();
        for _ in 0..(n / 8) {
            let s = (lcg(&mut st) as usize % n) as u32;
            let d = (lcg(&mut st) as usize % n) as u32;
            let w = match lcg(&mut st) % 4 {
                0 => 0.0,
                1 => 1234.567,
                2 => 0.1 * (lcg(&mut st) % 1000) as f32,
                _ => 2400.0,
            };
            extra.push((s, d, w));
        }
        (n, off, dst, diag, extra)
    }

    #[test]
    fn radix_dijkstra_is_bit_identical_to_binary_heap() {
        for seed in 1..6u64 {
            let (n, off, dst, diag, extra) = random_graph(seed, 30);
            let es: Vec<u32> = extra.iter().map(|e| e.0).collect();
            let ed: Vec<u32> = extra.iter().map(|e| e.1).collect();
            let ew: Vec<f32> = extra.iter().map(|e| e.2).collect();
            let g = AltGraph::new(n, &off, &dst, &diag, &es, &ed, &ew);
            // Reference edge list: walk (implied weights) + extras, per direction.
            let mut fwd_edges = Vec::new();
            for u in 0..n {
                for slot in off[u] as usize..off[u + 1] as usize {
                    let d = (diag[slot / 8] >> (slot % 8)) & 1 == 1;
                    let w = if d { walk_diagonal_ms() } else { WALK_CARDINAL_MS };
                    fwd_edges.push((u as u32, dst[slot], w));
                }
            }
            let mut rev_edges: Vec<(u32, u32, f32)> = fwd_edges.iter().map(|&(s, d, w)| (d, s, w)).collect();
            fwd_edges.extend(extra.iter().copied());
            rev_edges.extend(extra.iter().map(|&(s, d, w)| (d, s, w)));
            let mut heap = RadixHeap::new();
            let mut dist = vec![f32::INFINITY; n];
            for start in [0usize, n / 2, n - 1, 17] {
                for (dir, edges) in [(Dir::Fwd, &fwd_edges), (Dir::Rev, &rev_edges)] {
                    dist.fill(f32::INFINITY);
                    dijkstra(&g, dir, start, &mut dist, &mut heap);
                    let r = reference(n, edges, start);
                    for v in 0..n {
                        assert_eq!(dist[v].to_bits(), r[v].to_bits(), "seed {seed} start {start} {dir:?} node {v}");
                    }
                }
            }
        }
    }

    #[test]
    fn pruned_update_equals_pointwise_min() {
        let (n, off, dst, diag, extra) = random_graph(9, 30);
        let es: Vec<u32> = extra.iter().map(|e| e.0).collect();
        let ed: Vec<u32> = extra.iter().map(|e| e.1).collect();
        let ew: Vec<f32> = extra.iter().map(|e| e.2).collect();
        let g = AltGraph::new(n, &off, &dst, &diag, &es, &ed, &ew);
        let mut heap = RadixHeap::new();
        let sources = [3usize, 400, 899, 250, 612];
        let mut envelope = vec![f32::INFINITY; n];
        let mut expected = vec![f32::INFINITY; n];
        let mut full = vec![f32::INFINITY; n];
        for &s in &sources {
            full.fill(f32::INFINITY);
            dijkstra(&g, Dir::Fwd, s, &mut full, &mut heap);
            for v in 0..n {
                expected[v] = expected[v].min(full[v]);
            }
            let settled = dijkstra(&g, Dir::Fwd, s, &mut envelope, &mut heap);
            assert!(settled <= n);
            for v in 0..n {
                assert_eq!(envelope[v].to_bits(), expected[v].to_bits(), "source {s} node {v}");
            }
        }
    }
}
