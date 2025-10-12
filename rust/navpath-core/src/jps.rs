//! Jump Point Search (JPS) pruning utilities.
//!
//! This module provides helpers to support JPS-style pruning on a grid using the
//! existing `GraphProvider` movement edges. Integration with `AStar` is performed
//! separately so these utilities focus on detection and stepping logic.

use crate::graph::movement::{EAST, MOVEMENT_ORDER, NORTH, NORTHEAST, NORTHWEST, SOUTH, SOUTHEAST, SOUTHWEST, WEST};
use crate::graph::provider::{Edge as GEdge, GraphProvider};
use crate::models::Tile;
use crate::options::SearchOptions;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub dx: i32,
    pub dy: i32,
}

impl Step {
    pub fn is_diagonal(&self) -> bool { self.dx != 0 && self.dy != 0 }
}

/// Configuration for JPS pruning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JpsConfig {
    pub enabled: bool,
    pub allow_diagonals: bool,
    pub max_jump: Option<u32>,
}

impl Default for JpsConfig {
    fn default() -> Self { Self { enabled: true, allow_diagonals: true, max_jump: None } }
}

impl JpsConfig {
    pub fn disabled() -> Self { Self { enabled: false, ..Default::default() } }
    pub fn enabled() -> Self { Self { enabled: true, ..Default::default() } }
}

/// Stateless pruner that evaluates movement edges against JPS rules.
pub struct JpsPruner<'a, P: GraphProvider> {
    provider: &'a P,
    goal: Tile,
    options: &'a SearchOptions,
    config: JpsConfig,
}

impl<'a, P: GraphProvider> JpsPruner<'a, P> {
    pub fn new(provider: &'a P, goal: Tile, options: &'a SearchOptions, config: JpsConfig) -> Self {
        Self { provider, goal, options, config }
    }

    /// Determine the normalized direction from `from` to `to` as a unit step in {-1,0,1} per axis.
    pub fn direction(from: Tile, to: Tile) -> Option<Step> {
        let mut dx = to[0] - from[0];
        let mut dy = to[1] - from[1];
        if dx == 0 && dy == 0 { return None; }
        if dx != 0 { dx = dx.signum(); }
        if dy != 0 { dy = dy.signum(); }
        Some(Step { dx, dy })
    }

    /// Prune movement edges using JPS rules given the current tile and optional parent tile.
    /// Non-movement edges are returned unchanged; movement edges are filtered.
    pub fn prune_neighbors(&self, current: Tile, parent: Option<Tile>, raw: Vec<GEdge>) -> Vec<GEdge> {
        if !self.config.enabled || parent.is_none() {
            // Either disabled or at the start node: keep original ordering
            return raw;
        }
        let parent = parent.unwrap();
        let Some(dir) = Self::direction(parent, current) else { return raw };

        // Partition edges by movement vs others
        let mut others: Vec<GEdge> = Vec::new();
        let mut moves: Vec<GEdge> = Vec::new();
        for e in raw.into_iter() {
            if e.type_ == "move" { moves.push(e); } else { others.push(e); }
        }

        // Build a quick lookup of movement offsets available from `current`.
        let mut has_offset = |dx: i32, dy: i32| -> bool {
            moves.iter().any(|e| e.to_tile == [current[0] + dx, current[1] + dy, current[2]])
        };

        // Natural neighbors for the incoming direction
        let mut allowed_offsets: Vec<(i32,i32)> = Vec::new();
        if dir.is_diagonal() {
            // Diagonal: natural include (dx,dy), (dx,0), (0,dy)
            allowed_offsets.push((dir.dx, dir.dy));
            allowed_offsets.push((dir.dx, 0));
            allowed_offsets.push((0, dir.dy));
        } else {
            // Cardinal: natural include (dx,dy)
            allowed_offsets.push((dir.dx, dir.dy));
        }

        // Forced neighbors
        for (fx, fy) in self.forced_offsets(current, dir, &mut has_offset) {
            allowed_offsets.push((fx, fy));
        }

        // Filter movement edges to those matching allowed offsets, keep deterministic order per MOVEMENT_ORDER.
        let mut kept: Vec<GEdge> = Vec::new();
        for m in MOVEMENT_ORDER.iter() {
            let (dx, dy) = (m.dx, m.dy);
            if allowed_offsets.iter().any(|&(ax, ay)| ax == dx && ay == dy) {
                // Keep the exact edge if present
                if let Some(e) = moves.iter().find(|e| e.to_tile == [current[0] + dx, current[1] + dy, current[2]]) {
                    kept.push(e.clone());
                }
            }
        }
        // Append others unchanged after movement set to preserve type ordering semantics in provider
        kept.extend(others.into_iter());
        kept
    }

    /// Compute forced neighbor offsets for the current tile relative to the incoming direction.
    fn forced_offsets<F: FnMut(i32,i32) -> bool>(&self, _current: Tile, dir: Step, mut has: F) -> Vec<(i32,i32)> {
        let mut out: Vec<(i32,i32)> = Vec::new();
        if !dir.is_diagonal() {
            // Cardinal directions
            if dir.dx == 1 && dir.dy == 0 {
                // East: if north is blocked but northeast exists -> NE forced; if south blocked but southeast exists -> SE forced
                if !has(0, 1) && has(1, 1) { out.push((1, 1)); }
                if !has(0, -1) && has(1, -1) { out.push((1, -1)); }
            } else if dir.dx == -1 && dir.dy == 0 {
                if !has(0, 1) && has(-1, 1) { out.push((-1, 1)); }
                if !has(0, -1) && has(-1, -1) { out.push((-1, -1)); }
            } else if dir.dx == 0 && dir.dy == 1 {
                if !has(1, 0) && has(1, 1) { out.push((1, 1)); }
                if !has(-1, 0) && has(-1, 1) { out.push((-1, 1)); }
            } else if dir.dx == 0 && dir.dy == -1 {
                if !has(1, 0) && has(1, -1) { out.push((1, -1)); }
                if !has(-1, 0) && has(-1, -1) { out.push((-1, -1)); }
            }
        } else {
            // Diagonal: if one side is blocked, the outward diagonal becomes forced
            // Check (dx,0) side and (0,dy) side
            if !has(dir.dx, 0) && has(dir.dx, dir.dy) { out.push((dir.dx, dir.dy)); }
            if !has(0, dir.dy) && has(dir.dx, dir.dy) { out.push((dir.dx, dir.dy)); }
        }
        out
    }

    /// Perform a JPS jump from `from` stepping along `dir` until we hit a jump point, the goal, or we cannot continue.
    pub fn jump(&self, from: Tile, dir: Step) -> rusqlite::Result<Option<Tile>> {
        if !self.config.enabled { return Ok(None); }
        let mut steps: u32 = 0;
        let mut current = from;
        loop {
            if let Some(maxj) = self.config.max_jump { if steps >= maxj { return Ok(Some(current)); } }
            // Next tile in direction
            let next = [current[0] + dir.dx, current[1] + dir.dy, current[2]];
            // Determine if movement to `next` exists
            let moves = self.movement_neighbors(current)?;
            if !moves.iter().any(|e| e.to_tile == next) { return Ok(None); }
            current = next;
            steps += 1;
            // Goal reached
            if current == self.goal { return Ok(Some(current)); }
            // Check for forced neighbors at current
            let mut has = |dx: i32, dy: i32| -> bool {
                moves.iter().any(|e| e.to_tile == [current[0] + dx, current[1] + dy, current[2]])
            };
            if !self.forced_offsets(current, dir, &mut has).is_empty() { return Ok(Some(current)); }
            // If diagonal, recurse on the cardinal components
            if dir.is_diagonal() {
                if self.jump(current, Step { dx: dir.dx, dy: 0 })?.is_some() { return Ok(Some(current)); }
                if self.jump(current, Step { dx: 0, dy: dir.dy })?.is_some() { return Ok(Some(current)); }
            }
        }
    }

    fn movement_neighbors(&self, tile: Tile) -> rusqlite::Result<Vec<GEdge>> {
        let neighbors = self.provider.neighbors(tile, self.goal, self.options)?;
        Ok(neighbors.into_iter().filter(|e| e.type_ == "move").collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::CostModel;
    use crate::graph::provider::{Edge as GEdge, GraphProvider};

    // Mock grid provider using an open-set of tiles. Diagonals allowed when destination exists.
    struct GridProvider { open: std::collections::HashSet<(i32,i32,i32)> }
    impl GridProvider { fn new(open: &[[i32;3]]) -> Self { Self { open: open.iter().map(|t| (t[0],t[1],t[2])).collect() } } }
    impl GraphProvider for GridProvider {
        fn neighbors(&self, tile: Tile, _goal: Tile, _options: &SearchOptions) -> rusqlite::Result<Vec<GEdge>> {
            let mut out = Vec::new();
            if !self.open.contains(&(tile[0],tile[1],tile[2])) { return Ok(out); }
            for m in MOVEMENT_ORDER.iter() {
                let to = [tile[0] + m.dx, tile[1] + m.dy, tile[2]];
                if self.open.contains(&(to[0],to[1],to[2])) {
                    out.push(GEdge { type_: "move".into(), from_tile: tile, to_tile: to, cost_ms: crate::cost::DEFAULT_STEP_COST_MS, node: None, metadata: None });
                }
            }
            Ok(out)
        }
    }

    fn default_opts() -> SearchOptions { SearchOptions::default() }

    #[test]
    fn direction_normalizes() {
        assert_eq!(JpsPruner::<GridProvider>::direction([0,0,0],[3,0,0]), Some(Step{dx:1,dy:0}));
        assert_eq!(JpsPruner::<GridProvider>::direction([0,0,0],[0,-5,0]), Some(Step{dx:0,dy:-1}));
        assert_eq!(JpsPruner::<GridProvider>::direction([0,0,0],[7,7,0]), Some(Step{dx:1,dy:1}));
        assert_eq!(JpsPruner::<GridProvider>::direction([0,0,0],[0,0,0]), None);
    }

    #[test]
    fn prune_includes_natural_and_forced_for_cardinal() {
        // Block north from (0,0,0) but allow NE; moving east induces NE as forced neighbor
        let open = vec![ [0,0,0], [1,0,0], [1,1,0] ];
        let gp = GridProvider::new(&open);
        let opts = default_opts();
        let cfg = JpsConfig { enabled: true, ..Default::default() };
        let pruner = JpsPruner::new(&gp, [10,10,0], &opts, cfg);
        let cur = [0,0,0];
        let parent = Some([-1,0,0]); // came from west -> dir east
        let raw = gp.neighbors(cur, [10,10,0], &opts).unwrap();
        let pruned = pruner.prune_neighbors(cur, parent, raw);
        let move_dests: Vec<Tile> = pruned.iter().filter(|e| e.type_=="move").map(|e| e.to_tile).collect();
        assert!(move_dests.contains(&[1,0,0])); // natural forward
        assert!(move_dests.contains(&[1,1,0])); // forced NE
        assert!(!move_dests.contains(&[0,1,0])); // blocked north not included
    }

    #[test]
    fn jump_reaches_goal_in_straight_corridor() {
        // Open east corridor from (0,0,0) to (5,0,0)
        let open = vec![ [0,0,0], [1,0,0], [2,0,0], [3,0,0], [4,0,0], [5,0,0] ];
        let gp = GridProvider::new(&open);
        let opts = default_opts();
        let cfg = JpsConfig { enabled: true, ..Default::default() };
        let pruner = JpsPruner::new(&gp, [5,0,0], &opts, cfg);
        let jp = pruner.jump([0,0,0], Step{dx:1,dy:0}).unwrap();
        assert_eq!(jp, Some([5,0,0]));
    }

    #[test]
    fn jump_reaches_goal_in_diagonal_corridor() {
        // Open NE corridor (0,0,0) -> (3,3,0)
        let open = vec![ [0,0,0], [1,1,0], [2,2,0], [3,3,0] ];
        let gp = GridProvider::new(&open);
        let opts = default_opts();
        let cfg = JpsConfig { enabled: true, ..Default::default() };
        let pruner = JpsPruner::new(&gp, [3,3,0], &opts, cfg);
        let jp = pruner.jump([0,0,0], Step{dx:1,dy:1}).unwrap();
        assert_eq!(jp, Some([3,3,0]));
    }
}
