//! Canonical successor pruning for the uniform-cost 8-connected walk grid
//! (roadmap Phase E, §4.6 Stages 1-2a).
//!
//! Stage 1: the per-tile 8-direction mask grid is DERIVED from the v8 walk CSR at
//! load (1 B/node) — no snapshot format change. Two invariants make an O(1)
//! direction -> CSR-slot mapping possible, both validated during derivation:
//! node ids follow packed (plane, y, x) key order, and every CSR row is emitted in
//! ascending direction-bit order, so
//! `slot(u, d) = walk_offsets[u] + popcount(mask[u] & ((1 << d) - 1))`.
//!
//! Stage 2a: a per-(node, incoming-direction) successor table (8 B/node) prunes walk
//! successors that are STRICTLY dominated: direction `c` out of `u` (reached from `p`
//! via `d`) is dropped only when some local detour `p -> x -> n_c` (or the direct edge
//! `p -> n_c`) is STRICTLY cheaper than `p -> u -> n_c`. Soundness is unconditional:
//! a pruned triple on an optimal path would witness a cheaper path — contradiction —
//! so EVERY optimal path survives verbatim, node-wise optimal distances are preserved
//! (unidirectional and per-side in bidirectional MM), and the two known traps of
//! equal-cost tie-pruning are structurally avoided:
//!   1. the 4,406 measured diamond anomalies (diagonal banned, both cardinal detours
//!      open) where mechanical `<=`-pruning makes the equal-cost detours prune each
//!      other — ties are simply never pruned here;
//!   2. the parent-race: a node first reached at EQUAL cost from a non-canonical
//!      parent expands with that parent's pruning set, which can drop the canonical
//!      continuation that tie-pruning's rewriting proof relies on. Strict-only pruning
//!      is indifferent to which optimal parent won the race.
//! Equal-cost tie-pruning (classic canonical/JPS orderings, the remaining ~1.5x) is
//! Stage 2b, gated on resolving that proof obligation — see the roadmap log.
//!
//! Direction recovery at query time compares UNPACKED coordinates of the stored parent
//! (|dx| <= 1, |dy| <= 1, same plane), never raw key deltas — a key delta of ±1 can
//! also be an x-boundary wrap, and macro/teleport parents must fall back to the full
//! successor set. Arrival via an ADJACENT MACRO edge is priced correctly because every
//! adjacent macro edge is at least as expensive as the walk step it parallels
//! (validated by [`CanonicalGrid::build`]; measured minimum 600 ms vs 424.26) — the
//! pruning comparison then only UNDERSTATES the through-cost, which keeps pruned
//! alternatives no-worse and the decision sound.

use crate::snapshot::{unpack_coord, walk_diagonal_ms};

/// Direction bits, identical to the builder's mask encoding (graph.rs):
/// 0:left(-1,0) 1:bottom(0,-1) 2:right(+1,0) 3:top(0,+1)
/// 4:topleft(-1,+1) 5:bottomleft(-1,-1) 6:bottomright(+1,-1) 7:topright(+1,+1).
const DELTAS: [(i32, i32); 8] = [
    (-1, 0),
    (0, -1),
    (1, 0),
    (0, 1),
    (-1, 1),
    (-1, -1),
    (1, -1),
    (1, 1),
];

/// Exact integer step costs for local comparisons: cardinal 10, diagonal 14. The only
/// equalities in {sums of two} are the true geometric ties (2.0 == 2.0, 1+sqrt2 ==
/// sqrt2+1), and no 1-step cost ever equals a 2-step cost — so strict `<` on these
/// integers is exactly strict `<` on real costs.
const STEP_COST: [u16; 8] = [10, 10, 10, 10, 14, 14, 14, 14];

#[inline]
fn dir_of(dx: i32, dy: i32) -> Option<usize> {
    DELTAS.iter().position(|&(x, y)| x == dx && y == dy)
}

/// Load-derived canonical grid: effective outgoing masks + strict-domination
/// successor table. Build once per snapshot; ~9 MB at 1.12M nodes.
pub struct CanonicalGrid {
    /// Effective outgoing direction bits per node (bit d set = walk edge in DELTAS[d]).
    pub masks: Vec<u8>,
    /// `succ[node * 8 + incoming_dir]`: direction bits worth relaxing when the node
    /// was reached from the adjacent tile in `incoming_dir`. Entry arrivals (no
    /// parent, non-adjacent parent) use `masks[node]` instead.
    pub succ: Vec<u8>,
}

impl CanonicalGrid {
    /// Derive the grid from the snapshot's walk CSR. Errors (returned, not panicked)
    /// mean the snapshot violates a canonical precondition — the caller falls back to
    /// full expansion:
    /// - a walk edge that is not same-plane 8-adjacent, or a CSR row not in ascending
    ///   direction-bit order (pre-invariant snapshots);
    /// - an adjacent macro edge cheaper than its parallel walk step (breaks the
    ///   direction-recovery pricing argument).
    pub fn build(
        nodes: usize,
        coords: &[u32],
        walk_offsets: &[u32],
        walk_dst: &[u32],
        macro_src: &[u32],
        macro_dst: &[u32],
        macro_w: &[f32],
    ) -> Result<CanonicalGrid, String> {
        if coords.len() < nodes || walk_offsets.len() < nodes + 1 {
            return Err("coords/offsets shorter than node count".into());
        }
        let diag_ms = walk_diagonal_ms();
        for i in 0..macro_src.len() {
            let (s, d) = (macro_src[i] as usize, macro_dst[i] as usize);
            if s == 0 && d == 0 {
                continue; // synthetic global-metadata carrier
            }
            if s >= nodes || d >= nodes {
                continue;
            }
            let (sx, sy, sp) = unpack_coord(coords[s]);
            let (dx, dy, dp) = unpack_coord(coords[d]);
            if sp == dp && (sx - dx).abs() <= 1 && (sy - dy).abs() <= 1 && macro_w[i] < diag_ms {
                return Err(format!(
                    "adjacent macro edge {s}->{d} costs {} < walk diagonal {diag_ms}; \
                     direction-recovery pricing would be unsound",
                    macro_w[i]
                ));
            }
        }

        // ---- Stage 1: masks + invariant validation ----
        let mut masks = vec![0u8; nodes];
        for u in 0..nodes {
            let (ux, uy, up) = unpack_coord(coords[u]);
            let (s, e) = (walk_offsets[u] as usize, walk_offsets[u + 1] as usize);
            let mut prev_dir: i32 = -1;
            for &v in &walk_dst[s..e] {
                let (vx, vy, vp) = unpack_coord(coords[v as usize]);
                if vp != up {
                    return Err(format!("walk edge {u}->{v} crosses planes"));
                }
                let Some(d) = dir_of(vx - ux, vy - uy) else {
                    return Err(format!("walk edge {u}->{v} is not 8-adjacent"));
                };
                if (d as i32) <= prev_dir {
                    return Err(format!(
                        "CSR row of node {u} is not in ascending direction-bit order \
                         (pre-invariant snapshot?)"
                    ));
                }
                prev_dir = d as i32;
                masks[u] |= 1 << d;
            }
        }

        // ---- Stage 2a: strict-domination successor table ----
        // succ chunks are disjoint per node range: plain scoped threads, deterministic.
        let mut succ = vec![0u8; nodes * 8];
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(16);
        let chunk = nodes.div_ceil(threads.max(1)).max(1);
        let masks_ref = &masks;
        std::thread::scope(|scope| {
            for (ci, out) in succ.chunks_mut(chunk * 8).enumerate() {
                let base = ci * chunk;
                scope.spawn(move || {
                    for (local, row) in out.chunks_mut(8).enumerate() {
                        let u = base + local;
                        fill_succ_row(u, coords, walk_offsets, walk_dst, masks_ref, row);
                    }
                });
            }
        });

        Ok(CanonicalGrid { masks, succ })
    }

    /// Successor direction bits for `node` given its stored parent, or the full mask
    /// for entry arrivals (no parent / non-adjacent parent — seeds, teleports, macro
    /// and fairy hops, cross-plane moves).
    #[inline]
    pub fn succ_bits(&self, node: u32, parent: u32, coords: &[u32]) -> u8 {
        let u = node as usize;
        if parent == u32::MAX {
            return self.masks[u];
        }
        let (px, py, pp) = unpack_coord(coords[parent as usize]);
        let (ux, uy, up) = unpack_coord(coords[u]);
        if pp != up {
            return self.masks[u];
        }
        let (dx, dy) = (ux - px, uy - py);
        if dx.abs() > 1 || dy.abs() > 1 || (dx == 0 && dy == 0) {
            return self.masks[u];
        }
        // dir_of over the fixed 8-entry table; adjacency was just verified.
        let d = dir_of(dx, dy).unwrap();
        self.succ[u * 8 + d]
    }
}

/// Compute the strict-domination successor sets of `u` for all 8 incoming directions.
fn fill_succ_row(
    u: usize,
    coords: &[u32],
    walk_offsets: &[u32],
    walk_dst: &[u32],
    masks: &[u8],
    row: &mut [u8],
) {
    let mask_u = masks[u];
    let (ux, uy, up) = unpack_coord(coords[u]);

    // Neighbor node id of `w` in direction `d`, IF the walk edge exists in w's mask.
    let step = |w: usize, mask_w: u8, d: usize| -> Option<usize> {
        if mask_w & (1 << d) == 0 {
            return None;
        }
        let slot = walk_offsets[w] as usize + (mask_w & ((1u8 << d) - 1)).count_ones() as usize;
        Some(walk_dst[slot] as usize)
    };

    for din in 0..8 {
        // Parent tile p = u - delta(din). If it does not exist as a node OR has no
        // edge into u, this entry can only be consulted for adjacent-macro arrivals;
        // p must still exist for those, so a missing tile falls back to the full mask.
        let (pdx, pdy) = DELTAS[din];
        let (px, py) = (ux - pdx, uy - pdy);
        // Locate p through any of u's edges pointing back at it (reverse direction),
        // else through a neighbor — cheapest reliable way is the reverse edge; walk
        // symmetry (builder-asserted) guarantees it exists whenever p->u does. For
        // adjacent-macro-only arrivals the walk edge may be absent: prune nothing.
        let rev = dir_of(-pdx, -pdy).unwrap();
        let Some(p) = step(u, mask_u, rev) else {
            row[din] = mask_u;
            continue;
        };
        debug_assert_eq!(unpack_coord(coords[p]), (px, py, up));
        let mask_p = masks[p];

        let mut keep: u8 = 0;
        for c in 0..8 {
            if mask_u & (1 << c) == 0 {
                continue;
            }
            let (cdx, cdy) = DELTAS[c];
            let (nx, ny) = (ux + cdx, uy + cdy);
            if nx == px && ny == py {
                continue; // going straight back to the parent is always dominated
            }
            let through = STEP_COST[din] + STEP_COST[c];

            // Strictly cheaper local alternative from p to n avoiding u?
            let mut dominated = false;
            // Direct edge p -> n (n is 8-adjacent to p).
            if let Some(dd) = dir_of(nx - px, ny - py) {
                if mask_p & (1 << dd) != 0 && STEP_COST[dd] < through {
                    dominated = true;
                }
            }
            // Two-step detours p -> x -> n, x adjacent to both, x != u.
            if !dominated {
                for xd in 0..8 {
                    let (xdx, xdy) = DELTAS[xd];
                    let (xx, xy) = (px + xdx, py + xdy);
                    if (xx, xy) == (ux, uy) || ((xx - nx).abs() > 1 || (xy - ny).abs() > 1) {
                        continue;
                    }
                    let Some(x) = step(p, mask_p, xd) else { continue };
                    let Some(d2) = dir_of(nx - xx, ny - xy) else { continue }; // (x == n) excluded by direct case? x==n means dir_of(0,0)=None
                    if masks[x] & (1 << d2) != 0 && STEP_COST[xd] + STEP_COST[d2] < through {
                        dominated = true;
                        break;
                    }
                }
            }
            if !dominated {
                keep |= 1 << c;
            }
        }
        row[din] = keep;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::pack_coord;

    /// Build a grid from an explicit tile list with FULL mutual walk edges wherever
    /// both endpoints exist and `allow` says the (undirected) pair is connected.
    fn grid_from(
        tiles: &[(i32, i32)],
        allow: impl Fn((i32, i32), (i32, i32)) -> bool,
    ) -> (CanonicalGrid, Vec<u32>) {
        let mut coords: Vec<u32> = tiles.iter().map(|&(x, y)| pack_coord(x, y, 0)).collect();
        coords.sort_unstable();
        let pos = |x: i32, y: i32| -> Option<usize> {
            if x < 0 || y < 0 {
                return None;
            }
            coords.binary_search(&pack_coord(x, y, 0)).ok()
        };
        let n = coords.len();
        let mut offsets = vec![0u32; n + 1];
        let mut dst: Vec<u32> = Vec::new();
        for u in 0..n {
            let (ux, uy, _) = unpack_coord(coords[u]);
            for (d, &(dx, dy)) in DELTAS.iter().enumerate() {
                let _ = d;
                if let Some(v) = pos(ux + dx, uy + dy) {
                    if allow((ux, uy), (ux + dx, uy + dy)) {
                        dst.push(v as u32);
                    }
                }
            }
            offsets[u + 1] = dst.len() as u32;
        }
        let g = CanonicalGrid::build(n, &coords, &offsets, &dst, &[], &[], &[]).expect("build");
        (g, coords)
    }

    #[test]
    fn direction_mapping_roundtrips() {
        for (d, &(dx, dy)) in DELTAS.iter().enumerate() {
            assert_eq!(dir_of(dx, dy), Some(d));
        }
        assert_eq!(dir_of(0, 0), None);
        assert_eq!(dir_of(2, 0), None);
    }

    #[test]
    fn open_grid_prunes_dominated_successors() {
        // 5x5 fully open grid; inspect the center node (2,2).
        let tiles: Vec<(i32, i32)> = (0..5).flat_map(|x| (0..5).map(move |y| (x, y))).collect();
        let (g, coords) = grid_from(&tiles, |_, _| true);
        let center = coords.binary_search(&pack_coord(2, 2, 0)).unwrap();
        assert_eq!(g.masks[center], 0xFF);
        // Reached moving RIGHT (din = 2, parent at (1,2)): strict-domination keeps
        // straight (RIGHT) and the two forward diagonals (TOPRIGHT, BOTTOMRIGHT);
        // everything else has a strictly cheaper detour from the parent.
        let keep = g.succ[center * 8 + 2];
        assert_eq!(keep, (1 << 2) | (1 << 7) | (1 << 6), "keep={keep:#010b}");
        // Reached moving TOPRIGHT (din = 7, parent at (1,1)): keeps the diagonal and
        // its two components plus the tie detour targets (ties never pruned):
        // RIGHT, TOP, TOPRIGHT survive; others are strictly dominated.
        let keep = g.succ[center * 8 + 7];
        assert_eq!(keep, (1 << 2) | (1 << 3) | (1 << 7), "keep={keep:#010b}");
    }

    #[test]
    fn diamond_anomaly_keeps_both_detours() {
        // The measured trap shape: diagonals banned everywhere, both cardinal detours
        // open. From (0,0) moving RIGHT to (1,0), the step up to (1,1) must SURVIVE
        // (its only alternative via (0,1) is an equal-cost tie, never pruned).
        let tiles = [(0, 0), (1, 0), (0, 1), (1, 1)];
        let (g, coords) = grid_from(&tiles, |a, b| a.0 == b.0 || a.1 == b.1);
        let c10 = coords.binary_search(&pack_coord(1, 0, 0)).unwrap();
        let keep = g.succ[c10 * 8 + 2]; // reached via RIGHT from (0,0)
        assert_ne!(keep & (1 << 3), 0, "TOP successor to (1,1) must survive; keep={keep:#010b}");
        // And symmetrically at (0,1) reached via TOP.
        let c01 = coords.binary_search(&pack_coord(0, 1, 0)).unwrap();
        let keep = g.succ[c01 * 8 + 3];
        assert_ne!(keep & (1 << 2), 0, "RIGHT successor to (1,1) must survive; keep={keep:#010b}");
    }

    #[test]
    fn open_diagonal_dominates_detour() {
        // Fully open 2x2: from (0,0) RIGHT to (1,0), the step to (1,1) IS pruned —
        // the direct diagonal (0,0)->(1,1) is strictly cheaper (14 < 20).
        let tiles = [(0, 0), (1, 0), (0, 1), (1, 1)];
        let (g, coords) = grid_from(&tiles, |_, _| true);
        let c10 = coords.binary_search(&pack_coord(1, 0, 0)).unwrap();
        let keep = g.succ[c10 * 8 + 2];
        assert_eq!(keep & (1 << 3), 0, "TOP successor is strictly dominated; keep={keep:#010b}");
    }

    #[test]
    fn succ_bits_falls_back_for_non_adjacent_parents() {
        let tiles: Vec<(i32, i32)> = (0..3).flat_map(|x| (0..3).map(move |y| (x, y))).collect();
        let (g, coords) = grid_from(&tiles, |_, _| true);
        let center = coords.binary_search(&pack_coord(1, 1, 0)).unwrap() as u32;
        let corner = coords.binary_search(&pack_coord(0, 0, 0)).unwrap() as u32;
        // No parent -> full mask.
        assert_eq!(g.succ_bits(center, u32::MAX, &coords), g.masks[center as usize]);
        // Adjacent parent -> pruned set (strictly smaller on an open grid).
        let pruned = g.succ_bits(center, corner, &coords);
        assert!(pruned.count_ones() < g.masks[center as usize].count_ones());
    }
}
