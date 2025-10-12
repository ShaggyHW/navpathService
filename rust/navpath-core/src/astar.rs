use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::time::Instant;

use crate::cost::CostModel;
use crate::graph::provider::{Edge as GEdge, GraphProvider};
use crate::models::{ActionStep, NodeRef, Rect, Tile};
use crate::options::SearchOptions;
use serde_json::Value;
use crate::jps::{JpsConfig, JpsPruner};
use tracing::info;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TileKey(i32, i32, i32);
impl From<Tile> for TileKey { fn from(t: Tile) -> Self { TileKey(t[0], t[1], t[2]) } }
impl From<TileKey> for Tile { fn from(k: TileKey) -> Self { [k.0, k.1, k.2] } }

fn jps_config_from_options(options: &SearchOptions) -> JpsConfig {
    let mut cfg = JpsConfig::default();
    // Flat keys
    if let Some(v) = options.extras.get("jps_enabled").and_then(Value::as_bool) { cfg.enabled = v; }
    if let Some(v) = options.extras.get("jps_allow_diagonals").and_then(Value::as_bool) { cfg.allow_diagonals = v; }
    if let Some(v) = options.extras.get("jps_max_jump").and_then(Value::as_i64) { if v >= 0 { cfg.max_jump = Some(v as u32); } }
    // Nested object: { jps: { enabled, allow_diagonals, max_jump } }
    if let Some(obj) = options.extras.get("jps").and_then(Value::as_object) {
        if let Some(v) = obj.get("enabled").and_then(Value::as_bool) { cfg.enabled = v; }
        if let Some(v) = obj.get("allow_diagonals").and_then(Value::as_bool) { cfg.allow_diagonals = v; }
        if let Some(v) = obj.get("max_jump").and_then(Value::as_i64) { if v >= 0 { cfg.max_jump = Some(v as u32); } }
    }
    cfg
}

#[derive(Clone, Debug)]
struct CameFromEntry {
    prev: TileKey,
    edge_type: String,
    node: Option<NodeRef>,
    cost_ms: i64,
    metadata: Option<Value>,
}

#[derive(Clone, Copy, Debug)]
struct QueueNode {
    tile: TileKey,
    f: i64,
    g: i64,
    h: i64,
    seq: u64,
}

impl PartialEq for QueueNode { fn eq(&self, other: &Self) -> bool { self.f == other.f && self.h == other.h && self.g == other.g && self.seq == other.seq && self.tile == other.tile } }
impl Eq for QueueNode {}
impl PartialOrd for QueueNode { fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) } }
impl Ord for QueueNode {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is max-heap; invert ordering for min-heap behavior
        (other.f, other.h, other.g, other.seq, (other.tile.0, other.tile.1, other.tile.2))
            .cmp(&(self.f, self.h, self.g, self.seq, (self.tile.0, self.tile.1, self.tile.2)))
    }
}

pub struct AStar<'a, P: GraphProvider> {
    provider: &'a P,
    cost_model: &'a CostModel,
}

impl<'a, P: GraphProvider> AStar<'a, P> {
    pub fn new(provider: &'a P, cost_model: &'a CostModel) -> Self { Self { provider, cost_model } }

    pub fn find_path(&self, start: Tile, goal: Tile, options: &SearchOptions) -> rusqlite::Result<crate::models::PathResult> {
        let started_at = Instant::now();
        info!(start=?start, goal=?goal, movement_only=options.movement_only(), max_expansions=options.max_expansions, timeout_ms=options.timeout_ms, "astar_start");
        if start == goal {
            let out = crate::models::PathResult { path: Some(vec![start]), actions: vec![], reason: None, expanded: 0, cost_ms: 0 };
            let duration_ms = started_at.elapsed().as_millis() as u64;
            info!(expanded=out.expanded, jps_pruned_edges=0u64, duration_ms, neighbor_calls=0u64, total_neighbor_edges=0u64, avg_neighbors=0.0, max_neighbors=0usize, max_open_len=0usize, path_len=out.path.as_ref().map(|p| p.len()).unwrap_or(0), actions_len=out.actions.len(), reason=?out.reason, "astar_done");
            return Ok(out);
        }

        let mut open = BinaryHeap::new();
        let mut g_score: HashMap<TileKey, i64> = HashMap::new();
        let mut came_from: HashMap<TileKey, CameFromEntry> = HashMap::new();
        let mut expanded: u64 = 0;
        let mut seq: u64 = 0;
        let mut jps_pruned_edges: u64 = 0;
        let mut neighbor_calls: u64 = 0;
        let mut total_neighbor_edges: u64 = 0;
        let mut max_neighbors: usize = 0;
        let mut max_open_len: usize = 0;

        let h0 = self.cost_model.heuristic(start, goal);
        open.push(QueueNode { tile: start.into(), f: h0, g: 0, h: h0, seq });
        g_score.insert(start.into(), 0);
        max_open_len = max_open_len.max(open.len());

        while let Some(qn) = open.pop() {
            // Discard stale
            if let Some(best_g) = g_score.get(&qn.tile) { if qn.g > *best_g { continue; } }
            expanded += 1;
            if expanded > options.max_expansions {
                let out = crate::models::PathResult { path: None, actions: vec![], reason: Some("expansion-limit".into()), expanded, cost_ms: 0 };
                let duration_ms = started_at.elapsed().as_millis() as u64;
                let avg_neighbors = if neighbor_calls > 0 { total_neighbor_edges as f64 / neighbor_calls as f64 } else { 0.0 };
                info!(expanded=out.expanded, jps_pruned_edges, duration_ms, neighbor_calls, total_neighbor_edges, avg_neighbors, max_neighbors, max_open_len, path_len=out.path.as_ref().map(|p| p.len()).unwrap_or(0), actions_len=out.actions.len(), reason=?out.reason, "astar_done");
                return Ok(out);
            }

            let current: Tile = qn.tile.into();
            if current == goal {
                // reconstruct
                let (path, actions, total_cost) = reconstruct(&came_from, start, current);
                let out = crate::models::PathResult { path: Some(path), actions, reason: None, expanded, cost_ms: total_cost };
                let duration_ms = started_at.elapsed().as_millis() as u64;
                let avg_neighbors = if neighbor_calls > 0 { total_neighbor_edges as f64 / neighbor_calls as f64 } else { 0.0 };
                let path_len = out.path.as_ref().map(|p| p.len()).unwrap_or(0);
                let actions_len = out.actions.len();
                info!(expanded=out.expanded, jps_pruned_edges, duration_ms, neighbor_calls, total_neighbor_edges, avg_neighbors, max_neighbors, max_open_len, path_len, actions_len, reason=?out.reason, "astar_done");
                return Ok(out);
            }

            // Neighbors (optionally pruned via JPS for movement-only scenarios)
            let mut neighbors: Vec<GEdge> = self.provider.neighbors(current, goal, options)?;
            let prune_with_actions = options
                .extras
                .get("jps_prune_with_actions")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if options.movement_only() || prune_with_actions {
                let jps_cfg = jps_config_from_options(options);
                if jps_cfg.enabled {
                    let has_diagonal = neighbors.iter().any(|e| e.type_ == "move" && {
                        let dx = e.to_tile[0] - e.from_tile[0];
                        let dy = e.to_tile[1] - e.from_tile[1];
                        dx != 0 && dy != 0
                    });
                    if has_diagonal {
                        if let Some(entry) = came_from.get(&qn.tile) {
                            if entry.edge_type == "move" {
                                let parent_opt: Option<Tile> = Some(entry.prev.into());
                                let before_moves = neighbors.iter().filter(|e| e.type_ == "move").count();
                                let pruner = JpsPruner::new(self.provider, goal, options, jps_cfg);
                                neighbors = pruner.prune_neighbors(current, parent_opt, neighbors);
                                let after_moves = neighbors.iter().filter(|e| e.type_ == "move").count();
                                if after_moves < before_moves {
                                    jps_pruned_edges += (before_moves - after_moves) as u64;
                                }
                            }
                        }
                    }
                }
            }
            neighbor_calls += 1;
            let neighbor_count = neighbors.len();
            total_neighbor_edges += neighbor_count as u64;
            if neighbor_count > max_neighbors {
                max_neighbors = neighbor_count;
            }
            for e in neighbors {
                let tentative_g = qn.g + e.cost_ms;
                let nk: TileKey = e.to_tile.into();
                let best = g_score.get(&nk).copied();
                if best.map(|bg| tentative_g < bg).unwrap_or(true) {
                    g_score.insert(nk, tentative_g);
                    came_from.insert(nk, CameFromEntry { prev: qn.tile, edge_type: e.type_.clone(), node: e.node.clone(), cost_ms: e.cost_ms, metadata: e.metadata.clone() });
                    let h = self.cost_model.heuristic(e.to_tile, goal);
                    seq += 1;
                    let f = tentative_g + h;
                    open.push(QueueNode { tile: nk, f, g: tentative_g, h, seq });
                    let open_len = open.len();
                    if open_len > max_open_len {
                        max_open_len = open_len;
                    }
                }
            }
        }

        let out = crate::models::PathResult { path: None, actions: vec![], reason: Some("no-path".into()), expanded, cost_ms: 0 };
        let duration_ms = started_at.elapsed().as_millis() as u64;
        let avg_neighbors = if neighbor_calls > 0 { total_neighbor_edges as f64 / neighbor_calls as f64 } else { 0.0 };
        info!(expanded=out.expanded, jps_pruned_edges, duration_ms, neighbor_calls, total_neighbor_edges, avg_neighbors, max_neighbors, max_open_len, path_len=out.path.as_ref().map(|p| p.len()).unwrap_or(0), actions_len=out.actions.len(), reason=?out.reason, "astar_done");
        Ok(out)
    }
}

fn reconstruct(came_from: &HashMap<TileKey, CameFromEntry>, start: Tile, mut current: Tile) -> (Vec<Tile>, Vec<ActionStep>, i64) {
    let mut tiles: Vec<Tile> = vec![current];
    let mut actions: Vec<ActionStep> = Vec::new();
    let mut total_cost: i64 = 0;
    while current != start {
        let ck: TileKey = current.into();
        let entry = came_from.get(&ck).expect("reconstruct missing came_from entry");
        let prev: Tile = entry.prev.into();
        // Prepend tile
        tiles.push(prev);
        // Build action step for this edge
        // Default rects are single-tile bounds
        let mut from_rect = Rect { min: prev, max: prev };
        let mut to_rect = Rect { min: current, max: current };
        // If metadata carries a db_row with explicit orig/dest bounds, prefer those
        if let Some(Value::Object(map)) = &entry.metadata {
            if let Some(Value::Object(db_row)) = map.get("db_row") {
                // Helper to read bounds prefix ("orig" or "dest")
                let read_bounds = |prefix: &str, fallback_plane: i32| -> Option<Rect> {
                    let k = |s: &str| format!("{}_{}", prefix, s);
                    let min_x = db_row.get(&k("min_x")).and_then(Value::as_i64).map(|v| v as i32);
                    let max_x = db_row.get(&k("max_x")).and_then(Value::as_i64).map(|v| v as i32);
                    let min_y = db_row.get(&k("min_y")).and_then(Value::as_i64).map(|v| v as i32);
                    let max_y = db_row.get(&k("max_y")).and_then(Value::as_i64).map(|v| v as i32);
                    let plane = db_row.get(&k("plane")).and_then(Value::as_i64).map(|v| v as i32).unwrap_or(fallback_plane);
                    match (min_x, max_x, min_y, max_y) {
                        (Some(ax), Some(bx), Some(ay), Some(by)) => Some(Rect { min: [ax, ay, plane], max: [bx, by, plane] }),
                        _ => None,
                    }
                };
                // Use orig bounds for from_rect when available
                if let Some(r) = read_bounds("orig", prev[2]) { from_rect = r; }
                // Only expand to_rect when this step moves tiles
                if prev != current {
                    if let Some(r) = read_bounds("dest", current[2]) { to_rect = r; }
                }
            }
        }
        let step = ActionStep {
            type_: entry.edge_type.clone(),
            from_rect,
            to_rect,
            cost_ms: entry.cost_ms,
            node: entry.node.clone(),
            metadata: entry.metadata.clone(),
        };
        actions.push(step);
        total_cost += entry.cost_ms;
        current = prev;
    }
    tiles.reverse();
    actions.reverse();
    (tiles, actions, total_cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockProvider;
    impl GraphProvider for MockProvider {
        fn neighbors(&self, tile: Tile, _goal: Tile, _options: &SearchOptions) -> rusqlite::Result<Vec<GEdge>> {
            // Simple 2x1 movement: (0,0,0) -> (1,0,0)
            let mut edges = Vec::new();
            let [x,y,p] = tile;
            let to = [x+1,y,p];
            edges.push(GEdge { type_: "move".into(), from_tile: tile, to_tile: to, cost_ms: 200, node: None, metadata: None });
            Ok(edges)
        }
    }

    #[test]
    fn finds_simple_path_and_actions() {
        let provider = MockProvider;
        let opts = SearchOptions::default();
        let cm = CostModel::default();
        let astar = AStar::new(&provider, &cm);
        let res = astar.find_path([0,0,0],[2,0,0], &opts).unwrap();
        assert!(res.path.is_some());
        let path = res.path.unwrap();
        assert_eq!(path, vec![[0,0,0],[1,0,0],[2,0,0]]);
        assert_eq!(res.actions.len(), 2);
        assert_eq!(res.actions[0].type_, "move");
        assert_eq!(res.actions[1].type_, "move");
        assert!(res.cost_ms >= 400);
    }

    #[test]
    fn deterministic_tie_breaker() {
        // Two neighbors with same f/h/g order; sequence and tile order should deterministically choose one.
        struct TwoNeighborProvider;
        impl GraphProvider for TwoNeighborProvider {
            fn neighbors(&self, tile: Tile, _goal: Tile, _options: &SearchOptions) -> rusqlite::Result<Vec<GEdge>> {
                let [x,y,p] = tile;
                Ok(vec![
                    GEdge { type_: "move".into(), from_tile: tile, to_tile: [x+1,y,p], cost_ms: 200, node: None, metadata: None },
                    GEdge { type_: "move".into(), from_tile: tile, to_tile: [x,y+1,p], cost_ms: 200, node: None, metadata: None },
                ])
            }
        }
        let provider = TwoNeighborProvider;
        let opts = SearchOptions::default();
        let cm = CostModel::default();
        let astar = AStar::new(&provider, &cm);
        let res1 = astar.find_path([0,0,0],[1,1,0], &opts).unwrap();
        let res2 = astar.find_path([0,0,0],[1,1,0], &opts).unwrap();
        assert_eq!(res1.path, res2.path);
        assert_eq!(res1.actions, res2.actions);
    }

    #[test]
    fn jps_toggle_does_not_change_path_in_basic_grid() {
        struct GridProvider;
        impl GraphProvider for GridProvider {
            fn neighbors(&self, tile: Tile, _goal: Tile, _options: &SearchOptions) -> rusqlite::Result<Vec<GEdge>> {
                let [x,y,p] = tile;
                // Simple 2D grid: allow east and north moves
                Ok(vec![
                    GEdge { type_: "move".into(), from_tile: tile, to_tile: [x+1,y,p], cost_ms: 200, node: None, metadata: None },
                    GEdge { type_: "move".into(), from_tile: tile, to_tile: [x,y+1,p], cost_ms: 200, node: None, metadata: None },
                ])
            }
        }
        let provider = GridProvider;
        let mut opts_legacy = SearchOptions::default();
        // Movement-only to allow JPS pruning path, but disabled here
        opts_legacy.use_doors = false;
        opts_legacy.use_lodestones = false;
        opts_legacy.use_objects = false;
        opts_legacy.use_ifslots = false;
        opts_legacy.use_npcs = false;
        opts_legacy.use_items = false;

        let mut opts_jps = opts_legacy.clone();
        opts_jps.extras.insert("jps_enabled".into(), Value::from(true));

        let cm_legacy = CostModel::new(opts_legacy.clone());
        let cm_jps = CostModel::new(opts_jps.clone());
        let astar_legacy = AStar::new(&provider, &cm_legacy);
        let astar_jps = AStar::new(&provider, &cm_jps);
        let res_legacy = astar_legacy.find_path([0,0,0],[2,2,0], &opts_legacy).unwrap();
        let res_jps = astar_jps.find_path([0,0,0],[2,2,0], &opts_jps).unwrap();
        assert_eq!(res_legacy.path, res_jps.path);
        assert_eq!(res_legacy.actions, res_jps.actions);
    }
}