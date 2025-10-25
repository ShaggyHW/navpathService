use crate::models::Tile;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
struct State {
    // f = g + h
    f: i32,
    g: i32,
    x: i32,
    y: i32,
    // Monotonic increasing sequence to keep pop order deterministic
    seq: u64,
}

impl Ord for State {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse for min-heap behavior when used directly in BinaryHeap (which is a max-heap)
        // We compare f asc, then g asc, then y asc, then x asc, then seq asc
        (other.f, other.g, other.y, other.x, other.seq).cmp(&(self.f, self.g, self.y, self.x, self.seq))
    }
}

impl PartialOrd for State {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn manhattan(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs() + (a.1 - b.1).abs()
}

pub fn find_path_4dir<FA, FW>(start: Tile, goal: Tile, is_allowed: FA, is_walkable: FW) -> Option<Vec<Tile>>
where
    FA: Fn(i32, i32) -> bool,
    FW: Fn(i32, i32) -> bool,
{
    if start.plane != goal.plane {
        return None;
    }
    if start.x == goal.x && start.y == goal.y {
        return Some(vec![start]);
    }

    let plane = start.plane;
    if !is_allowed(start.x, start.y) || !is_allowed(goal.x, goal.y) {
        return None;
    }
    if !is_walkable(start.x, start.y) || !is_walkable(goal.x, goal.y) {
        return None;
    }

    // A* setup
    let mut open = BinaryHeap::new();
    let mut came_from: HashMap<(i32, i32), (i32, i32)> = HashMap::new();
    let mut g_score: HashMap<(i32, i32), i32> = HashMap::new();
    let mut in_open: HashSet<(i32, i32)> = HashSet::new();
    let mut seq: u64 = 0;

    let start_pos = (start.x, start.y);
    let goal_pos = (goal.x, goal.y);

    g_score.insert(start_pos, 0);
    open.push(State { f: manhattan(start_pos, goal_pos), g: 0, x: start.x, y: start.y, seq });
    in_open.insert(start_pos);

    // Fixed neighbor order for determinism: N, E, S, W
    let neighbors = [(0, -1), (1, 0), (0, 1), (-1, 0)];

    while let Some(current) = open.pop() {
        let cur_pos = (current.x, current.y);
        if cur_pos == goal_pos {
            // reconstruct path
            let mut path = Vec::new();
            let mut p = cur_pos;
            path.push(Tile { x: p.0, y: p.1, plane });
            while let Some(prev) = came_from.get(&p) {
                p = *prev;
                path.push(Tile { x: p.0, y: p.1, plane });
            }
            path.reverse();
            return Some(path);
        }

        in_open.remove(&cur_pos);

        for (dx, dy) in neighbors {
            let nx = current.x + dx;
            let ny = current.y + dy;
            if !is_allowed(nx, ny) || !is_walkable(nx, ny) {
                continue;
            }
            let neighbor_pos = (nx, ny);
            let tentative_g = current.g + 1; // uniform cost
            let ng = *g_score.get(&neighbor_pos).unwrap_or(&i32::MAX);
            if tentative_g < ng {
                came_from.insert(neighbor_pos, cur_pos);
                g_score.insert(neighbor_pos, tentative_g);
                let f = tentative_g + manhattan(neighbor_pos, goal_pos);
                seq = seq.wrapping_add(1);
                let state = State { f, g: tentative_g, x: nx, y: ny, seq };
                if !in_open.contains(&neighbor_pos) {
                    open.push(state);
                    in_open.insert(neighbor_pos);
                } else {
                    // Re-insert with better score for simplicity; duplicates are harmless due to best-g check
                    open.push(state);
                }
            }
        }
    }

    None
}
