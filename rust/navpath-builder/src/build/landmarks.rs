use std::cmp::Ordering;
use std::collections::BinaryHeap;

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

    // Build forward and reverse adjacency lists
    let mut adj_fwd: Vec<Vec<(u32, f32)>> = vec![Vec::new(); nodes];
    let mut adj_rev: Vec<Vec<(u32, f32)>> = vec![Vec::new(); nodes];

    for i in 0..walk_src.len() {
        let s = walk_src[i] as usize;
        let d = walk_dst[i] as usize;
        let w = walk_w[i];
        if s < nodes && d < nodes {
            adj_fwd[s].push((d as u32, w));
            adj_rev[d].push((s as u32, w));
        }
    }
    for i in 0..macro_src.len() {
        let s = macro_src[i] as usize;
        let d = macro_dst[i] as usize;
        let w = macro_w[i];
        if s < nodes && d < nodes {
            adj_fwd[s].push((d as u32, w));
            adj_rev[d].push((s as u32, w));
        }
    }

    let lm_count = landmarks.len();
    let mut lm_fw = vec![f32::INFINITY; nodes * lm_count];
    let mut lm_bw = vec![f32::INFINITY; nodes * lm_count];

    for (li, &lmid) in landmarks.iter().enumerate() {
        let src = lmid as usize;
        // forward distances: from landmark to nodes
        let df = dijkstra(&adj_fwd, src);
        for n in 0..nodes {
            lm_fw[li * nodes + n] = df[n];
        }
        // backward distances: to landmark (run on reverse graph)
        let db = dijkstra(&adj_rev, src);
        for n in 0..nodes {
            lm_bw[li * nodes + n] = db[n];
        }
    }

    (lm_fw, lm_bw)
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

fn dijkstra(adj: &Vec<Vec<(u32, f32)>>, start: usize) -> Vec<f32> {
    let n = adj.len();
    let mut dist = vec![f32::INFINITY; n];
    if start >= n { return dist; }
    dist[start] = 0.0;
    let mut heap = BinaryHeap::new();
    heap.push(State { cost_bits: 0.0f32.to_bits(), node: start as u32 });

    while let Some(State { cost_bits, node }) = heap.pop() {
        let u = node as usize;
        let cost = f32::from_bits(cost_bits);
        if cost > dist[u] { continue; }
        for &(v_u32, w) in &adj[u] {
            let v = v_u32 as usize;
            let next = cost + w;
            if next < dist[v] {
                dist[v] = next;
                heap.push(State { cost_bits: next.to_bits(), node: v_u32 });
            }
        }
    }

    dist
}
