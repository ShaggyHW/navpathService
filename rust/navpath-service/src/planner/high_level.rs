use crate::models::Tile;
use crate::planner::graph::{build_graph, BuildOptions as GraphBuildOptions, Graph, GraphInputs, NodeKind};
use crate::planner::micro_astar::find_path_4dir;
use crate::requirements::RequirementEvaluator;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct HopPlan {
    pub entrances: Vec<i64>,
}

/// Build the abstract graph and compute the high-level node index path
/// using dynamic micro edges from start->entrances and entrances->end.
/// Returns (Graph, indices) on success.
pub fn plan_hl_indices(
    inputs: &GraphInputs<'_>,
    evaluator: &RequirementEvaluator,
    start: Tile,
    end: Tile,
    cluster_tiles: &HashMap<i64, HashSet<(i32, i32, i32)>>,
    is_walkable: &Box<dyn Fn(i32, i32, i32) -> bool + '_>,
) -> Option<(Graph, Vec<usize>)> {
    let opts = GraphBuildOptions { start, end };
    let graph = build_graph(inputs, evaluator, &opts);
    // locate virtual start/end indices
    let start_idx = graph.nodes.iter().position(|n| matches!(n.kind, NodeKind::VirtualStart(_)))?;
    let end_idx = graph.nodes.iter().position(|n| matches!(n.kind, NodeKind::VirtualEnd(_)))?;

    // Build adjacency including base edges
    let mut adj: Vec<Vec<(usize, i64)>> = vec![Vec::new(); graph.nodes.len()];
    for e in graph.edges.iter() { adj[e.from.0].push((e.to.0, e.cost)); }

    // Dynamic micro edges: start->entrance and entrance->end
    for (idx, node) in graph.nodes.iter().enumerate() {
        if let NodeKind::Entrance { entrance_id: _, cluster_id, x, y, plane } = node.kind {
            if plane == start.plane {
                if let Some(c) = micro_cost_within_cluster(start, Tile { x, y, plane }, cluster_id, cluster_tiles, is_walkable) {
                    adj[start_idx].push((idx, c));
                }
            }
            if plane == end.plane {
                if let Some(c) = micro_cost_within_cluster(Tile { x, y, plane }, end, cluster_id, cluster_tiles, is_walkable) {
                    adj[idx].push((end_idx, c));
                }
            }
        }
    }

    let indices = dijkstra_indices_with_adj(&graph, &adj, start_idx, end_idx)?;
    Some((graph, indices))
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
struct HLState {
    f: i64,
    g: i64,
    idx: usize,
    seq: u64,
}

impl Ord for HLState {
    fn cmp(&self, other: &Self) -> Ordering {
        // Dijkstra: minimize g, break ties deterministically by idx and seq
        (other.g, other.idx, other.seq).cmp(&(self.g, self.idx, self.seq))
    }
}
impl PartialOrd for HLState { fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) } }

pub fn plan_hops(
    inputs: &GraphInputs<'_>,
    evaluator: &RequirementEvaluator,
    start: Tile,
    end: Tile,
    cluster_tiles: &HashMap<i64, HashSet<(i32, i32, i32)>>,
    is_walkable: &Box<dyn Fn(i32, i32, i32) -> bool + '_>,
) -> Option<HopPlan> {
    let opts = GraphBuildOptions { start, end };
    let graph = build_graph(inputs, evaluator, &opts);
    // locate virtual start/end indices
    let start_idx = graph.nodes.iter().position(|n| matches!(n.kind, NodeKind::VirtualStart(_)))?;
    let end_idx = graph.nodes.iter().position(|n| matches!(n.kind, NodeKind::VirtualEnd(_)))?;

    // Build adjacency including base edges
    let mut adj: Vec<Vec<(usize, i64)>> = vec![Vec::new(); graph.nodes.len()];
    for e in graph.edges.iter() { adj[e.from.0].push((e.to.0, e.cost)); }

    // Dynamic micro edges: start->entrance and entrance->end
    for (idx, node) in graph.nodes.iter().enumerate() {
        if let NodeKind::Entrance { entrance_id: _, cluster_id, x, y, plane } = node.kind {
            if plane == start.plane {
                if let Some(c) = micro_cost_within_cluster(start, Tile { x, y, plane }, cluster_id, cluster_tiles, is_walkable) {
                    adj[start_idx].push((idx, c));
                }
            }
            if plane == end.plane {
                if let Some(c) = micro_cost_within_cluster(Tile { x, y, plane }, end, cluster_id, cluster_tiles, is_walkable) {
                    adj[idx].push((end_idx, c));
                }
            }
        }
    }

    let indices = dijkstra_indices_with_adj(&graph, &adj, start_idx, end_idx)?;
    let mut entrances = Vec::new();
    for i in indices {
        if let NodeKind::Entrance { entrance_id, .. } = graph.nodes[i].kind {
            entrances.push(entrance_id);
        }
    }
    Some(HopPlan { entrances })
}

fn dijkstra_indices_with_adj(graph: &Graph, adj: &Vec<Vec<(usize, i64)>>, start_idx: usize, end_idx: usize) -> Option<Vec<usize>> {
    let mut open = BinaryHeap::new();
    let mut came_from: HashMap<usize, usize> = HashMap::new();
    let mut g_score: HashMap<usize, i64> = HashMap::new();
    let mut in_open: HashSet<usize> = HashSet::new();
    let mut seq: u64 = 0;

    g_score.insert(start_idx, 0);
    open.push(HLState { f: 0, g: 0, idx: start_idx, seq });
    in_open.insert(start_idx);

    while let Some(st) = open.pop() {
        if st.idx == end_idx {
            let mut path = Vec::new();
            let mut cur = st.idx;
            path.push(cur);
            while let Some(&p) = came_from.get(&cur) { cur = p; path.push(cur); }
            path.reverse();
            return Some(path);
        }
        in_open.remove(&st.idx);
        for (to, w) in &adj[st.idx] {
            let tentative_g = st.g + *w;
            let best = g_score.get(to).copied().unwrap_or(i64::MAX);
            if tentative_g < best {
                came_from.insert(*to, st.idx);
                g_score.insert(*to, tentative_g);
                seq = seq.wrapping_add(1);
                let s2 = HLState { f: tentative_g, g: tentative_g, idx: *to, seq };
                if !in_open.contains(to) { open.push(s2); in_open.insert(*to); } else { open.push(s2); }
            }
        }
    }
    None
}

fn micro_cost_within_cluster(
    a: Tile,
    b: Tile,
    cluster_id: i64,
    cluster_tiles: &HashMap<i64, HashSet<(i32, i32, i32)>>,
    is_walkable: &Box<dyn Fn(i32, i32, i32) -> bool + '_>,
) -> Option<i64> {
    let tiles = cluster_tiles.get(&cluster_id)?;
    let allowed = |x: i32, y: i32| tiles.contains(&(x, y, a.plane));
    let walk = |x: i32, y: i32| (is_walkable)(x, y, a.plane);
    let path = find_path_4dir(a, b, allowed, walk)?;
    Some((path.len() as i64).saturating_sub(1))
}
