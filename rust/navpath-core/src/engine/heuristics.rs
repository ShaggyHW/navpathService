use crate::snapshot::{ALT_QUANTUM_MS, ALT_SATURATED, ALT_UNREACHABLE};

/// Number of landmarks evaluated per heuristic call. At the start of each query the best
/// `ACTIVE_LANDMARKS` landmarks for the (start, goal) pair are selected and only those are
/// evaluated during the search. The max over a subset of admissible, consistent landmark
/// lower bounds is still admissible and consistent, so optimal path cost is preserved;
/// the heuristic is only marginally weaker, in exchange for far cheaper per-node cost.
pub const ACTIVE_LANDMARKS: usize = 8;

/// Landmark (ALT) heuristic backed by the memory-mapped quantized distance table.
///
/// The table is **node-major and interleaved**: node `n`'s row occupies
/// `[n * 2 * landmarks, (n + 1) * 2 * landmarks)` as `[fw(l0), bw(l0), fw(l1), bw(l1), …]`
/// u16 quanta of [`ALT_QUANTUM_MS`], so one heuristic call reads one contiguous row.
/// [`ALT_UNREACHABLE`] marks unreachable pairs.
pub struct LandmarkHeuristic<'a> {
    pub nodes: usize,
    pub landmarks: usize,
    pub tab: &'a [u16],
}

/// Per-query landmark selection produced by [`LandmarkHeuristic::select_active`].
///
/// Holds the chosen landmark column indices plus the goal's forward/backward quanta for
/// those landmarks, so the goal row is read once per query rather than on every heuristic
/// evaluation.
#[derive(Default)]
pub struct ActiveLandmarks {
    /// Total landmark count (row stride = 2 * landmarks for node-major indexing).
    pub landmarks: usize,
    /// Selected landmark column indices.
    pub indices: Vec<usize>,
    /// `fw[goal, li]` quanta for each selected landmark (parallel to `indices`).
    pub goal_fw: Vec<u16>,
    /// `bw[goal, li]` quanta for each selected landmark (parallel to `indices`).
    pub goal_bw: Vec<u16>,
}

impl<'a> LandmarkHeuristic<'a> {
    /// Select the best `k` landmarks for the (start, goal) pair and cache the goal's row for
    /// those landmarks. Landmarks are scored by the lower bound they yield at the start node
    /// (the tightest bounds for this query); ties break by landmark index for determinism.
    ///
    /// Only the GOAL entries must be finite: with finite goal rows, a node whose own entry
    /// is unreachable yields either an ignored negative term or an infinite bound — and the
    /// infinite case is provably correct, because `d(u,L) <= d(u,goal) + d(goal,L)` means a
    /// node that can reach the goal can also reach every landmark the goal reaches. An
    /// unreachable START entry therefore scores the landmark as maximally useful (the
    /// start's whole unreachable-to-goal region gets pruned). Landmarks with unreachable
    /// goal entries are excluded — they cannot produce valid bounds.
    pub fn select_active(&self, start: u32, goal: u32, k: usize) -> ActiveLandmarks {
        let l = self.landmarks;
        if l == 0 || self.nodes == 0 || self.tab.is_empty() {
            return ActiveLandmarks::default();
        }
        let stride = 2 * l;
        let sb = start as usize * stride;
        let gb = goal as usize * stride;

        let mut scored: Vec<(i64, usize)> = Vec::with_capacity(l);
        for li in 0..l {
            let gfw = self.tab[gb + 2 * li];
            let gbw = self.tab[gb + 2 * li + 1];
            // Saturated goal entries are as unusable as unreachable ones: an understated
            // d(goal,L) overstates the backward bound (b = bu - gbw), and an understated
            // d(L,goal) just weakens the forward one. Drop the landmark for this query.
            if gfw >= ALT_SATURATED || gbw >= ALT_SATURATED {
                continue;
            }
            let sfw = self.tab[sb + 2 * li];
            let sbw = self.tab[sb + 2 * li + 1];
            // Score in quanta; unreachable start entries score as "infinitely useful".
            // A saturated forward entry understates d(L,start) and would overstate the
            // bound, so that side is unusable (mirrors h_active).
            let a = if sfw >= ALT_SATURATED { i64::MIN } else { gfw as i64 - sfw as i64 };
            let b = if sbw == ALT_UNREACHABLE { i64::MAX } else { sbw as i64 - gbw as i64 };
            let v = a.max(b).max(0);
            scored.push((v, li));
        }
        // Descending by bound, then ascending by index for a deterministic selection.
        scored.sort_unstable_by(|x, y| y.0.cmp(&x.0).then_with(|| x.1.cmp(&y.1)));

        let k = k.min(scored.len());
        let mut active = ActiveLandmarks {
            landmarks: l,
            indices: Vec::with_capacity(k),
            goal_fw: Vec::with_capacity(k),
            goal_bw: Vec::with_capacity(k),
        };
        for &(_, li) in scored.iter().take(k) {
            active.indices.push(li);
            active.goal_fw.push(self.tab[gb + 2 * li]);
            active.goal_bw.push(self.tab[gb + 2 * li + 1]);
        }
        active
    }

    /// Heuristic over the selected active landmarks, reusing the cached goal row. Reads
    /// one contiguous interleaved row of the active node.
    ///
    /// Returns milliseconds. One quantum is subtracted from the max bound to compensate
    /// floor-quantization (keeps the bound admissible). `f32::INFINITY` is returned when
    /// the node provably cannot reach the goal (see `select_active`).
    #[inline]
    pub fn h_active(&self, u: u32, active: &ActiveLandmarks) -> f32 {
        if active.indices.is_empty() {
            return 0.0;
        }
        let stride = 2 * active.landmarks;
        let ub = u as usize * stride;
        let row = &self.tab[ub..ub + stride];
        let mut best: i64 = 0;
        for i in 0..active.indices.len() {
            let li = active.indices[i];
            let fu = row[2 * li];
            let bu = row[2 * li + 1];
            if bu == ALT_UNREACHABLE {
                // u cannot reach this landmark, but the goal can (select_active
                // guarantees finite goal entries), so u cannot reach the goal.
                return f32::INFINITY;
            }
            // A SATURATED bu still yields a valid (understated) bound: d(u,L) really is
            // at least SATURATED quanta.
            let b = bu as i64 - active.goal_bw[i] as i64;
            if b > best { best = b; }
            // The forward side is only valid when fu is exact: a saturated fu
            // understates d(L,u), which would OVERstate this bound.
            if fu < ALT_SATURATED {
                let a = active.goal_fw[i] as i64 - fu as i64;
                if a > best { best = a; }
            }
        }
        ((best - 1).max(0) as f32) * ALT_QUANTUM_MS
    }
}

/// Quantize a millisecond distance for the v8 ALT table: floor to quanta, saturating,
/// with non-finite mapped to [`ALT_UNREACHABLE`].
pub fn quantize_alt_ms(ms: f32) -> u16 {
    if !ms.is_finite() {
        return ALT_UNREACHABLE;
    }
    let q = (ms / ALT_QUANTUM_MS).floor();
    if q >= ALT_SATURATED as f32 {
        ALT_SATURATED
    } else if q <= 0.0 {
        0
    } else {
        q as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_and_bound() {
        assert_eq!(quantize_alt_ms(f32::INFINITY), ALT_UNREACHABLE);
        assert_eq!(quantize_alt_ms(0.0), 0);
        assert_eq!(quantize_alt_ms(63.9), 0);
        assert_eq!(quantize_alt_ms(64.0), 1);
        assert_eq!(quantize_alt_ms(1e12), ALT_SATURATED);

        // One landmark, two nodes: d(L,u)=0, d(L,g)=6400ms (100 quanta).
        // Bound at u toward g must be <= 6400 and >= 6400 - 2 quanta.
        let tab: Vec<u16> = vec![
            0, 0,      // node 0 (== landmark)
            100, 100,  // node 1 (goal)
        ];
        let lm = LandmarkHeuristic { nodes: 2, landmarks: 1, tab: &tab };
        let active = lm.select_active(0, 1, 8);
        assert_eq!(active.indices, vec![0]);
        let h = lm.h_active(0, &active);
        assert!(h <= 6400.0 && h >= 6400.0 - 2.0 * ALT_QUANTUM_MS, "h={h}");
        // goal itself: zero-ish bound
        assert!(lm.h_active(1, &active) <= ALT_QUANTUM_MS);
    }

    #[test]
    fn saturated_goal_entries_disqualify_landmark() {
        // Goal's backward entry saturated: using it would overstate bounds; the landmark
        // must not be selected for this query.
        let tab: Vec<u16> = vec![
            0, 0,
            ALT_SATURATED, ALT_SATURATED,
        ];
        let lm = LandmarkHeuristic { nodes: 2, landmarks: 1, tab: &tab };
        let active = lm.select_active(0, 1, 8);
        assert!(active.indices.is_empty());
    }

    #[test]
    fn unreachable_node_gets_infinite_bound() {
        // node 2 cannot reach the landmark; goal (node 1) can.
        let tab: Vec<u16> = vec![
            0, 0,
            100, 100,
            ALT_UNREACHABLE, ALT_UNREACHABLE,
        ];
        let lm = LandmarkHeuristic { nodes: 3, landmarks: 1, tab: &tab };
        let active = lm.select_active(0, 1, 8);
        assert!(lm.h_active(2, &active).is_infinite());
    }
}
