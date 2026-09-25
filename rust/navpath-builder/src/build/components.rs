//! Connectivity structure of the ALT table graph.
//!
//! The walk graph is symmetric, so every walk component is STRONGLY connected. The
//! strongly and weakly connected components of the whole table graph (walk + macro +
//! fairy clique) are therefore unions of walk components, and they can be computed on
//! the tiny condensation whose nodes are walk components (~800) and whose edges are the
//! extra edges (~4k) — O(V + E) over the walk CSR once, then trivial.

/// Walk components over the symmetric walk CSR, labelled in order of their lowest node
/// id. That is exactly the labelling of the old union-find + first-appearance
/// compaction (`walk_component_ids`), so the snapshot's `comp` section is unchanged.
pub fn walk_components(off: &[u32], dst: &[u32]) -> (Vec<u32>, usize) {
    let n = off.len() - 1;
    const NONE: u32 = u32::MAX;
    let mut label = vec![NONE; n];
    let mut stack: Vec<u32> = Vec::new();
    let mut next = 0u32;
    for s in 0..n {
        if label[s] != NONE {
            continue;
        }
        label[s] = next;
        stack.push(s as u32);
        while let Some(u) = stack.pop() {
            let u = u as usize;
            for &v in &dst[off[u] as usize..off[u + 1] as usize] {
                if label[v as usize] == NONE {
                    label[v as usize] = next;
                    stack.push(v);
                }
            }
        }
        next += 1;
    }
    (label, next as usize)
}

/// SCC / WCC structure of the table graph, stored per walk component.
pub struct Structure {
    /// Walk component of every node.
    pub comp: Vec<u32>,
    pub comp_size: Vec<u32>,
    /// SCC id per walk component. SCC ids are ranked: 0 is the largest SCC, ties broken
    /// by lowest member node id, so the numbering is deterministic.
    pub comp_scc: Vec<u32>,
    /// WCC id per walk component, ranked the same way.
    pub comp_wcc: Vec<u32>,
    pub scc_size: Vec<usize>,
    pub wcc_size: Vec<usize>,
    /// WCC containing each SCC.
    pub scc_wcc: Vec<u32>,
    /// Lowest node id of each SCC / WCC.
    pub scc_min_node: Vec<u32>,
    pub wcc_min_node: Vec<u32>,
}

impl Structure {
    /// `comp`/`comp_count` from [`walk_components`]; `extra` = (src, dst) of every
    /// non-walk table-graph edge.
    pub fn new(comp: Vec<u32>, comp_count: usize, extra: impl Iterator<Item = (u32, u32)>) -> Self {
        let c = comp_count;
        let mut comp_size = vec![0u32; c];
        let mut comp_min = vec![u32::MAX; c];
        for (v, &k) in comp.iter().enumerate() {
            comp_size[k as usize] += 1;
            if comp_min[k as usize] == u32::MAX {
                comp_min[k as usize] = v as u32;
            }
        }
        // Condensation adjacency (deduplicated, self-loops dropped).
        let mut edges: Vec<(u32, u32)> = extra
            .filter_map(|(s, d)| {
                let (cs, cd) = (comp[s as usize], comp[d as usize]);
                (cs != cd).then_some((cs, cd))
            })
            .collect();
        edges.sort_unstable();
        edges.dedup();
        let mut off = vec![0u32; c + 1];
        for &(s, _) in &edges {
            off[s as usize + 1] += 1;
        }
        for i in 0..c {
            off[i + 1] += off[i];
        }
        let adj: Vec<u32> = edges.iter().map(|&(_, d)| d).collect();

        let raw_scc = tarjan(c, &off, &adj);

        // Weak components: union-find over the same edges.
        let mut parent: Vec<u32> = (0..c as u32).collect();
        fn find(p: &mut [u32], mut x: u32) -> u32 {
            while p[x as usize] != x {
                p[x as usize] = p[p[x as usize] as usize];
                x = p[x as usize];
            }
            x
        }
        for &(s, d) in &edges {
            let (a, b) = (find(&mut parent, s), find(&mut parent, d));
            if a != b {
                parent[a as usize] = b;
            }
        }
        let raw_wcc: Vec<u32> = (0..c as u32).map(|k| find(&mut parent, k)).collect();

        let (comp_scc, scc_size, scc_min_node) = rank_groups(&raw_scc, &comp_size, &comp_min);
        let (comp_wcc, wcc_size, wcc_min_node) = rank_groups(&raw_wcc, &comp_size, &comp_min);
        let mut scc_wcc = vec![0u32; scc_size.len()];
        for k in 0..c {
            scc_wcc[comp_scc[k] as usize] = comp_wcc[k];
        }
        Structure { comp, comp_size, comp_scc, comp_wcc, scc_size, wcc_size, scc_wcc, scc_min_node, wcc_min_node }
    }

    #[inline]
    pub fn scc_of(&self, v: usize) -> u32 {
        self.comp_scc[self.comp[v] as usize]
    }

    #[inline]
    pub fn wcc_of(&self, v: usize) -> u32 {
        self.comp_wcc[self.comp[v] as usize]
    }

    pub fn nodes(&self) -> usize {
        self.comp.len()
    }

    /// Node lists per group (SCC if `scc`, else WCC), each ascending.
    pub fn members(&self, scc: bool) -> Vec<Vec<u32>> {
        let sizes = if scc { &self.scc_size } else { &self.wcc_size };
        let mut out: Vec<Vec<u32>> = sizes.iter().map(|&s| Vec::with_capacity(s)).collect();
        for v in 0..self.comp.len() {
            let g = if scc { self.scc_of(v) } else { self.wcc_of(v) };
            out[g as usize].push(v as u32);
        }
        out
    }
}

/// Relabel arbitrary group ids so that group 0 is the largest (by node count), ties by
/// lowest member node id. Returns (rank per component, size per rank, min node per rank).
fn rank_groups(raw: &[u32], comp_size: &[u32], comp_min: &[u32]) -> (Vec<u32>, Vec<usize>, Vec<u32>) {
    use std::collections::HashMap;
    let mut agg: HashMap<u32, (usize, u32)> = HashMap::new();
    for (k, &g) in raw.iter().enumerate() {
        let e = agg.entry(g).or_insert((0, u32::MAX));
        e.0 += comp_size[k] as usize;
        e.1 = e.1.min(comp_min[k]);
    }
    let mut groups: Vec<(u32, usize, u32)> = agg.into_iter().map(|(g, (s, m))| (g, s, m)).collect();
    groups.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    let mut rank_of: HashMap<u32, u32> = HashMap::with_capacity(groups.len());
    for (r, g) in groups.iter().enumerate() {
        rank_of.insert(g.0, r as u32);
    }
    let per_comp = raw.iter().map(|g| rank_of[g]).collect();
    let sizes = groups.iter().map(|g| g.1).collect();
    let mins = groups.iter().map(|g| g.2).collect();
    (per_comp, sizes, mins)
}

/// Iterative Tarjan SCC over a small CSR. Returns an arbitrary SCC id per node.
fn tarjan(n: usize, off: &[u32], adj: &[u32]) -> Vec<u32> {
    const UNSEEN: u32 = u32::MAX;
    let mut index = vec![UNSEEN; n];
    let mut low = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<u32> = Vec::new();
    let mut scc = vec![UNSEEN; n];
    let mut next_index = 0u32;
    let mut next_scc = 0u32;
    // Call stack of (node, next edge position).
    let mut call: Vec<(u32, u32)> = Vec::new();
    for root in 0..n as u32 {
        if index[root as usize] != UNSEEN {
            continue;
        }
        call.push((root, off[root as usize]));
        index[root as usize] = next_index;
        low[root as usize] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root as usize] = true;
        while let Some(&(u, pos)) = call.last() {
            let ui = u as usize;
            if pos < off[ui + 1] {
                call.last_mut().unwrap().1 += 1;
                let v = adj[pos as usize];
                let vi = v as usize;
                if index[vi] == UNSEEN {
                    index[vi] = next_index;
                    low[vi] = next_index;
                    next_index += 1;
                    stack.push(v);
                    on_stack[vi] = true;
                    call.push((v, off[vi]));
                } else if on_stack[vi] {
                    low[ui] = low[ui].min(index[vi]);
                }
            } else {
                call.pop();
                if let Some(&(p, _)) = call.last() {
                    low[p as usize] = low[p as usize].min(low[ui]);
                }
                if low[ui] == index[ui] {
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack[w as usize] = false;
                        scc[w as usize] = next_scc;
                        if w == u {
                            break;
                        }
                    }
                    next_scc += 1;
                }
            }
        }
    }
    scc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_labels_follow_lowest_node_order() {
        // 0-3 connected, 1-2 connected, 4 alone: labels by min node: {0,3}=0, {1,2}=1, {4}=2.
        let off = vec![0u32, 1, 2, 3, 4, 4];
        let dst = vec![3u32, 2, 1, 0];
        let (lab, count) = walk_components(&off, &dst);
        assert_eq!(count, 3);
        assert_eq!(lab, vec![0, 1, 1, 0, 2]);
    }

    #[test]
    fn scc_and_wcc_over_condensation() {
        // Walk comps: {0,1}, {2,3}, {4}, {5}. Extras: 1->2, 3->0 (so {0..3} one SCC),
        // 3->4 (sink pocket), 5 isolated.
        let off = vec![0u32, 1, 2, 3, 4, 4, 4];
        let dst = vec![1u32, 0, 3, 2];
        let (comp, cc) = walk_components(&off, &dst);
        let st = Structure::new(comp, cc, vec![(1u32, 2u32), (3, 0), (3, 4)].into_iter());
        assert_eq!(st.scc_of(0), st.scc_of(3));
        assert_ne!(st.scc_of(0), st.scc_of(4));
        assert_eq!(st.scc_of(0), 0, "largest SCC ranks first");
        assert_eq!(st.scc_size[0], 4);
        assert_eq!(st.wcc_of(4), st.wcc_of(0));
        assert_ne!(st.wcc_of(5), st.wcc_of(0));
        assert_eq!(st.wcc_size[st.wcc_of(0) as usize], 5);
        assert_eq!(st.scc_wcc[st.scc_of(4) as usize], st.wcc_of(0));
    }
}
