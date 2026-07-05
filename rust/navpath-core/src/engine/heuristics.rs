use crate::snapshot::LeSliceF32;

pub trait OctileCoords {
    fn coords(&self, node: u32) -> (i32, i32, i32);
}

/// Number of landmarks evaluated per heuristic call. At the start of each query the best
/// `ACTIVE_LANDMARKS` landmarks for the (start, goal) pair are selected and only those are
/// evaluated during the search. The max over a subset of admissible, consistent landmark
/// lower bounds is still admissible and consistent, so optimal path cost is preserved;
/// the heuristic is only marginally weaker, in exchange for far cheaper per-node cost.
pub const ACTIVE_LANDMARKS: usize = 8;

/// Landmark (ALT) heuristic backed by the memory-mapped distance tables.
///
/// The tables are stored **node-major**: the distances for node `n` occupy the contiguous
/// range `[n * landmarks, n * landmarks + landmarks)`. This keeps all of a node's landmark
/// values within a few cache lines, unlike the old landmark-major layout where consecutive
/// landmark values for a node were `nodes` entries apart.
pub struct LandmarkHeuristic<'a> {
    pub nodes: usize,
    pub landmarks: usize,
    pub lm_fw: LeSliceF32<'a>,
    pub lm_bw: LeSliceF32<'a>,
}

/// Per-query landmark selection produced by [`LandmarkHeuristic::select_active`].
///
/// Holds the chosen landmark column indices plus the goal's forward/backward distances for
/// those landmarks, so the goal row is read once per query rather than on every heuristic
/// evaluation.
#[derive(Default)]
pub struct ActiveLandmarks {
    /// Total landmark count (row stride for node-major indexing).
    pub landmarks: usize,
    /// Selected landmark column indices.
    pub indices: Vec<usize>,
    /// `lm_fw[goal, li]` for each selected landmark `li` (parallel to `indices`).
    pub goal_fw: Vec<f32>,
    /// `lm_bw[goal, li]` for each selected landmark `li` (parallel to `indices`).
    pub goal_bw: Vec<f32>,
}

impl<'a> LandmarkHeuristic<'a> {
    /// Full heuristic over **all** landmarks. Retained for reference and tests; the search
    /// hot path uses [`Self::h_active`].
    pub fn h(&self, u: u32, goal: u32) -> f32 {
        if self.landmarks == 0 || self.nodes == 0 {
            return 0.0;
        }
        let ub = u as usize * self.landmarks;
        let gb = goal as usize * self.landmarks;
        let mut best = 0.0f32;
        for li in 0..self.landmarks {
            let a = self.lm_fw.get(gb + li).unwrap_or(0.0) - self.lm_fw.get(ub + li).unwrap_or(0.0);
            let b = self.lm_bw.get(ub + li).unwrap_or(0.0) - self.lm_bw.get(gb + li).unwrap_or(0.0);
            let v = if a > b { a } else { b };
            if v > best { best = v; }
        }
        best
    }

    /// Select the best `k` landmarks for the (start, goal) pair and cache the goal's row for
    /// those landmarks. Landmarks are scored by the lower bound they yield at the start node
    /// (the tightest bounds for this query); ties break by landmark index for determinism.
    pub fn select_active(&self, start: u32, goal: u32, k: usize) -> ActiveLandmarks {
        let l = self.landmarks;
        if l == 0 || self.nodes == 0 {
            return ActiveLandmarks::default();
        }
        let sb = start as usize * l;
        let gb = goal as usize * l;

        let mut scored: Vec<(f32, usize)> = Vec::with_capacity(l);
        for li in 0..l {
            let gfw = self.lm_fw.get(gb + li).unwrap_or(0.0);
            let sfw = self.lm_fw.get(sb + li).unwrap_or(0.0);
            let gbw = self.lm_bw.get(gb + li).unwrap_or(0.0);
            let sbw = self.lm_bw.get(sb + li).unwrap_or(0.0);
            let a = gfw - sfw;
            let b = sbw - gbw;
            let v = a.max(b).max(0.0);
            scored.push((v, li));
        }
        // Descending by bound, then ascending by index for a deterministic selection.
        scored.sort_unstable_by(|x, y| {
            y.0.partial_cmp(&x.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| x.1.cmp(&y.1))
        });

        let k = k.min(l);
        let mut active = ActiveLandmarks {
            landmarks: l,
            indices: Vec::with_capacity(k),
            goal_fw: Vec::with_capacity(k),
            goal_bw: Vec::with_capacity(k),
        };
        for &(_, li) in scored.iter().take(k) {
            active.indices.push(li);
            active.goal_fw.push(self.lm_fw.get(gb + li).unwrap_or(0.0));
            active.goal_bw.push(self.lm_bw.get(gb + li).unwrap_or(0.0));
        }
        active
    }

    /// Heuristic over the selected active landmarks, reusing the cached goal row. Only the
    /// active node's contiguous landmark rows are read.
    #[inline]
    pub fn h_active(&self, u: u32, active: &ActiveLandmarks) -> f32 {
        if active.indices.is_empty() {
            return 0.0;
        }
        let ub = u as usize * active.landmarks;
        let mut best = 0.0f32;
        for i in 0..active.indices.len() {
            let li = active.indices[i];
            let fu = self.lm_fw.get(ub + li).unwrap_or(0.0);
            let bu = self.lm_bw.get(ub + li).unwrap_or(0.0);
            let a = active.goal_fw[i] - fu;
            let b = bu - active.goal_bw[i];
            let v = if a > b { a } else { b };
            if v > best { best = v; }
        }
        best
    }
}

pub fn octile<C: OctileCoords>(c: &C, a: u32, b: u32) -> f32 {
    let (ax, ay, ap) = c.coords(a);
    let (bx, by, bp) = c.coords(b);
    if ap != bp { return 0.0; }
    let dx = (ax - bx).abs() as f32;
    let dy = (ay - by).abs() as f32;
    let dmin = if dx < dy { dx } else { dy };
    let dmax = if dx > dy { dx } else { dy };
    dmin * 2_f32.sqrt() + (dmax - dmin)
}
