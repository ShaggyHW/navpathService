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

/// Cardinal component directions of each diagonal (index d-4): (x-component, y-component).
const DIAG_COMPONENTS: [(usize, usize); 4] = [
    (0, 3), // (-1, 1): LEFT, TOP
    (0, 1), // (-1,-1): LEFT, BOTTOM
    (2, 1), // ( 1,-1): RIGHT, BOTTOM
    (2, 3), // ( 1, 1): RIGHT, TOP
];

/// Preference rank for equal-cost tie pruning: diagonals before cardinals, then by
/// direction index. Of two equal-cost two-step routes p->x->n and p->u->n the one
/// whose first step ranks lower is canonical; the other is pruned. A strict total
/// order, so two routes never prune each other, and every rewrite lowers the path's
/// direction sequence lexicographically, so the canonical form exists (terminates).
#[inline]
fn rank(d: usize) -> usize {
    if d >= 4 { d - 4 } else { d + 4 }
}

/// Node id of a packed coordinate (ids ascend with the packed key).
#[inline]
fn find_node(coords: &[u32], x: i32, y: i32, plane: i32) -> Option<usize> {
    if !(0..32768).contains(&x) || !(0..32768).contains(&y) {
        return None;
    }
    coords.binary_search(&crate::snapshot::pack_coord(x, y, plane)).ok()
}

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
    /// Stage 3 (jump-point expansion): the same table with EQUAL-cost alternatives
    /// pruned by a fixed preference (diagonal-first, then lower direction index), so
    /// every grid node has exactly one canonical optimal route pattern and straight
    /// runs can be jumped. Row `u*8 + din` as for `succ`. See [`CanonicalGrid::jump`].
    pub succ_jps: Vec<u8>,
    /// Nodes a jump must stop at because they carry non-grid edges (macro sources,
    /// fairy rings): they are only relaxed when expanded, so passing over them would
    /// lose their edges. A superset is always safe. Bitmap, 1 bit per node.
    pub stop: Vec<u64>,
    /// JPS+ tables (Harabor & Grastien 2014), row `u*8 + d`: `jump_dist` > 0 means a
    /// goal-independent jump point lies `dist` steps along `d` (its id in `jump_to`);
    /// < 0 means the run dead-ends after `|dist|` steps (last node in `jump_to`); 0 = no
    /// edge. Built by [`CanonicalGrid::build_jump_tables`] AFTER every stop node is
    /// known; empty tables make [`CanonicalGrid::jump`] fall back to walking.
    pub jump_dist: Vec<i16>,
    pub jump_to: Vec<u32>,
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
        let mut succ_jps = vec![0u8; nodes * 8];
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(16);
        let chunk = nodes.div_ceil(threads.max(1)).max(1);
        let masks_ref = &masks;
        std::thread::scope(|scope| {
            for ((ci, out), out_jps) in succ.chunks_mut(chunk * 8).enumerate().zip(succ_jps.chunks_mut(chunk * 8)) {
                let base = ci * chunk;
                scope.spawn(move || {
                    for ((local, row), row_jps) in out.chunks_mut(8).enumerate().zip(out_jps.chunks_mut(8)) {
                        let u = base + local;
                        fill_succ_row(u, coords, walk_offsets, walk_dst, masks_ref, row, row_jps);
                    }
                });
            }
        });

        let mut stop = vec![0u64; nodes.div_ceil(64)];
        for &src in macro_src {
            let i = src as usize;
            if i < nodes {
                stop[i >> 6] |= 1 << (i & 63);
            }
        }

        Ok(CanonicalGrid { masks, succ, succ_jps, stop, jump_dist: Vec::new(), jump_to: Vec::new() })
    }

    /// Precompute the JPS+ jump tables from `succ_jps` and the stop bitmap. Each
    /// direction is one linear pass in an order where the neighbour along `d` has
    /// already been resolved (node ids ascend with (plane, y, x), so directions with
    /// +y or (+x, y=0) are processed in descending id order, the rest ascending);
    /// cardinals first because a diagonal step is a jump point when a cardinal
    /// sub-run from it reaches one. Call after [`CanonicalGrid::add_stop_nodes`].
    pub fn build_jump_tables(&mut self, offsets: &[u32], dst: &[u32]) {
        let n = self.masks.len();
        let mut jd = vec![0i16; n * 8];
        let mut jt = vec![0u32; n * 8];
        // Cardinals first, then diagonals; descending for +y / +x directions.
        const ORDER: [(usize, bool); 8] = [(2, true), (3, true), (0, false), (1, false), (7, true), (4, true), (5, false), (6, false)];
        for (d, descending) in ORDER {
            let natural: u8 = if d < 4 {
                1u8 << d
            } else {
                let (c1, c2) = DIAG_COMPONENTS[d - 4];
                (1u8 << d) | (1u8 << c1) | (1u8 << c2)
            };
            let mut u = if descending { n } else { 0 };
            for _ in 0..n {
                if descending { u -= 1; }
                let Some(v) = self.step(u, d, offsets, dst) else {
                    jd[u * 8 + d] = 0;
                    jt[u * 8 + d] = u as u32;
                    if !descending { u += 1; }
                    continue;
                };
                let sv = self.succ_jps[v * 8 + d];
                let mut is_jp = self.is_stop(v) || (sv & !natural) != 0;
                if !is_jp && d >= 4 {
                    let (c1, c2) = DIAG_COMPONENTS[d - 4];
                    is_jp = (sv & (1 << c1) != 0 && jd[v * 8 + c1] > 0) || (sv & (1 << c2) != 0 && jd[v * 8 + c2] > 0);
                }
                let (dist, to) = if is_jp {
                    (1i16, v as u32)
                } else if sv & (1 << d) == 0 {
                    (-1i16, v as u32)
                } else {
                    let dv = jd[v * 8 + d];
                    debug_assert!(dv != 0, "continuable run must have a resolved successor");
                    (if dv > 0 { dv.saturating_add(1) } else { dv.saturating_sub(1) }, jt[v * 8 + d])
                };
                jd[u * 8 + d] = dist;
                jt[u * 8 + d] = to;
                if !descending { u += 1; }
            }
        }
        self.jump_dist = jd;
        self.jump_to = jt;
    }

    /// O(1) jump via the JPS+ tables (goal handled at query time: a goal lying on the
    /// run, or — for diagonals — reachable by a straight cardinal run from a node on
    /// the run, stops the jump there). Falls back to [`CanonicalGrid::jump_walk`]
    /// when the tables have not been built. Same contract as `jump_walk`.
    #[inline]
    pub fn jump(&self, u: usize, d: usize, goal: usize, offsets: &[u32], dst: &[u32], coords: &[u32]) -> Option<(usize, u32)> {
        if self.jump_dist.is_empty() {
            return self.jump_walk(u, d, goal, offsets, dst);
        }
        let k = self.jump_dist[u * 8 + d];
        if k == 0 {
            return None;
        }
        let n = k.unsigned_abs() as u32;
        let (ux, uy, up) = unpack_coord(coords[u]);
        let (gx, gy, gp) = unpack_coord(coords[goal]);
        if up == gp {
            let (dx, dy) = DELTAS[d];
            let (ex, ey) = (gx - ux, gy - uy);
            if d < 4 {
                let t = if dx != 0 { if ey == 0 { ex * dx } else { 0 } } else if ex == 0 { ey * dy } else { 0 };
                if t >= 1 && t as u32 <= n {
                    return Some((goal, t as u32));
                }
            } else {
                let (ix, iy) = (ex * dx, ey * dy);
                if ix == iy && ix >= 1 && ix as u32 <= n {
                    return Some((goal, ix as u32));
                }
                let (c1, c2) = DIAG_COMPONENTS[d - 4];
                let mut best: Option<u32> = None;
                // Goal's row is reached at step iy; then a horizontal run of `rem`.
                if iy >= 1 && (iy as u32) <= n {
                    let rem = (ex - iy * dx) * dx;
                    if rem >= 1 {
                        if let Some(ni) = find_node(coords, ux + iy * dx, uy + iy * dy, up) {
                            if rem as u32 <= self.jump_dist[ni * 8 + c1].unsigned_abs() as u32 {
                                best = Some(iy as u32);
                            }
                        }
                    }
                }
                if ix >= 1 && (ix as u32) <= n {
                    let rem = (ey - ix * dy) * dy;
                    if rem >= 1 && best.map_or(true, |b| (ix as u32) < b) {
                        if let Some(ni) = find_node(coords, ux + ix * dx, uy + ix * dy, up) {
                            if rem as u32 <= self.jump_dist[ni * 8 + c2].unsigned_abs() as u32 {
                                best = Some(ix as u32);
                            }
                        }
                    }
                }
                if let Some(i) = best {
                    let ni = find_node(coords, ux + i as i32 * dx, uy + i as i32 * dy, up).expect("node on the run");
                    return Some((ni, i));
                }
            }
        }
        if k > 0 { Some((self.jump_to[u * 8 + d] as usize, n)) } else { None }
    }

    /// Mark extra nodes a jump must stop at (fairy ring nodes; any superset is safe).
    pub fn add_stop_nodes(&mut self, nodes: &[u32]) {
        for &n in nodes {
            let i = n as usize;
            if i < self.masks.len() {
                self.stop[i >> 6] |= 1 << (i & 63);
            }
        }
    }

    #[inline(always)]
    pub fn is_stop(&self, node: usize) -> bool {
        (self.stop[node >> 6] >> (node & 63)) & 1 != 0
    }

    /// Tie-pruned successor bits for `node` entered via direction code `dcode`
    /// (0 = no grid parent: seeds, teleport/macro arrivals => full mask; else
    /// direction index + 1, as stored in `SearchContext::dirs`).
    #[inline(always)]
    pub fn jps_succ(&self, node: usize, dcode: u8) -> u8 {
        if dcode == 0 {
            self.masks[node]
        } else {
            self.succ_jps[node * 8 + (dcode as usize - 1)]
        }
    }

    /// Neighbour of `u` in direction `d`, if that walk edge exists (O(1) CSR slot via
    /// the ascending direction-bit row invariant).
    #[inline(always)]
    pub fn step(&self, u: usize, d: usize, offsets: &[u32], dst: &[u32]) -> Option<usize> {
        let m = self.masks[u];
        if m & (1 << d) == 0 {
            return None;
        }
        Some(dst[offsets[u] as usize + (m & ((1u8 << d) - 1)).count_ones() as usize] as usize)
    }

    /// Jump from `u` in direction `d` (Harabor & Grastien's JPS on the tie-pruned
    /// canonical ordering): follow the straight run until the goal, a stop node
    /// (non-grid edges), a node with a forced successor (a `succ_jps` bit outside the
    /// natural continuation), or — for diagonal runs — a node whose cardinal sub-jumps
    /// find a jump point. Returns the jump point and the number of steps, or None if
    /// the run dead-ends. Every node on an optimal path has an optimal canonical path
    /// that only turns at jump points, so relaxing jump points alone is cost-exact.
    pub fn jump_walk(&self, u: usize, d: usize, goal: usize, offsets: &[u32], dst: &[u32]) -> Option<(usize, u32)> {
        let mut cur = u;
        let mut k: u32 = 0;
        if d < 4 {
            let natural = 1u8 << d;
            loop {
                let next = self.step(cur, d, offsets, dst)?;
                k += 1;
                if next == goal || self.is_stop(next) {
                    return Some((next, k));
                }
                let s = self.succ_jps[next * 8 + d];
                if s & !natural != 0 {
                    return Some((next, k));
                }
                if s & natural == 0 {
                    return None;
                }
                cur = next;
            }
        } else {
            let (c1, c2) = DIAG_COMPONENTS[d - 4];
            let natural = (1u8 << d) | (1u8 << c1) | (1u8 << c2);
            loop {
                let next = self.step(cur, d, offsets, dst)?;
                k += 1;
                if next == goal || self.is_stop(next) {
                    return Some((next, k));
                }
                let s = self.succ_jps[next * 8 + d];
                if s & !natural != 0 {
                    return Some((next, k));
                }
                if s & (1 << c1) != 0 && self.jump_walk(next, c1, goal, offsets, dst).is_some() {
                    return Some((next, k));
                }
                if s & (1 << c2) != 0 && self.jump_walk(next, c2, goal, offsets, dst).is_some() {
                    return Some((next, k));
                }
                if s & (1 << d) == 0 {
                    return None;
                }
                cur = next;
            }
        }
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
    row_jps: &mut [u8],
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
        let (pdx, pdy) = DELTAS[din];
        let (px, py) = (ux - pdx, uy - pdy);
        let rev = dir_of(-pdx, -pdy).unwrap();
        let Some(p) = step(u, mask_u, rev) else {
            row[din] = mask_u;
            row_jps[din] = mask_u;
            continue;
        };
        debug_assert_eq!(unpack_coord(coords[p]), (px, py, up));
        let mask_p = masks[p];

        let mut keep: u8 = 0;
        let mut keep_jps: u8 = 0;
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

            // `dominated` (Stage 2a, cost-exact): an alternative p->n route that is
            // STRICTLY cheaper exists. `dominated_jps` (Stage 3): strictly cheaper, or
            // equal-cost and canonically preferred (lower first-step rank than `din`).
            let mut dominated = false;
            let mut dominated_jps = false;
            if let Some(dd) = dir_of(nx - px, ny - py) {
                if mask_p & (1 << dd) != 0 && STEP_COST[dd] < through {
                    dominated = true;
                    dominated_jps = true;
                }
            }
            if !dominated_jps {
                for xd in 0..8 {
                    let (xdx, xdy) = DELTAS[xd];
                    let (xx, xy) = (px + xdx, py + xdy);
                    if (xx, xy) == (ux, uy) || ((xx - nx).abs() > 1 || (xy - ny).abs() > 1) {
                        continue;
                    }
                    let Some(x) = step(p, mask_p, xd) else { continue };
                    let Some(d2) = dir_of(nx - xx, ny - xy) else { continue };
                    if masks[x] & (1 << d2) == 0 {
                        continue;
                    }
                    let alt = STEP_COST[xd] + STEP_COST[d2];
                    if alt < through {
                        dominated = true;
                        dominated_jps = true;
                        break;
                    }
                    if alt == through && rank(xd) < rank(din) {
                        dominated_jps = true;
                    }
                }
            }
            if !dominated {
                keep |= 1 << c;
            }
            if !dominated_jps {
                keep_jps |= 1 << c;
            }
        }
        row[din] = keep;
        row_jps[din] = keep_jps;
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

    /// Rebuild the CSR the same way `grid_from` does (needed to call the jump API).
    fn csr_of(coords: &[u32], allow: impl Fn((i32, i32), (i32, i32)) -> bool) -> (Vec<u32>, Vec<u32>) {
        let pos = |x: i32, y: i32| -> Option<usize> {
            if x < 0 || y < 0 { return None; }
            coords.binary_search(&pack_coord(x, y, 0)).ok()
        };
        let n = coords.len();
        let mut offsets = vec![0u32; n + 1];
        let mut dst: Vec<u32> = Vec::new();
        for u in 0..n {
            let (ux, uy, _) = unpack_coord(coords[u]);
            for &(dx, dy) in DELTAS.iter() {
                if let Some(v) = pos(ux + dx, uy + dy) {
                    if allow((ux, uy), (ux + dx, uy + dy)) { dst.push(v as u32); }
                }
            }
            offsets[u + 1] = dst.len() as u32;
        }
        (offsets, dst)
    }

    #[test]
    fn jump_tables_agree_with_walking_jumps() {
        // 14x14 grid with a wall segment, a pillar, a notch and two stop nodes.
        let blocked = |x: i32, y: i32| (y == 6 && (3..=9).contains(&x)) || (x == 11 && (2..=4).contains(&y)) || (x == 5 && y == 10);
        let tiles: Vec<(i32, i32)> = (0..14).flat_map(|x| (0..14).map(move |y| (x, y))).filter(|&(x, y)| !blocked(x, y)).collect();
        // Disallow diagonal edges that cut a blocked corner (both flanks must be open).
        let allow = move |a: (i32, i32), b: (i32, i32)| {
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            if dx != 0 && dy != 0 { !blocked(a.0 + dx, a.1) && !blocked(a.0, a.1 + dy) } else { true }
        };
        let (mut g, coords) = grid_from(&tiles, allow);
        let (offsets, dst) = csr_of(&coords, allow);
        let n = coords.len();
        g.add_stop_nodes(&[coords.binary_search(&pack_coord(2, 2, 0)).unwrap() as u32, coords.binary_search(&pack_coord(8, 12, 0)).unwrap() as u32]);
        g.build_jump_tables(&offsets, &dst);
        assert_eq!(g.jump_dist.len(), n * 8);
        let mut checked = 0usize;
        for u in 0..n {
            for d in 0..8 {
                for goal in 0..n {
                    let a = g.jump_walk(u, d, goal, &offsets, &dst);
                    let b = g.jump(u, d, goal, &offsets, &dst, &coords);
                    assert_eq!(a, b, "u={u} d={d} goal={goal}: walk {a:?} vs table {b:?}");
                    checked += 1;
                }
            }
        }
        assert!(checked > 100_000);
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
