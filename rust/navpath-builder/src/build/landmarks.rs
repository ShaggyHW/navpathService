use std::cmp::Ordering;
use std::collections::BinaryHeap;

use navpath_core::engine::heuristics::quantize_alt_ms;
use rayon::prelude::*;

/// Minimum component size (in nodes) for a component to receive landmarks. The largest
/// component is always eligible; smaller pockets fall back to a zero heuristic, which is
/// fine because searches inside them are tiny. 4096 = one full 64x64 region.
const MIN_LANDMARK_COMPONENT: usize = 4096;

/// Flat CSR adjacency for fast, shareable Dijkstra runs.
struct Csr {
    offsets: Vec<u32>,
    dst: Vec<u32>,
    w: Vec<f32>,
}

impl Csr {
    /// Build from parallel edge arrays; `reversed` swaps src/dst.
    fn build(
        nodes: usize,
        edge_lists: &[(&[u32], &[u32], &[f32])],
        reversed: bool,
    ) -> Csr {
        let mut counts = vec![0u32; nodes + 1];
        let mut total = 0usize;
        for (src, dst, _) in edge_lists {
            for i in 0..src.len() {
                let (s, d) = (src[i] as usize, dst[i] as usize);
                if s < nodes && d < nodes {
                    let key = if reversed { d } else { s };
                    counts[key + 1] += 1;
                    total += 1;
                }
            }
        }
        for i in 0..nodes {
            counts[i + 1] += counts[i];
        }
        let offsets = counts;
        let mut cur = offsets.clone();
        let mut adst = vec![0u32; total];
        let mut aw = vec![0f32; total];
        for (src, dst, w) in edge_lists {
            for i in 0..src.len() {
                let (s, d) = (src[i] as usize, dst[i] as usize);
                if s < nodes && d < nodes {
                    let (from, to) = if reversed { (d, s) } else { (s, d) };
                    let p = cur[from] as usize;
                    adst[p] = to as u32;
                    aw[p] = w[i];
                    cur[from] += 1;
                }
            }
        }
        Csr { offsets, dst: adst, w: aw }
    }

    #[inline]
    fn neighbors(&self, u: usize) -> (&[u32], &[f32]) {
        let s = self.offsets[u] as usize;
        let e = self.offsets[u + 1] as usize;
        (&self.dst[s..e], &self.w[s..e])
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct State { cost_bits: u32, node: u32 }

impl Ord for State {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse for min-heap behavior
        other.cost_bits.cmp(&self.cost_bits)
            .then_with(|| self.node.cmp(&other.node))
    }
}
impl PartialOrd for State {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

fn dijkstra_csr(csr: &Csr, nodes: usize, start: usize, dist: &mut Vec<f32>) {
    dist.clear();
    dist.resize(nodes, f32::INFINITY);
    if start >= nodes { return; }
    dist[start] = 0.0;
    let mut heap = BinaryHeap::with_capacity(1024);
    heap.push(State { cost_bits: 0.0f32.to_bits(), node: start as u32 });

    while let Some(State { cost_bits, node }) = heap.pop() {
        let u = node as usize;
        let cost = f32::from_bits(cost_bits);
        if cost > dist[u] { continue; }
        let (ds, ws) = csr.neighbors(u);
        for i in 0..ds.len() {
            let v = ds[i] as usize;
            let next = cost + ws[i];
            if next < dist[v] {
                dist[v] = next;
                heap.push(State { cost_bits: next.to_bits(), node: ds[i] });
            }
        }
    }
}

/// Undirected connected components over the same edge set the tables use, so landmark
/// placement never targets a pocket the search graph cannot reach.
fn component_sizes(nodes: usize, edge_lists: &[(&[u32], &[u32], &[f32])]) -> (Vec<u32>, Vec<usize>) {
    let mut parent: Vec<u32> = (0..nodes as u32).collect();
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            parent[x as usize] = parent[parent[x as usize] as usize];
            x = parent[x as usize];
        }
        x
    }
    for (src, dst, _) in edge_lists {
        for i in 0..src.len() {
            let (s, d) = (src[i], dst[i]);
            if (s as usize) < nodes && (d as usize) < nodes {
                let (rs, rd) = (find(&mut parent, s), find(&mut parent, d));
                if rs != rd { parent[rs as usize] = rd; }
            }
        }
    }
    let mut sizes = vec![0usize; nodes];
    let mut roots = vec![0u32; nodes];
    for v in 0..nodes as u32 {
        let r = find(&mut parent, v);
        roots[v as usize] = r;
        sizes[r as usize] += 1;
    }
    (roots, sizes)
}

/// Compact per-node component ids over the WALK graph only (undirected), for the v8
/// snapshot's reachability-precheck section. Returns (comp_id per node, component count).
pub fn walk_component_ids(nodes: usize, walk_src: &[u32], walk_dst: &[u32]) -> (Vec<u16>, u32) {
    let mut parent: Vec<u32> = (0..nodes as u32).collect();
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            parent[x as usize] = parent[parent[x as usize] as usize];
            x = parent[x as usize];
        }
        x
    }
    for i in 0..walk_src.len() {
        let (s, d) = (walk_src[i], walk_dst[i]);
        if (s as usize) < nodes && (d as usize) < nodes {
            let (rs, rd) = (find(&mut parent, s), find(&mut parent, d));
            if rs != rd { parent[rs as usize] = rd; }
        }
    }
    let mut compact: std::collections::HashMap<u32, u16> = std::collections::HashMap::new();
    let mut ids = vec![0u16; nodes];
    for v in 0..nodes as u32 {
        let r = find(&mut parent, v);
        let next = compact.len() as u16;
        let id = *compact.entry(r).or_insert(next);
        ids[v as usize] = id;
    }
    (ids, compact.len() as u32)
}

/// Select `count` landmarks by farthest-point sampling and compute the node-major ALT
/// tables in one pass, reusing each landmark's selection Dijkstra as its forward column.
///
/// Selection: start from the lowest-id node of the largest component, repeatedly take the
/// node maximizing min-distance to the chosen set. Unreached-but-eligible nodes (other
/// large components) count as infinitely far, so every component above
/// [`MIN_LANDMARK_COMPONENT`] receives a landmark before spreading continues — this
/// replaces the old `0..N` selection whose 64 landmarks all sat in one map corner.
///
/// Returns `(landmark_ids, lm_tab)` where `lm_tab` is the snapshot's interleaved
/// quantized layout `[node][landmark][fw, bw]` (u16 quanta), produced directly by the
/// transpose. The full-size f32 node-major intermediates — the single largest builder
/// allocation (2 x nodes x count x 4 B, ~2 GB at 4M/64) — and the old serial
/// re-quantize pass in main.rs no longer exist (roadmap 7.1).
pub fn select_and_compute_alt(
    nodes: usize,
    walk_src: &[u32],
    walk_dst: &[u32],
    walk_w: &[f32],
    macro_src: &[u32],
    macro_dst: &[u32],
    macro_w: &[f32],
    count: u32,
) -> (Vec<u32>, Vec<u16>) {
    if count == 0 || nodes == 0 {
        return (Vec::new(), Vec::new());
    }
    let edge_lists: [(&[u32], &[u32], &[f32]); 2] = [
        (walk_src, walk_dst, walk_w),
        (macro_src, macro_dst, macro_w),
    ];

    let fwd = Csr::build(nodes, &edge_lists, false);
    let rev = Csr::build(nodes, &edge_lists, true);

    let (roots, sizes) = component_sizes(nodes, &edge_lists);
    let largest = sizes.iter().copied().max().unwrap_or(0);
    let threshold = MIN_LANDMARK_COMPONENT.min(largest.max(1));
    let eligible: Vec<bool> = roots
        .iter()
        .map(|&r| sizes[r as usize] >= threshold)
        .collect();

    let eligible_count = eligible.iter().filter(|&&e| e).count();
    let k = (count as usize).min(eligible_count);
    if k == 0 {
        return (Vec::new(), Vec::new());
    }

    // Deterministic seed: lowest-id node of the largest component. The seed itself is not
    // a landmark; the first landmark is the farthest eligible node from it.
    let largest_root = (0..nodes)
        .max_by_key(|&v| (sizes[roots[v] as usize], std::cmp::Reverse(v)))
        .map(|v| roots[v])
        .unwrap();
    let seed = (0..nodes).find(|&v| roots[v] == largest_root).unwrap();

    let mut min_dist = Vec::new();
    dijkstra_csr(&fwd, nodes, seed, &mut min_dist);

    let mut landmarks: Vec<u32> = Vec::with_capacity(k);
    let mut is_landmark = vec![false; nodes];
    // Forward columns captured during selection: fwd_cols[i][n] = d(L_i, n).
    let mut fwd_cols: Vec<Vec<f32>> = Vec::with_capacity(k);

    for _ in 0..k {
        // argmax of min-dist over eligible non-landmark nodes; INFINITY (uncovered
        // component) wins over any finite distance, ties break to the lowest id.
        let mut best: Option<(f32, usize)> = None;
        for v in 0..nodes {
            if !eligible[v] || is_landmark[v] {
                continue;
            }
            let d = min_dist[v];
            match best {
                None => best = Some((d, v)),
                Some((bd, _)) => {
                    if d > bd {
                        best = Some((d, v));
                    }
                }
            }
        }
        let Some((_, lm)) = best else { break };
        is_landmark[lm] = true;
        landmarks.push(lm as u32);

        let mut col = Vec::new();
        dijkstra_csr(&fwd, nodes, lm, &mut col);
        if fwd_cols.is_empty() {
            // The seed only bootstraps the first pick and is not itself a landmark, so
            // coverage restarts from the first landmark's distances alone.
            min_dist.copy_from_slice(&col);
        } else {
            for v in 0..nodes {
                if col[v] < min_dist[v] {
                    min_dist[v] = col[v];
                }
            }
        }
        fwd_cols.push(col);
    }

    let lm_count = landmarks.len();

    // Backward columns are independent — run them in parallel against the shared CSR.
    let bwd_cols: Vec<Vec<f32>> = landmarks
        .par_iter()
        .map(|&lm| {
            let mut col = Vec::new();
            dijkstra_csr(&rev, nodes, lm as usize, &mut col);
            col
        })
        .collect();

    // Blocked transpose STRAIGHT into the interleaved quantized snapshot layout.
    // Chunking by node block keeps each output region cache-resident instead of doing
    // one full-table strided pass per landmark; quantization happens at store time, so
    // the values are bit-identical to the old transpose-then-quantize pipeline.
    let mut lm_tab = vec![0u16; nodes * lm_count * 2];
    const BLOCK: usize = 8192;
    lm_tab
        .par_chunks_mut(BLOCK * lm_count * 2)
        .enumerate()
        .for_each(|(chunk_idx, chunk)| {
            let base = chunk_idx * BLOCK;
            let block_nodes = chunk.len() / (lm_count * 2);
            for li in 0..lm_count {
                let fcol = &fwd_cols[li];
                let bcol = &bwd_cols[li];
                for n in 0..block_nodes {
                    chunk[(n * lm_count + li) * 2] = quantize_alt_ms(fcol[base + n]);
                    chunk[(n * lm_count + li) * 2 + 1] = quantize_alt_ms(bcol[base + n]);
                }
            }
        });

    (landmarks, lm_tab)
}

/// Compute ALT tables for a fixed landmark set (node-major layout). Kept for callers and
/// tests that already have landmark ids; the builder's main path uses
/// [`select_and_compute_alt`] which also picks the landmarks.
pub fn compute_alt_tables(
    nodes: usize,
    walk_src: &[u32],
    walk_dst: &[u32],
    walk_w: &[f32],
    macro_src: &[u32],
    macro_dst: &[u32],
    macro_w: &[f32],
    landmarks: &[u32],
) -> (Vec<f32>, Vec<f32>) {
    if landmarks.is_empty() || nodes == 0 {
        return (Vec::new(), Vec::new());
    }
    let edge_lists: [(&[u32], &[u32], &[f32]); 2] = [
        (walk_src, walk_dst, walk_w),
        (macro_src, macro_dst, macro_w),
    ];
    let fwd = Csr::build(nodes, &edge_lists, false);
    let rev = Csr::build(nodes, &edge_lists, true);

    let lm_count = landmarks.len();
    let cols: Vec<(Vec<f32>, Vec<f32>)> = landmarks
        .par_iter()
        .map(|&lm| {
            let mut f = Vec::new();
            let mut b = Vec::new();
            dijkstra_csr(&fwd, nodes, lm as usize, &mut f);
            dijkstra_csr(&rev, nodes, lm as usize, &mut b);
            (f, b)
        })
        .collect();

    let mut lm_fw = vec![f32::INFINITY; nodes * lm_count];
    let mut lm_bw = vec![f32::INFINITY; nodes * lm_count];
    const BLOCK: usize = 8192;
    lm_fw
        .par_chunks_mut(BLOCK * lm_count)
        .zip(lm_bw.par_chunks_mut(BLOCK * lm_count))
        .enumerate()
        .for_each(|(chunk_idx, (fw_chunk, bw_chunk))| {
            let base = chunk_idx * BLOCK;
            let block_nodes = fw_chunk.len() / lm_count;
            for li in 0..lm_count {
                let (fcol, bcol) = &cols[li];
                for n in 0..block_nodes {
                    fw_chunk[n * lm_count + li] = fcol[base + n];
                    bw_chunk[n * lm_count + li] = bcol[base + n];
                }
            }
        });

    (lm_fw, lm_bw)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 0 -1- 1 -1- 2 -1- 3 (bidirectional line), plus isolated pair 4-5.
    fn line_edges() -> (Vec<u32>, Vec<u32>, Vec<f32>) {
        let mut src = Vec::new();
        let mut dst = Vec::new();
        let mut w = Vec::new();
        for (a, b) in [(0u32, 1u32), (1, 2), (2, 3), (4, 5)] {
            src.push(a); dst.push(b); w.push(1.0);
            src.push(b); dst.push(a); w.push(1.0);
        }
        (src, dst, w)
    }

    #[test]
    fn compute_alt_tables_node_major() {
        let (src, dst, w) = line_edges();
        let landmarks = [0u32, 3u32];
        let (fw, bw) = compute_alt_tables(6, &src, &dst, &w, &[], &[], &[], &landmarks);
        assert_eq!(fw.len(), 6 * 2);
        // node-major: node n row = [d(L0,n), d(L1,n)]
        assert_eq!(fw[0 * 2 + 0], 0.0); // d(0,0)
        assert_eq!(fw[3 * 2 + 0], 3.0); // d(0,3)
        assert_eq!(fw[0 * 2 + 1], 3.0); // d(3,0)
        assert!(fw[4 * 2 + 0].is_infinite()); // island unreachable
        // symmetric graph: bw == fw
        assert_eq!(bw[3 * 2 + 0], 3.0);
    }

    #[test]
    fn farthest_point_spreads_landmarks() {
        let (src, dst, w) = line_edges();
        // Weights scaled so quantized (64 ms) values stay discriminative.
        let w: Vec<f32> = w.iter().map(|x| x * 6400.0).collect();
        // Only the 4-node line is above the (clamped) threshold; ask for 2 landmarks.
        let (lms, tab) = select_and_compute_alt(6, &src, &dst, &w, &[], &[], &[], 2);
        assert_eq!(lms.len(), 2);
        // Farthest-point on a line must pick the two endpoints (0 and 3), in some order.
        let mut sorted = lms.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 3]);
        // Interleaved [node][landmark][fw, bw], u16 quanta of 64 ms: one hop = 100 quanta.
        assert_eq!(tab.len(), 6 * 2 * 2);
        let li = |lm: u32| lms.iter().position(|&x| x == lm).unwrap();
        assert_eq!(tab[(0 * 2 + li(0)) * 2], 0); // d(0,0) fw
        assert_eq!(tab[(3 * 2 + li(0)) * 2], 300); // d(0,3) = 3 hops = 19200 ms
        assert_eq!(tab[(1 * 2 + li(0)) * 2], 100); // d(0,1)
        // symmetric graph: bw == fw
        assert_eq!(tab[(3 * 2 + li(0)) * 2 + 1], 300);
        // island unreachable
        assert_eq!(tab[(4 * 2 + li(0)) * 2], navpath_core::snapshot::ALT_UNREACHABLE);
    }
}
