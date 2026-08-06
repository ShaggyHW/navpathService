use crate::snapshot::{ALT_QUANTUM_MS, ALT_SATURATED, ALT_UNREACHABLE};

/// Default number of landmarks evaluated per heuristic call: ALL of them.
///
/// The max over any subset of admissible landmark bounds is admissible, so this only
/// tightens the heuristic. Historically 8 were selected per (start, goal) pair to save
/// compute, but a node's interleaved row is 4 cache lines and 8 scattered pairs already
/// touch ~3.6 of them — full-width evaluation is nearly memory-free, and MEASURED
/// (2026-07-14, deployed 64-landmark snapshot): 53-67% fewer pops on the incident-class
/// long/gated routes and ~60% lower wall time on the differential corpus, because every
/// avoided pop was itself a DRAM row gather. Point selection is also provably degenerate
/// for multi-source virtual starts (all scores tie at 0); full width has no selection
/// step to degenerate. Override with NAVPATH_ACTIVE_LANDMARKS for A/B runs.
pub const ACTIVE_LANDMARKS: usize = usize::MAX;

/// Active-landmark count from `NAVPATH_ACTIVE_LANDMARKS` (default [`ACTIVE_LANDMARKS`]).
pub fn active_landmarks() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("NAVPATH_ACTIVE_LANDMARKS").ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(ACTIVE_LANDMARKS)
    })
}

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
    /// Quantum (ms) the table was built with — from the snapshot header, NOT the
    /// compiled constant (a stale binary must still read new snapshots correctly).
    pub quantum: f32,
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
    /// Full-width branchless operands, present when every valid landmark is selected
    /// (the default since roadmap 3.1). See [`FullRowOperands`].
    pub full: Option<FullRowOperands>,
}

/// Precomputed per-query operand lanes for the branchless full-row heuristic. Lane
/// layout mirrors the interleaved table row `[fw(l0), bw(l0), fw(l1), bw(l1), …]`:
///
/// - `ga`: even lanes hold `goal_fw` for VALID landmarks (0 otherwise/odd) — so
///   `ga.saturating_sub(row)` yields `max(0, gfw − fu)` on even lanes, with the
///   forward saturation rule free of charge (`fu >= SATURATED` saturates to 0, and
///   invalid landmarks contribute the max-neutral 0).
/// - `gb`: odd lanes hold `goal_bw` for valid landmarks (0xFFFF otherwise/even) — so
///   `row.saturating_sub(gb)` yields `max(0, bu − gbw)` on odd lanes (a SATURATED
///   `bu` still gives the valid understated bound, exactly like the scalar rule).
/// - `inf_odd`: 0xFFFF on valid odd lanes — `row == 0xFFFF` there proves the node
///   cannot reach the goal (the scalar early-INF return).
///
/// The lane-wise max over `ga⊖row` and `row⊖gb` equals the scalar loop's `best`
/// integer exactly, so results are bit-identical; the whole pass is fixed-trip
/// u16 saturating arithmetic that LLVM autovectorizes under target-cpu=native.
///
/// `c`/`valid_odd` are the same operands folded into TWO streams for the explicit
/// AVX-512 path ([`h_full_row_avx512`]); they carry no new information, so both paths
/// return bit-identical values.
pub struct FullRowOperands {
    ga: Vec<u16>,
    gb: Vec<u16>,
    inf_odd: Vec<u16>,
    /// `ga` and `gb` merged: even lanes hold `goal_fw`, odd lanes `goal_bw` (0 / 0xFFFF
    /// on the lanes of invalid landmarks — the same neutral values `ga`/`gb` use).
    c: Vec<u16>,
    /// One bit per row lane (32 lanes per word): set on the odd lanes of VALID
    /// landmarks. Replaces the `inf_odd` operand stream with a k-mask operand.
    valid_odd: Vec<u32>,
}

/// Is the explicit AVX-512 full-row path available? Cached CPU detection plus the
/// `NAVPATH_H_SIMD=0` kill switch (falls back to the portable autovectorized loop,
/// which returns identical values).
#[cfg(target_arch = "x86_64")]
fn avx512_full_row() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("NAVPATH_H_SIMD").ok().as_deref() != Some("0")
            && std::is_x86_feature_detected!("avx512f")
            && std::is_x86_feature_detected!("avx512bw")
            && std::is_x86_feature_detected!("avx2")
            && std::is_x86_feature_detected!("sse4.1")
    })
}

/// Portable full-width row pass: one branchless fixed-trip loop over the interleaved
/// row that LLVM autovectorizes under target-cpu=native. See [`FullRowOperands`].
#[inline]
fn h_full_row_portable(row: &[u16], full: &FullRowOperands, quantum: f32) -> f32 {
    let stride = row.len();
    // Equal-length slice bindings so LLVM can hoist the bounds checks and
    // vectorize the fixed-trip u16 loop (psubusw/pmaxuw/pcmpeqw); indexing the
    // four Vecs directly defeated autovectorization (measured 3.4x slower).
    let ga = &full.ga[..stride];
    let gbv = &full.gb[..stride];
    let io = &full.inf_odd[..stride];
    let row = &row[..stride];
    let mut best: u16 = 0;
    let mut inf: u16 = 0;
    for i in 0..stride {
        let r = row[i];
        let a = ga[i].saturating_sub(r);
        let b = r.saturating_sub(gbv[i]);
        best = best.max(a.max(b));
        // Branchless: 0xFFFF where r == UNREACHABLE, masked to valid odd lanes.
        let m = ((r == ALT_UNREACHABLE) as u16).wrapping_neg();
        inf |= m & io[i];
    }
    if inf != 0 {
        // u cannot reach a landmark the goal reaches → u cannot reach the goal.
        return f32::INFINITY;
    }
    ((best as i64 - 1).max(0) as f32) * quantum
}

/// The same pass in explicit AVX-512, reading TWO operand streams (the node's row and
/// the merged goal vector) instead of the portable path's four — measured 8.7 → 3.6 ns
/// per node warm, 10.8 → 4.3 ns on random rows, bit-identical on the deployed table.
///
/// Even lanes hold `c = goal_fw`, odd lanes `c = goal_bw`, so `subs(c, row)` is the
/// forward term and `subs(row, c)` the backward one; a constant lane-parity blend picks
/// the right term per lane, which is exactly the portable `max(ga⊖row, row⊖gb)` (the
/// wrong-parity term is 0 by construction there, and the neutral operands of invalid
/// landmarks are identical here). The INF test folds `inf_odd` into a per-query k-mask.
///
/// # Safety
/// The caller must have verified the target features via [`avx512_full_row`] and pass
/// `row.len() == c.len()` a multiple of 32, with `valid_odd.len() >= row.len() / 32`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx2,sse4.1")]
unsafe fn h_full_row_avx512(row: &[u16], c: &[u16], valid_odd: &[u32], quantum: f32) -> f32 {
    use core::arch::x86_64::*;
    // Odd (backward) lanes of a 32 x u16 register.
    const ODD: u32 = 0xAAAA_AAAA;
    let ones = _mm512_set1_epi16(-1i16); // 0xFFFF
    let mut acc = _mm512_setzero_si512();
    let mut inf: u32 = 0;
    let mut j = 0usize;
    while j < row.len() {
        let r = _mm512_loadu_si512(row.as_ptr().add(j) as *const _);
        let cv = _mm512_loadu_si512(c.as_ptr().add(j) as *const _);
        let fwd = _mm512_subs_epu16(cv, r);
        let bwd = _mm512_subs_epu16(r, cv);
        acc = _mm512_max_epu16(acc, _mm512_mask_blend_epi16(ODD, fwd, bwd));
        inf |= _mm512_cmpeq_epu16_mask(r, ones) & valid_odd[j / 32];
        j += 32;
    }
    if inf != 0 {
        return f32::INFINITY;
    }
    // Horizontal max over 32 u16 lanes: fold to 128 bits, then `minpos` on the
    // complement (x86 has no horizontal max for u16).
    let m256 = _mm256_max_epu16(
        _mm512_extracti64x4_epi64(acc, 0),
        _mm512_extracti64x4_epi64(acc, 1),
    );
    let m128 = _mm_max_epu16(
        _mm256_extracti128_si256(m256, 0),
        _mm256_extracti128_si256(m256, 1),
    );
    let inv = _mm_xor_si128(m128, _mm_set1_epi16(-1i16));
    let best = 0xFFFFu32 - (_mm_extract_epi16(_mm_minpos_epu16(inv), 0) as u32);
    ((best as i64 - 1).max(0) as f32) * quantum
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
        let k = k.min(scored.len());
        // Chosen landmarks are STORED index-ascending so h_active walks each node's
        // interleaved row monotonically (max over the same set — bit-exact). Subset
        // selection ranks by bound strength first (descending score, index tie-break
        // for determinism); full-width selection (the default) skips ranking entirely —
        // the fill loop already produced ascending indices.
        let full_width = k >= scored.len();
        let chosen: Vec<usize> = if !full_width {
            scored.sort_unstable_by(|x, y| y.0.cmp(&x.0).then_with(|| x.1.cmp(&y.1)));
            let mut c: Vec<usize> = scored.iter().take(k).map(|&(_, li)| li).collect();
            c.sort_unstable();
            c
        } else {
            scored.iter().map(|&(_, li)| li).collect()
        };
        let mut active = ActiveLandmarks {
            landmarks: l,
            indices: Vec::with_capacity(k),
            goal_fw: Vec::with_capacity(k),
            goal_bw: Vec::with_capacity(k),
            full: None,
        };
        for &li in &chosen {
            active.indices.push(li);
            active.goal_fw.push(self.tab[gb + 2 * li]);
            active.goal_bw.push(self.tab[gb + 2 * li + 1]);
        }
        if full_width {
            // Neutral lanes: ga=0 (⊖row saturates to 0), gb=0xFFFF (row⊖ saturates to
            // 0), inf_odd=0 (never matches). Valid landmarks overwrite their lanes.
            let mut ga = vec![0u16; stride];
            let mut gb_ops = vec![0xFFFFu16; stride];
            let mut inf_odd = vec![0u16; stride];
            // Merged operand for the AVX-512 path: the neutral lanes of ga (even) and
            // gb (odd) interleaved, so one load covers both terms.
            let mut c: Vec<u16> = (0..stride).map(|i| if i % 2 == 0 { 0 } else { 0xFFFF }).collect();
            let mut valid_odd = vec![0u32; stride.div_ceil(32)];
            for (i, &li) in active.indices.iter().enumerate() {
                ga[2 * li] = active.goal_fw[i];
                gb_ops[2 * li + 1] = active.goal_bw[i];
                inf_odd[2 * li + 1] = 0xFFFF;
                c[2 * li] = active.goal_fw[i];
                c[2 * li + 1] = active.goal_bw[i];
                valid_odd[(2 * li + 1) / 32] |= 1 << ((2 * li + 1) % 32);
            }
            active.full = Some(FullRowOperands { ga, gb: gb_ops, inf_odd, c, valid_odd });
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

        // Full-width fast path (the default): one branchless fixed-trip pass over the
        // whole interleaved row. Values are bit-identical to the scalar subset loop
        // below, and identical between the two implementations (see
        // [`FullRowOperands`]); the AVX-512 form just reads two operand streams
        // instead of four. Row strides that are not a whole number of 32-lane
        // registers take the portable loop.
        if let Some(full) = &active.full {
            #[cfg(target_arch = "x86_64")]
            {
                if stride % 32 == 0 && avx512_full_row() {
                    // SAFETY: avx512_full_row() verified the target features; the row
                    // and merged-goal slices are both `stride` long (select_active
                    // sizes them) and valid_odd has one word per 32-lane chunk.
                    return unsafe {
                        h_full_row_avx512(&row[..stride], &full.c[..stride], &full.valid_odd, self.quantum)
                    };
                }
            }
            return h_full_row_portable(&row[..stride], full, self.quantum);
        }

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
        ((best - 1).max(0) as f32) * self.quantum
    }
}


/// Per-query landmark selection for the BACKWARD side of a bidirectional search: lower
/// bounds on d(anchor_set, v) where the anchor set is the forward search's origins
/// (start at g=0 plus every seeded global teleport at g=cost).
///
/// Per selected landmark the anchors are pre-aggregated:
///   bound_a(v) = fw_v*Q + c1        c1 = min over anchors (g0 - fw_a*Q)
///   bound_b(v) = c2 - bw_v*Q        c2 = min over anchors (g0 + bw_a*Q)
/// Validity requires EVERY anchor's per-anchor bound to be valid (any anchor could be
/// the true minimizer):
///   a-side: all fw_a < SATURATED (a saturated fw_a understates d(L,a) and would
///           overstate the bound);
///   b-side: all bw_a != UNREACHABLE (floor/saturation only understate, which is safe
///           on the minuend side).
/// The v-side rules mirror the forward heuristic: a saturated bw_v disables the b term
/// for that node; fw_v == UNREACHABLE proves d(anchors, v) = infinity when every
/// anchor's fw entry is not UNREACHABLE (L reaches every anchor, so any anchor->v path
/// would make v reachable from L).
#[derive(Default)]
pub struct ActiveLandmarksRev {
    pub landmarks: usize,
    pub indices: Vec<usize>,
    /// ms aggregate for the a-side; NAN when the a-side is unusable for this landmark.
    pub c1: Vec<f32>,
    /// ms aggregate for the b-side; NAN when the b-side is unusable for this landmark.
    pub c2: Vec<f32>,
    /// Whether fw_v == UNREACHABLE proves unreachability from all anchors.
    pub inf_ok: Vec<bool>,
}

impl<'a> LandmarkHeuristic<'a> {
    /// Select landmarks for the backward bound, scored by the bound they yield at the
    /// goal (the node where the backward search starts and bounds matter most early).
    pub fn select_active_rev(&self, anchors: &[(u32, f32)], goal: u32, k: usize) -> ActiveLandmarksRev {
        let l = self.landmarks;
        if l == 0 || self.nodes == 0 || self.tab.is_empty() || anchors.is_empty() {
            return ActiveLandmarksRev::default();
        }
        let stride = 2 * l;
        let gb = goal as usize * stride;

        struct Cand { li: usize, c1: f32, c2: f32, inf_ok: bool, score: f32 }
        let mut cands: Vec<Cand> = Vec::with_capacity(l);
        for li in 0..l {
            let mut c1 = f32::INFINITY;
            let mut c2 = f32::INFINITY;
            let mut a_ok = true;
            let mut b_ok = true;
            let mut inf_ok = true;
            for &(a, g0) in anchors {
                let fa = self.tab[a as usize * stride + 2 * li];
                let ba = self.tab[a as usize * stride + 2 * li + 1];
                if fa >= ALT_SATURATED { a_ok = false; }
                if fa == ALT_UNREACHABLE { inf_ok = false; }
                if ba == ALT_UNREACHABLE { b_ok = false; }
                if a_ok {
                    let t = g0 - fa as f32 * self.quantum;
                    if t < c1 { c1 = t; }
                }
                if b_ok {
                    let t = g0 + ba as f32 * self.quantum;
                    if t < c2 { c2 = t; }
                }
            }
            if !a_ok && !b_ok {
                continue;
            }
            let c1 = if a_ok { c1 } else { f32::NAN };
            let c2 = if b_ok { c2 } else { f32::NAN };
            // Score: bound at the goal node.
            let gfw = self.tab[gb + 2 * li];
            let gbw = self.tab[gb + 2 * li + 1];
            let mut score = 0.0f32;
            if a_ok && gfw < ALT_SATURATED {
                let v = gfw as f32 * self.quantum + c1;
                if v > score { score = v; }
            }
            if b_ok && gbw < ALT_SATURATED {
                let v = c2 - gbw as f32 * self.quantum;
                if v > score { score = v; }
            }
            cands.push(Cand { li, c1, c2, inf_ok, score });
        }
        if k < cands.len() {
            cands.sort_unstable_by(|x, y| {
                y.score.partial_cmp(&x.score).unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| x.li.cmp(&y.li))
            });
            cands.truncate(k);
            // Same monotone-row-order storage as select_active (max over the same set).
            cands.sort_unstable_by_key(|c| c.li);
        }

        let mut out = ActiveLandmarksRev { landmarks: l, ..Default::default() };
        for c in cands {
            out.indices.push(c.li);
            out.c1.push(c.c1);
            out.c2.push(c.c2);
            out.inf_ok.push(c.inf_ok);
        }
        out
    }

    /// Lower bound (ms) on d(anchor_set, v); INFINITY when v is provably unreachable
    /// from every anchor. Mirrors `h_active`'s quantization slack handling.
    #[inline]
    pub fn h_active_rev(&self, v: u32, active: &ActiveLandmarksRev) -> f32 {
        if active.indices.is_empty() {
            return 0.0;
        }
        let stride = 2 * active.landmarks;
        let vb = v as usize * stride;
        let row = &self.tab[vb..vb + stride];
        let mut best = 0.0f32;
        for i in 0..active.indices.len() {
            let li = active.indices[i];
            let fv = row[2 * li];
            let bv = row[2 * li + 1];
            if fv == ALT_UNREACHABLE {
                if active.inf_ok[i] {
                    return f32::INFINITY;
                }
            } else if fv < ALT_SATURATED && !active.c1[i].is_nan() {
                let a = fv as f32 * self.quantum + active.c1[i];
                if a > best { best = a; }
            }
            if bv < ALT_SATURATED && !active.c2[i].is_nan() {
                let b = active.c2[i] - bv as f32 * self.quantum;
                if b > best { best = b; }
            }
        }
        (best - self.quantum).max(0.0)
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
        let lm = LandmarkHeuristic { nodes: 2, landmarks: 1, tab: &tab, quantum: ALT_QUANTUM_MS };
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
        let lm = LandmarkHeuristic { nodes: 2, landmarks: 1, tab: &tab, quantum: ALT_QUANTUM_MS };
        let active = lm.select_active(0, 1, 8);
        assert!(active.indices.is_empty());
    }

    /// The two full-width implementations must agree bit-for-bit on every lane
    /// pattern, including the UNREACHABLE/SATURATED sentinels that drive the
    /// early-INF and disabled-term rules.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn avx512_full_row_matches_portable() {
        if !avx512_full_row() {
            return; // no AVX-512 (or kill switch set): only one path exists here
        }
        // 16 landmarks -> stride 32 = exactly one AVX-512 register; 32 landmarks ->
        // two, exercising the chunk loop and the per-chunk INF mask word.
        for l in [16usize, 32] {
            let stride = 2 * l;
            let nodes = 48usize;
            let mut tab = vec![0u16; nodes * stride];
            let mut x: u32 = 0x9E3779B9;
            for v in tab.iter_mut() {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                *v = match (x >> 29) % 8 {
                    0 => ALT_UNREACHABLE,
                    1 => ALT_SATURATED,
                    _ => (x % 4000) as u16,
                };
            }
            let lm = LandmarkHeuristic { nodes, landmarks: l, tab: &tab, quantum: ALT_QUANTUM_MS };
            for goal in 0..nodes as u32 {
                let active = lm.select_active(0, goal, usize::MAX);
                let Some(full) = &active.full else { continue };
                for u in 0..nodes as u32 {
                    let ub = u as usize * stride;
                    let row = &tab[ub..ub + stride];
                    let portable = h_full_row_portable(row, full, ALT_QUANTUM_MS);
                    // SAFETY: features checked above; slice lengths match the contract.
                    let simd = unsafe {
                        h_full_row_avx512(row, &full.c, &full.valid_odd, ALT_QUANTUM_MS)
                    };
                    assert_eq!(
                        portable.to_bits(), simd.to_bits(),
                        "l={l} goal={goal} node={u}: portable {portable} != simd {simd}"
                    );
                }
            }
        }
    }

    #[test]
    fn unreachable_node_gets_infinite_bound() {
        // node 2 cannot reach the landmark; goal (node 1) can.
        let tab: Vec<u16> = vec![
            0, 0,
            100, 100,
            ALT_UNREACHABLE, ALT_UNREACHABLE,
        ];
        let lm = LandmarkHeuristic { nodes: 3, landmarks: 1, tab: &tab, quantum: ALT_QUANTUM_MS };
        let active = lm.select_active(0, 1, 8);
        assert!(lm.h_active(2, &active).is_infinite());
    }
}
