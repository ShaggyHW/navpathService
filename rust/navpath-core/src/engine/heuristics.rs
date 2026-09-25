use crate::snapshot::alt_pack::{PackedAlt, Unknown};
use crate::snapshot::{Snapshot, ALT_QUANTUM_MS, ALT_SATURATED, ALT_UNREACHABLE};

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

/// One-sided landmark use (see [`ActiveLandmarks`]), default on. `NAVPATH_ALT_ONE_SIDED=0`
/// restores the pre-2026-09 rule (a landmark is used only when BOTH goal entries are
/// exact) — for A/B runs and bit-exact comparisons against older recordings.
pub fn alt_one_sided() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(std::env::var("NAVPATH_ALT_ONE_SIDED").ok().as_deref().map(str::trim), Some("0") | Some("false"))
    })
}

/// Landmark (ALT) heuristic backed by the memory-mapped quantized distance table.
///
/// The table is **node-major and interleaved**: node `n`'s row occupies
/// `[n * 2 * landmarks, (n + 1) * 2 * landmarks)` as `[fw(l0), bw(l0), fw(l1), bw(l1), …]`
/// u16 quanta of [`ALT_QUANTUM_MS`], so one heuristic call reads one contiguous row.
/// `fw(l) = d(L_l, n)` (forward Dijkstra from the landmark), `bw(l) = d(n, L_l)`.
/// [`ALT_UNREACHABLE`] marks unreachable pairs, [`ALT_SATURATED`] distances past the
/// quantized range.
///
/// The table is either the plain u16 layout (`tab`) or the clustered u8 encoding
/// (`packed`, see [`crate::snapshot::alt_pack`]); exactly one is populated.
pub struct LandmarkHeuristic<'a> {
    pub nodes: usize,
    pub landmarks: usize,
    /// Plain interleaved table (empty when `packed` is set).
    pub tab: &'a [u16],
    /// Quantum (ms) the table was built with — from the snapshot header, NOT the
    /// compiled constant (a stale binary must still read new snapshots correctly).
    pub quantum: f32,
    /// Clustered u8 table (None for the plain layout).
    pub packed: Option<PackedAlt<'a>>,
}

/// Largest supported landmark count (row decode buffers live on the stack).
pub const MAX_LANDMARKS: usize = 128;

/// How [`LandmarkHeuristic::h_active`] evaluates a selection, resolved ONCE at
/// selection time instead of re-dispatching (empty check, operand presence, stride
/// check, CPU-feature probe) on every call.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum FwdMode {
    /// No usable landmark: h = 0.
    #[default]
    Empty,
    /// Full-width branchless pass, explicit AVX-512.
    Avx512,
    /// Full-width branchless pass, portable (autovectorized).
    Portable,
    /// Subset selection (NAVPATH_ACTIVE_LANDMARKS < all): scalar loop.
    Scalar,
}

/// Per-query landmark selection produced by [`LandmarkHeuristic::select_active`].
///
/// Holds the chosen landmark column indices plus the goal's forward/backward quanta for
/// those landmarks, so the goal row is read once per query rather than on every heuristic
/// evaluation.
///
/// Each landmark contributes whatever its GOAL entries make sound (one-sided use):
///   - forward term  `d(L,goal) − d(L,u)`   needs gfw reachable (a saturated gfw only
///     understates the minuend: weaker, still admissible);
///   - backward term `d(u,L) − d(goal,L)`   needs gbw exact (< SATURATED — an
///     understated subtrahend would overstate the bound);
///   - backward INF  `bu == UNREACHABLE`    proves u cannot reach the goal whenever the
///     goal reaches L (gbw != UNREACHABLE): otherwise u -> goal -> L;
///   - forward INF   `fu != UNREACHABLE`    proves u cannot reach the goal whenever L
///     cannot reach the goal (gfw == UNREACHABLE): otherwise L -> u -> goal.
///
/// Before 2026-09 a landmark was dropped unless BOTH goal entries were exact, which gave
/// every goal outside a landmark's strongly connected component h = 0 from it (the
/// deployed legacy placement left 57k nodes with no usable landmark at all).
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
    /// Per selected landmark: bit 0 forward term usable, bit 1 backward term usable,
    /// bit 2 backward-INF rule, bit 3 forward-INF rule (see the struct docs).
    pub sides: Vec<u8>,
    /// Full-width branchless operands, present when every landmark is selected
    /// (the default since roadmap 3.1). See [`FullRowOperands`].
    pub full: Option<FullRowOperands>,
    mode: FwdMode,
}

const SIDE_FWD: u8 = 1;
const SIDE_BWD: u8 = 2;
const SIDE_BWD_INF: u8 = 4;
const SIDE_FWD_INF: u8 = 8;

/// Precomputed per-query operand lanes for the branchless full-row heuristic. Lane
/// layout mirrors the interleaved table row `[fw(l0), bw(l0), fw(l1), bw(l1), …]`:
///
/// - `ga`: even lanes hold `goal_fw` where the forward term is usable (0 otherwise/odd)
///   — so `ga.saturating_sub(row)` yields `max(0, gfw − fu)` on even lanes, with the
///   forward saturation rule free of charge (`fu >= SATURATED` saturates to 0, and
///   unusable lanes contribute the max-neutral 0).
/// - `gb`: odd lanes hold `goal_bw` where the backward term is usable (0xFFFF
///   otherwise/even) — so `row.saturating_sub(gb)` yields `max(0, bu − gbw)` on odd
///   lanes (a SATURATED `bu` still gives the valid understated bound).
/// - `inf_odd`: 0xFFFF on odd lanes carrying the backward-INF rule — `row == 0xFFFF`
///   there proves the node cannot reach the goal.
/// - `inf_even`: 0xFFFF on even lanes carrying the forward-INF rule — `row != 0xFFFF`
///   there proves the node cannot reach the goal.
///
/// The lane-wise max over `ga⊖row` and `row⊖gb` equals the scalar loop's `best`
/// integer exactly, so results are bit-identical; the whole pass is fixed-trip
/// u16 saturating arithmetic that LLVM autovectorizes under target-cpu=native.
///
/// `c`/`valid_odd`/`fwd_inf_even` are the same operands folded into fewer streams for
/// the explicit AVX-512 path ([`h_full_row_avx512`]); they carry no new information, so
/// both paths return bit-identical values.
pub struct FullRowOperands {
    ga: Vec<u16>,
    gb: Vec<u16>,
    inf_odd: Vec<u16>,
    inf_even: Vec<u16>,
    /// `ga` and `gb` merged: even lanes hold `goal_fw`, odd lanes `goal_bw` (0 / 0xFFFF
    /// on unusable lanes — the same neutral values `ga`/`gb` use).
    c: Vec<u16>,
    /// One bit per row lane (32 lanes per word): set on the odd lanes carrying the
    /// backward-INF rule. Replaces the `inf_odd` operand stream with a k-mask operand.
    valid_odd: Vec<u32>,
    /// One bit per row lane: set on the even lanes carrying the forward-INF rule.
    fwd_inf_even: Vec<u32>,
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

#[cfg(not(target_arch = "x86_64"))]
fn avx512_full_row() -> bool {
    false
}

/// Portable full-width row pass: one branchless fixed-trip loop over the interleaved
/// row that LLVM autovectorizes under target-cpu=native. See [`FullRowOperands`].
#[inline]
fn h_full_row_portable(row: &[u16], full: &FullRowOperands, quantum: f32) -> f32 {
    let stride = row.len();
    // Equal-length slice bindings so LLVM can hoist the bounds checks and
    // vectorize the fixed-trip u16 loop (psubusw/pmaxuw/pcmpeqw); indexing the
    // Vecs directly defeated autovectorization (measured 3.4x slower).
    let ga = &full.ga[..stride];
    let gbv = &full.gb[..stride];
    let io = &full.inf_odd[..stride];
    let ie = &full.inf_even[..stride];
    let row = &row[..stride];
    let mut best: u16 = 0;
    let mut inf: u16 = 0;
    for i in 0..stride {
        let r = row[i];
        let a = ga[i].saturating_sub(r);
        let b = r.saturating_sub(gbv[i]);
        best = best.max(a.max(b));
        // Branchless: 0xFFFF where r == UNREACHABLE, masked to the INF-rule lanes.
        let m = ((r == ALT_UNREACHABLE) as u16).wrapping_neg();
        inf |= (m & io[i]) | (!m & ie[i]);
    }
    if inf != 0 {
        // u cannot reach the goal (see ActiveLandmarks for both INF rules).
        return f32::INFINITY;
    }
    ((best as i64 - 1).max(0) as f32) * quantum
}

/// The same pass in explicit AVX-512, reading TWO operand streams (the node's row and
/// the merged goal vector) instead of the portable path's five — measured 8.7 → 3.6 ns
/// per node warm, 10.8 → 4.3 ns on random rows, bit-identical on the deployed table.
///
/// Even lanes hold `c = goal_fw`, odd lanes `c = goal_bw`, so `subs(c, row)` is the
/// forward term and `subs(row, c)` the backward one; a constant lane-parity blend picks
/// the right term per lane, which is exactly the portable `max(ga⊖row, row⊖gb)` (the
/// wrong-parity term is 0 by construction there, and the neutral operands of unusable
/// lanes are identical here). Both INF rules fold into per-query k-masks.
///
/// # Safety
/// The caller must have verified the target features via [`avx512_full_row`] and pass
/// `row.len() == c.len()` a multiple of 32, with the mask vectors holding at least
/// `row.len() / 32` words.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx2,sse4.1")]
unsafe fn h_full_row_avx512(row: &[u16], c: &[u16], valid_odd: &[u32], fwd_inf_even: &[u32], quantum: f32) -> f32 {
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
        let unreach = _mm512_cmpeq_epu16_mask(r, ones);
        inf |= (unreach & valid_odd[j / 32]) | (!unreach & fwd_inf_even[j / 32]);
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

/// [`h_full_row_avx512`] over a packed row: each 32-lane chunk is decoded in registers
/// (`base + zext(off)`, then the three special codes substituted by mask moves: UNKNOWN
/// by the forward-node values of [`Unknown::FWD_NODE`], SAT and UNREACH by their u16
/// sentinels) and fed to the identical term/INF/max logic, so the result equals the
/// plain kernel run on the decoded row.
///
/// # Safety
/// As [`h_full_row_avx512`]; `base` holds `2 * row.len()` bytes (u16 lanes, LE).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx2,sse4.1")]
unsafe fn h_full_row_avx512_packed(
    base: &[u8],
    row: &[u8],
    c: &[u16],
    valid_odd: &[u32],
    fwd_inf_even: &[u32],
    quantum: f32,
) -> f32 {
    use core::arch::x86_64::*;
    const ODD: u32 = 0xAAAA_AAAA;
    let ones = _mm512_set1_epi16(-1i16);
    let sat = _mm512_set1_epi16(ALT_SATURATED as i16);
    // Little-endian lane pairs: even lane (low half) = FWD_NODE.even, odd = FWD_NODE.odd.
    let unk = _mm512_set1_epi32(((Unknown::FWD_NODE.odd as u32) << 16 | Unknown::FWD_NODE.even as u32) as i32);
    let c253 = _mm512_set1_epi16(crate::snapshot::alt_pack::CODE_UNKNOWN as i16);
    let c254 = _mm512_set1_epi16(crate::snapshot::alt_pack::CODE_SAT as i16);
    let c255 = _mm512_set1_epi16(crate::snapshot::alt_pack::CODE_UNREACH as i16);
    let mut acc = _mm512_setzero_si512();
    let mut inf: u32 = 0;
    let mut j = 0usize;
    while j < row.len() {
        let off = _mm512_cvtepu8_epi16(_mm256_loadu_si256(row.as_ptr().add(j) as *const __m256i));
        let b = _mm512_loadu_si512(base.as_ptr().add(2 * j) as *const _);
        let mut r = _mm512_add_epi16(b, off);
        r = _mm512_mask_mov_epi16(r, _mm512_cmpeq_epu16_mask(off, c253), unk);
        r = _mm512_mask_mov_epi16(r, _mm512_cmpeq_epu16_mask(off, c254), sat);
        let unreach = _mm512_cmpeq_epu16_mask(off, c255);
        r = _mm512_mask_mov_epi16(r, unreach, ones);
        let cv = _mm512_loadu_si512(c.as_ptr().add(j) as *const _);
        let fwd = _mm512_subs_epu16(cv, r);
        let bwd = _mm512_subs_epu16(r, cv);
        acc = _mm512_max_epu16(acc, _mm512_mask_blend_epi16(ODD, fwd, bwd));
        inf |= (unreach & valid_odd[j / 32]) | (!unreach & fwd_inf_even[j / 32]);
        j += 32;
    }
    if inf != 0 {
        return f32::INFINITY;
    }
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
    pub fn new(nodes: usize, landmarks: usize, tab: &'a [u16], quantum: f32) -> Self {
        assert!(landmarks <= MAX_LANDMARKS, "at most {MAX_LANDMARKS} landmarks are supported");
        LandmarkHeuristic { nodes, landmarks, tab, quantum, packed: None }
    }

    /// A heuristic over a clustered u8 table (`data` = [`crate::snapshot::alt_pack`]
    /// records for `nodes` nodes).
    /// `section` = cluster records followed by the exception list, as
    /// [`crate::snapshot::alt_pack::pack_alt`] produces and the snapshot stores it.
    pub fn new_packed(nodes: usize, landmarks: usize, section: &'a [u8], quantum: f32) -> Self {
        assert!(landmarks <= MAX_LANDMARKS, "at most {MAX_LANDMARKS} landmarks are supported");
        let packed = PackedAlt::from_section(section, nodes, landmarks);
        LandmarkHeuristic { nodes, landmarks, tab: &[], quantum, packed: Some(packed) }
    }

    /// The snapshot's ALT table (plain or packed) with the quantum stamped in its header.
    pub fn from_snapshot(s: &'a Snapshot) -> Self {
        let nodes = s.counts().nodes as usize;
        let landmarks = s.counts().landmarks as usize;
        let quantum = s.manifest().alt_quantum_ms;
        match s.lm_packed() {
            Some(data) => Self::new_packed(nodes, landmarks, data, quantum),
            None => Self::new(nodes, landmarks, s.lm_tab(), quantum),
        }
    }

    /// Whether the table holds any data.
    #[inline]
    fn has_table(&self) -> bool {
        self.landmarks > 0 && self.nodes > 0 && (self.packed.is_some() || !self.tab.is_empty())
    }

    /// Node `u`'s EXACT row as plain u16 lanes (per-query use: goal, start, anchors):
    /// borrowed from the plain table, or decoded exactly (exception list) from the
    /// packed one into `buf`.
    #[inline]
    fn row_exact<'b>(&'b self, u: u32, buf: &'b mut [u16; 2 * MAX_LANDMARKS]) -> &'b [u16] {
        let stride = 2 * self.landmarks;
        match &self.packed {
            None => {
                let ub = u as usize * stride;
                &self.tab[ub..ub + stride]
            }
            Some(p) => {
                p.decode_row_exact(u as usize, &mut buf[..stride]);
                &buf[..stride]
            }
        }
    }

    /// Best-effort prefetch of node `u`'s whole interleaved row (4 cache lines at 64
    /// landmarks). Prefetches never fault, so any id is safe.
    #[inline(always)]
    pub fn prefetch_row(&self, u: u32) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use core::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
            let stride = 2 * self.landmarks;
            let (ptr, bytes) = match &self.packed {
                None => ((self.tab.as_ptr() as *const u8).wrapping_add(u as usize * stride * 2), stride * 2),
                Some(p) => {
                    let (_, row) = p.parts(u as usize);
                    (row.as_ptr(), stride)
                }
            };
            let mut off = 0usize;
            while off < bytes {
                _mm_prefetch(ptr.wrapping_add(off) as *const i8, _MM_HINT_T0);
                off += 64;
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = u;
        }
    }

    /// Select the best `k` landmarks for the (start, goal) pair and cache the goal's row for
    /// those landmarks. Landmarks are scored by the lower bound they yield at the start node
    /// (the tightest bounds for this query); ties break by landmark index for determinism.
    ///
    /// Each landmark is used one-sidedly, as far as its GOAL entries allow (see
    /// [`ActiveLandmarks`]). The INF rules are provably correct: `d(u,L) <= d(u,goal) +
    /// d(goal,L)` means a node that can reach the goal can also reach every landmark the
    /// goal reaches, and `d(L,goal) <= d(L,u) + d(u,goal)` means a node that a landmark
    /// reaches cannot reach the goal when that landmark cannot.
    pub fn select_active(&self, start: u32, goal: u32, k: usize) -> ActiveLandmarks {
        let l = self.landmarks;
        if !self.has_table() {
            return ActiveLandmarks::default();
        }
        let stride = 2 * l;
        let (mut gbuf, mut sbuf) = ([0u16; 2 * MAX_LANDMARKS], [0u16; 2 * MAX_LANDMARKS]);
        let grow = self.row_exact(goal, &mut gbuf);
        let srow = self.row_exact(start, &mut sbuf);

        let one_sided = alt_one_sided();
        let mut scored: Vec<(i64, usize, u8)> = Vec::with_capacity(l);
        for li in 0..l {
            let gfw = grow[2 * li];
            let gbw = grow[2 * li + 1];
            if !one_sided && (gfw >= ALT_SATURATED || gbw >= ALT_SATURATED) {
                continue;
            }
            let mut sides = 0u8;
            if gfw != ALT_UNREACHABLE {
                sides |= SIDE_FWD;
            } else {
                sides |= SIDE_FWD_INF;
            }
            // Saturated goal bw is unusable as the backward subtrahend (an understated
            // d(goal,L) overstates b = bu - gbw), but still proves the goal reaches L.
            if gbw < ALT_SATURATED {
                sides |= SIDE_BWD;
            }
            if gbw != ALT_UNREACHABLE {
                sides |= SIDE_BWD_INF;
            }
            let sfw = srow[2 * li];
            let sbw = srow[2 * li + 1];
            // Score in quanta; provably-unreachable start entries score as "infinitely
            // useful" (the start's whole unreachable-to-goal region gets pruned). A
            // saturated forward start entry understates d(L,start) and would overstate
            // the bound, so that side is unusable (mirrors h_active).
            let a = if sides & SIDE_FWD != 0 && sfw < ALT_SATURATED { gfw as i64 - sfw as i64 } else { i64::MIN };
            let b = if sides & SIDE_BWD != 0 && sbw != ALT_UNREACHABLE { sbw as i64 - gbw as i64 } else { i64::MIN };
            let inf = (sides & SIDE_BWD_INF != 0 && sbw == ALT_UNREACHABLE)
                || (sides & SIDE_FWD_INF != 0 && sfw != ALT_UNREACHABLE);
            let v = if inf { i64::MAX } else { a.max(b).max(0) };
            scored.push((v, li, sides));
        }
        let k = k.min(scored.len());
        // Chosen landmarks are STORED index-ascending so h_active walks each node's
        // interleaved row monotonically (max over the same set — bit-exact). Subset
        // selection ranks by bound strength first (descending score, index tie-break
        // for determinism); full-width selection (the default) skips ranking entirely —
        // the fill loop already produced ascending indices.
        let full_width = k >= scored.len();
        let chosen: Vec<(usize, u8)> = if !full_width {
            scored.sort_unstable_by(|x, y| y.0.cmp(&x.0).then_with(|| x.1.cmp(&y.1)));
            let mut c: Vec<(usize, u8)> = scored.iter().take(k).map(|&(_, li, s)| (li, s)).collect();
            c.sort_unstable();
            c
        } else {
            scored.iter().map(|&(_, li, s)| (li, s)).collect()
        };
        let mut active = ActiveLandmarks {
            landmarks: l,
            indices: Vec::with_capacity(k),
            goal_fw: Vec::with_capacity(k),
            goal_bw: Vec::with_capacity(k),
            sides: Vec::with_capacity(k),
            full: None,
            mode: FwdMode::Empty,
        };
        for &(li, s) in &chosen {
            active.indices.push(li);
            active.goal_fw.push(grow[2 * li]);
            active.goal_bw.push(grow[2 * li + 1]);
            active.sides.push(s);
        }
        if active.indices.is_empty() {
            return active;
        }
        if full_width {
            // Neutral lanes: ga=0 (⊖row saturates to 0), gb=0xFFFF (row⊖ saturates to
            // 0), INF masks 0 (never fire). Usable sides overwrite their lanes.
            let mut ga = vec![0u16; stride];
            let mut gb_ops = vec![0xFFFFu16; stride];
            let mut inf_odd = vec![0u16; stride];
            let mut inf_even = vec![0u16; stride];
            // Merged operand for the AVX-512 path: the neutral lanes of ga (even) and
            // gb (odd) interleaved, so one load covers both terms.
            let mut c: Vec<u16> = (0..stride).map(|i| if i % 2 == 0 { 0 } else { 0xFFFF }).collect();
            let mut valid_odd = vec![0u32; stride.div_ceil(32)];
            let mut fwd_inf_even = vec![0u32; stride.div_ceil(32)];
            for (i, &li) in active.indices.iter().enumerate() {
                let s = active.sides[i];
                let (e, o) = (2 * li, 2 * li + 1);
                if s & SIDE_FWD != 0 {
                    ga[e] = active.goal_fw[i];
                    c[e] = active.goal_fw[i];
                }
                if s & SIDE_BWD != 0 {
                    gb_ops[o] = active.goal_bw[i];
                    c[o] = active.goal_bw[i];
                }
                if s & SIDE_BWD_INF != 0 {
                    inf_odd[o] = 0xFFFF;
                    valid_odd[o / 32] |= 1 << (o % 32);
                }
                if s & SIDE_FWD_INF != 0 {
                    inf_even[e] = 0xFFFF;
                    fwd_inf_even[e / 32] |= 1 << (e % 32);
                }
            }
            active.full = Some(FullRowOperands { ga, gb: gb_ops, inf_odd, inf_even, c, valid_odd, fwd_inf_even });
            active.mode = if stride.is_multiple_of(32) && avx512_full_row() { FwdMode::Avx512 } else { FwdMode::Portable };
        } else {
            active.mode = FwdMode::Scalar;
        }
        active
    }

    /// Heuristic over the selected active landmarks, reusing the cached goal row. Reads
    /// one contiguous interleaved row of the active node.
    ///
    /// Returns milliseconds. One quantum is subtracted from the max bound to compensate
    /// floor-quantization (keeps the bound admissible). `f32::INFINITY` is returned when
    /// the node provably cannot reach the goal (see [`ActiveLandmarks`]).
    #[inline]
    pub fn h_active(&self, u: u32, active: &ActiveLandmarks) -> f32 {
        let stride = 2 * active.landmarks;
        if let Some(p) = &self.packed {
            return self.h_active_packed(p, u, active);
        }
        match active.mode {
            FwdMode::Empty => 0.0,
            FwdMode::Avx512 => {
                #[cfg(target_arch = "x86_64")]
                {
                    let ub = u as usize * stride;
                    let row = &self.tab[ub..ub + stride];
                    let full = active.full.as_ref().unwrap();
                    // SAFETY: the mode is only chosen after avx512_full_row() verified
                    // the target features and stride % 32 == 0; the row and merged-goal
                    // slices are both `stride` long (select_active sizes them) and the
                    // mask vectors hold one word per 32-lane chunk.
                    unsafe { h_full_row_avx512(row, &full.c[..stride], &full.valid_odd, &full.fwd_inf_even, self.quantum) }
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    unreachable!()
                }
            }
            FwdMode::Portable => {
                let ub = u as usize * stride;
                h_full_row_portable(&self.tab[ub..ub + stride], active.full.as_ref().unwrap(), self.quantum)
            }
            FwdMode::Scalar => {
                let ub = u as usize * stride;
                self.h_scalar(&self.tab[ub..ub + stride], active)
            }
        }
    }

    /// [`LandmarkHeuristic::h_active`] over the packed table: the explicit AVX-512
    /// kernel decodes the row in registers; other modes decode into a stack buffer and
    /// run the plain-table evaluator on it (bit-identical to evaluating a plain table
    /// holding the decoded values).
    #[inline]
    fn h_active_packed(&self, p: &PackedAlt, u: u32, active: &ActiveLandmarks) -> f32 {
        let stride = 2 * active.landmarks;
        match active.mode {
            FwdMode::Empty => 0.0,
            FwdMode::Avx512 => {
                #[cfg(target_arch = "x86_64")]
                {
                    let (base, row) = p.parts(u as usize);
                    let full = active.full.as_ref().unwrap();
                    // SAFETY: as for the plain kernel; `base` holds 2*stride bytes and
                    // `row` stride bytes (PackedAlt::parts).
                    unsafe { h_full_row_avx512_packed(base, row, &full.c[..stride], &full.valid_odd, &full.fwd_inf_even, self.quantum) }
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    unreachable!()
                }
            }
            FwdMode::Portable | FwdMode::Scalar => {
                let mut buf = [0u16; 2 * MAX_LANDMARKS];
                let row = &mut buf[..stride];
                p.decode_row(u as usize, Unknown::FWD_NODE, row);
                if active.mode == FwdMode::Portable {
                    h_full_row_portable(row, active.full.as_ref().unwrap(), self.quantum)
                } else {
                    self.h_scalar(row, active)
                }
            }
        }
    }

    /// Reference evaluation (subset selections; also the oracle for the full-row paths).
    fn h_scalar(&self, row: &[u16], active: &ActiveLandmarks) -> f32 {
        let mut best: i64 = 0;
        for i in 0..active.indices.len() {
            let li = active.indices[i];
            let s = active.sides[i];
            let fu = row[2 * li];
            let bu = row[2 * li + 1];
            if (s & SIDE_BWD_INF != 0 && bu == ALT_UNREACHABLE) || (s & SIDE_FWD_INF != 0 && fu != ALT_UNREACHABLE) {
                return f32::INFINITY;
            }
            // A SATURATED bu still yields a valid (understated) bound: d(u,L) really is
            // at least SATURATED quanta.
            if s & SIDE_BWD != 0 {
                let b = bu as i64 - active.goal_bw[i] as i64;
                if b > best { best = b; }
            }
            // The forward side is only valid when fu is exact: a saturated fu
            // understates d(L,u), which would OVERstate this bound.
            if s & SIDE_FWD != 0 && fu < ALT_SATURATED {
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
    /// Full-row operands for the explicit AVX-512 evaluator (None: scalar loop).
    full: Option<RevFullRow>,
}

/// Branchless full-row operands of the backward bound, one f32 lane per interleaved row
/// lane: a lane's candidate is `r * s + k` — even lanes `fw*Q + c1` (`s = Q`, `k = c1`),
/// odd lanes `bw*(-Q) + c2` (== `c2 - bw*Q` exactly: IEEE subtraction IS addition of
/// the negation, and `bw*(-Q) == -(bw*Q)`), computed as a separate multiply and add
/// (never fused) so every candidate is bit-identical to the scalar loop's. `valid` masks
/// the lanes whose side is usable (unselected landmarks, NAN aggregates excluded); the
/// per-node rule "r < SATURATED" is applied at evaluation. `inf` marks the even lanes
/// carrying the UNREACHABLE => INF rule. The max over the same candidate set is order
/// independent, and the final `(best - Q).max(0)` erases the one possible difference
/// (a -0.0 vs +0.0 best), so results are bit-identical to the scalar evaluator.
struct RevFullRow {
    s: Vec<f32>,
    k: Vec<f32>,
    /// One bit per lane, 16 lanes per word (one AVX-512 f32 register per word).
    valid: Vec<u16>,
    inf: Vec<u16>,
}

/// Aggregate of a fixed anchor set over every landmark (see [`ActiveLandmarksRev`]):
/// the per-landmark `c1`/`c2` minima and validity flags before goal scoring. A pure
/// function of the anchor list, so the service computes the global-teleport part once
/// per profile ([`LandmarkHeuristic::rev_base`]) and each search folds in only its own
/// origin — min/AND aggregation is order-independent, so the result is bit-identical to
/// aggregating the full list per search.
pub struct RevAnchorBase {
    fp: u64,
    count: usize,
    landmarks: usize,
    c1: Vec<f32>,
    c2: Vec<f32>,
    a_ok: Vec<bool>,
    b_ok: Vec<bool>,
    inf_ok: Vec<bool>,
    /// The aggregated anchors and their exact rows (anchor-major, `2 * landmarks` lanes
    /// each), so a per-query SUBSET can be re-aggregated without row gathers or decodes
    /// ([`LandmarkHeuristic::select_active_rev_subset`]).
    anchors: Vec<(u32, f32)>,
    rows: Vec<u16>,
}

/// Order-sensitive fingerprint of an anchor list (ids and cost bits).
pub fn anchors_fingerprint<'x>(anchors: impl Iterator<Item = &'x (u32, f32)>) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut n: u64 = 0;
    for &(node, g0) in anchors {
        h = (h ^ node as u64).wrapping_mul(0x100000001b3);
        h = (h ^ g0.to_bits() as u64).wrapping_mul(0x100000001b3);
        n += 1;
    }
    (h ^ n).wrapping_mul(0x100000001b3)
}

impl RevAnchorBase {
    /// Whether this base aggregates exactly the given anchor list.
    pub fn matches<'x>(&self, anchors: impl Iterator<Item = &'x (u32, f32)>) -> bool {
        anchors_fingerprint(anchors) == self.fp
    }

    pub fn anchor_count(&self) -> usize {
        self.count
    }

    /// Landmark count of the table this base was aggregated over.
    pub fn landmarks(&self) -> usize {
        self.landmarks
    }

    /// The aggregated anchor list (in aggregation order).
    pub fn anchors(&self) -> &[(u32, f32)] {
        &self.anchors
    }
}

/// Is the AVX-512 backward evaluator available (same switch as the forward one)?
fn avx512_rev() -> bool {
    avx512_full_row()
}

/// Explicit AVX-512 backward bound over the full interleaved row (see [`RevFullRow`]).
///
/// # Safety
/// avx512f/avx512bw verified via [`avx512_rev`]; `row.len() == s.len() == k.len()` is a
/// multiple of 16 and `valid`/`inf` hold `row.len() / 16` words.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx2")]
unsafe fn h_rev_full_row_avx512(row: &[u16], f: &RevFullRow, quantum: f32) -> f32 {
    use core::arch::x86_64::*;
    let sat = _mm512_set1_epi32(ALT_SATURATED as i32);
    let unreach = _mm512_set1_epi32(ALT_UNREACHABLE as i32);
    let mut acc = _mm512_setzero_ps();
    let mut inf: u16 = 0;
    let mut j = 0usize;
    while j < row.len() {
        let w = j / 16;
        let r16 = _mm256_loadu_si256(row.as_ptr().add(j) as *const __m256i);
        let r32 = _mm512_cvtepu16_epi32(r16);
        inf |= _mm512_cmpeq_epi32_mask(r32, unreach) & f.inf[w];
        let rf = _mm512_cvtepi32_ps(r32);
        let t = _mm512_add_ps(
            _mm512_mul_ps(rf, _mm512_loadu_ps(f.s.as_ptr().add(j))),
            _mm512_loadu_ps(f.k.as_ptr().add(j)),
        );
        let ok = _mm512_cmplt_epi32_mask(r32, sat) & f.valid[w];
        acc = _mm512_mask_max_ps(acc, ok, acc, t);
        j += 16;
    }
    if inf != 0 {
        return f32::INFINITY;
    }
    let best = _mm512_reduce_max_ps(acc);
    (best - quantum).max(0.0)
}

/// [`h_rev_full_row_avx512`] over a packed row (16 lanes per step, decoded to u32:
/// UNKNOWN and SAT -> SATURATED per [`Unknown::REV_NODE`], UNREACH -> UNREACHABLE).
///
/// # Safety
/// As [`h_rev_full_row_avx512`]; `base` holds `2 * row.len()` bytes.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx2")]
unsafe fn h_rev_full_row_avx512_packed(base: &[u8], row: &[u8], f: &RevFullRow, quantum: f32) -> f32 {
    use core::arch::x86_64::*;
    debug_assert_eq!(Unknown::REV_NODE.even, ALT_SATURATED);
    debug_assert_eq!(Unknown::REV_NODE.odd, ALT_SATURATED);
    let sat = _mm512_set1_epi32(ALT_SATURATED as i32);
    let unreach = _mm512_set1_epi32(ALT_UNREACHABLE as i32);
    let c253 = _mm512_set1_epi32(crate::snapshot::alt_pack::CODE_UNKNOWN as i32);
    let c255 = _mm512_set1_epi32(crate::snapshot::alt_pack::CODE_UNREACH as i32);
    let mut acc = _mm512_setzero_ps();
    let mut inf: u16 = 0;
    let mut j = 0usize;
    while j < row.len() {
        let w = j / 16;
        let off = _mm512_cvtepu8_epi32(_mm_loadu_si128(row.as_ptr().add(j) as *const __m128i));
        let b = _mm512_cvtepu16_epi32(_mm256_loadu_si256(base.as_ptr().add(2 * j) as *const __m256i));
        let mut r32 = _mm512_add_epi32(b, off);
        // 253 (UNKNOWN) and 254 (SAT) both decode to SATURATED for this role.
        r32 = _mm512_mask_mov_epi32(r32, _mm512_cmpge_epu32_mask(off, c253), sat);
        let unr = _mm512_cmpeq_epi32_mask(off, c255);
        r32 = _mm512_mask_mov_epi32(r32, unr, unreach);
        inf |= unr & f.inf[w];
        let rf = _mm512_cvtepi32_ps(r32);
        let t = _mm512_add_ps(
            _mm512_mul_ps(rf, _mm512_loadu_ps(f.s.as_ptr().add(j))),
            _mm512_loadu_ps(f.k.as_ptr().add(j)),
        );
        let ok = _mm512_cmplt_epi32_mask(r32, sat) & f.valid[w];
        acc = _mm512_mask_max_ps(acc, ok, acc, t);
        j += 16;
    }
    if inf != 0 {
        return f32::INFINITY;
    }
    let best = _mm512_reduce_max_ps(acc);
    (best - quantum).max(0.0)
}

impl<'a> LandmarkHeuristic<'a> {
    /// Aggregate an anchor list over all landmarks (anchor-outer, so each anchor's row is
    /// read once, contiguously).
    pub fn rev_base(&self, anchors: &[(u32, f32)]) -> RevAnchorBase {
        let l = self.landmarks;
        let mut base = RevAnchorBase {
            fp: anchors_fingerprint(anchors.iter()),
            count: anchors.len(),
            landmarks: l,
            c1: vec![f32::INFINITY; l],
            c2: vec![f32::INFINITY; l],
            a_ok: vec![true; l],
            b_ok: vec![true; l],
            inf_ok: vec![true; l],
            anchors: anchors.to_vec(),
            rows: Vec::with_capacity(anchors.len() * 2 * l),
        };
        let mut buf = [0u16; 2 * MAX_LANDMARKS];
        for &(a, _) in anchors {
            let row = self.row_exact(a, &mut buf);
            base.rows.extend_from_slice(row);
        }
        self.fold_anchors(&mut base, anchors);
        base
    }

    /// An empty aggregate (identity of the min/AND fold).
    fn empty_agg(&self) -> RevAnchorBase {
        let l = self.landmarks;
        RevAnchorBase {
            fp: 0,
            count: 0,
            landmarks: l,
            c1: vec![f32::INFINITY; l],
            c2: vec![f32::INFINITY; l],
            a_ok: vec![true; l],
            b_ok: vec![true; l],
            inf_ok: vec![true; l],
            anchors: Vec::new(),
            rows: Vec::new(),
        }
    }

    fn fold_anchors(&self, base: &mut RevAnchorBase, anchors: &[(u32, f32)]) {
        let mut buf = [0u16; 2 * MAX_LANDMARKS];
        for &(a, g0) in anchors {
            let row = self.row_exact(a, &mut buf);
            Self::fold_row(base, row, g0, self.quantum);
        }
    }

    #[inline]
    fn fold_row(base: &mut RevAnchorBase, row: &[u16], g0: f32, q: f32) {
        for li in 0..base.landmarks {
            let fa = row[2 * li];
            let ba = row[2 * li + 1];
            if fa >= ALT_SATURATED { base.a_ok[li] = false; }
            if fa == ALT_UNREACHABLE { base.inf_ok[li] = false; }
            if ba == ALT_UNREACHABLE { base.b_ok[li] = false; }
            // min is exact and order-independent (no NaN, no -0.0 can arise), so
            // folding anchors in any order yields bit-identical aggregates; values
            // of an already-invalid side are discarded below.
            let t1 = g0 - fa as f32 * q;
            if t1 < base.c1[li] { base.c1[li] = t1; }
            let t2 = g0 + ba as f32 * q;
            if t2 < base.c2[li] { base.c2[li] = t2; }
        }
    }

    /// Backward selection over the SUBSET of `base`'s anchors flagged in `keep` plus
    /// `extra` — bit-identical to [`LandmarkHeuristic::select_active_rev`] over that
    /// concatenated list, re-aggregated from the base's stored rows (no row gathers).
    pub fn select_active_rev_subset(
        &self,
        base: &RevAnchorBase,
        keep: &[bool],
        extra: &[(u32, f32)],
        goal: u32,
        k: usize,
    ) -> ActiveLandmarksRev {
        let l = self.landmarks;
        if base.landmarks != l || keep.len() != base.anchors.len() {
            debug_assert!(false, "RevAnchorBase/keep shape mismatch");
            return ActiveLandmarksRev::default();
        }
        let mut agg = self.empty_agg();
        let stride = 2 * l;
        for (i, &(_, g0)) in base.anchors.iter().enumerate() {
            if keep[i] {
                Self::fold_row(&mut agg, &base.rows[i * stride..(i + 1) * stride], g0, self.quantum);
                agg.count += 1;
            }
        }
        self.fold_anchors(&mut agg, extra);
        agg.count += extra.len();
        if !self.has_table() || agg.count == 0 {
            return ActiveLandmarksRev::default();
        }
        self.finish_rev(agg, goal, k)
    }

    /// Select landmarks for the backward bound, scored by the bound they yield at the
    /// goal (the node where the backward search starts and bounds matter most early).
    pub fn select_active_rev(&self, anchors: &[(u32, f32)], goal: u32, k: usize) -> ActiveLandmarksRev {
        self.select_active_rev_based(None, anchors, goal, k)
    }

    /// [`LandmarkHeuristic::select_active_rev`] over `base`'s anchors plus `anchors`
    /// (bit-identical to passing the concatenated list).
    pub fn select_active_rev_based(
        &self,
        base: Option<&RevAnchorBase>,
        anchors: &[(u32, f32)],
        goal: u32,
        k: usize,
    ) -> ActiveLandmarksRev {
        let l = self.landmarks;
        let total = anchors.len() + base.map_or(0, |b| b.count);
        if !self.has_table() || total == 0 {
            return ActiveLandmarksRev::default();
        }
        let mut agg = match base {
            Some(b) if b.landmarks == l => RevAnchorBase {
                fp: 0,
                count: b.count,
                landmarks: l,
                c1: b.c1.clone(),
                c2: b.c2.clone(),
                a_ok: b.a_ok.clone(),
                b_ok: b.b_ok.clone(),
                inf_ok: b.inf_ok.clone(),
                anchors: Vec::new(),
                rows: Vec::new(),
            },
            // A base built for a different table shape cannot be reused, and dropping
            // its anchors would make the bound inadmissible (a min over FEWER anchors
            // is larger): fall back to no backward heuristic (h = 0, always admissible).
            // The engine checks `landmarks()` before passing a base, so this is only a
            // guard.
            Some(_) => {
                debug_assert!(false, "RevAnchorBase built for a different landmark count");
                return ActiveLandmarksRev::default();
            }
            None => self.empty_agg(),
        };
        self.fold_anchors(&mut agg, anchors);
        self.finish_rev(agg, goal, k)
    }

    /// Candidate selection + operand build over a finished anchor aggregate.
    fn finish_rev(&self, agg: RevAnchorBase, goal: u32, k: usize) -> ActiveLandmarksRev {
        let l = self.landmarks;
        let stride = 2 * l;
        let mut gbuf = [0u16; 2 * MAX_LANDMARKS];
        let grow = self.row_exact(goal, &mut gbuf);
        struct Cand { li: usize, c1: f32, c2: f32, inf_ok: bool, score: f32 }
        let mut cands: Vec<Cand> = Vec::with_capacity(l);
        for li in 0..l {
            let (a_ok, b_ok) = (agg.a_ok[li], agg.b_ok[li]);
            if !a_ok && !b_ok {
                continue;
            }
            let c1 = if a_ok { agg.c1[li] } else { f32::NAN };
            let c2 = if b_ok { agg.c2[li] } else { f32::NAN };
            // Score: bound at the goal node.
            let gfw = grow[2 * li];
            let gbw = grow[2 * li + 1];
            let mut score = 0.0f32;
            if a_ok && gfw < ALT_SATURATED {
                let v = gfw as f32 * self.quantum + c1;
                if v > score { score = v; }
            }
            if b_ok && gbw < ALT_SATURATED {
                let v = c2 - gbw as f32 * self.quantum;
                if v > score { score = v; }
            }
            cands.push(Cand { li, c1, c2, inf_ok: agg.inf_ok[li], score });
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
        if !out.indices.is_empty() && stride.is_multiple_of(16) && avx512_rev() {
            let q = self.quantum;
            let mut s = vec![0.0f32; stride];
            let mut kk = vec![0.0f32; stride];
            let mut valid = vec![0u16; stride / 16];
            let mut inf = vec![0u16; stride / 16];
            for i in 0..out.indices.len() {
                let li = out.indices[i];
                let (e, o) = (2 * li, 2 * li + 1);
                if !out.c1[i].is_nan() {
                    s[e] = q;
                    kk[e] = out.c1[i];
                    valid[e / 16] |= 1 << (e % 16);
                }
                if !out.c2[i].is_nan() {
                    s[o] = -q;
                    kk[o] = out.c2[i];
                    valid[o / 16] |= 1 << (o % 16);
                }
                if out.inf_ok[i] {
                    inf[e / 16] |= 1 << (e % 16);
                }
            }
            out.full = Some(RevFullRow { s, k: kk, valid, inf });
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
        if let Some(p) = &self.packed {
            #[cfg(target_arch = "x86_64")]
            if let Some(f) = &active.full {
                let (base, row) = p.parts(v as usize);
                // SAFETY: as below; PackedAlt::parts sizes base/row to the stride.
                return unsafe { h_rev_full_row_avx512_packed(base, row, f, self.quantum) };
            }
            let mut buf = [0u16; 2 * MAX_LANDMARKS];
            let row = &mut buf[..stride];
            p.decode_row(v as usize, Unknown::REV_NODE, row);
            return self.h_rev_scalar(row, active);
        }
        let vb = v as usize * stride;
        let row = &self.tab[vb..vb + stride];
        #[cfg(target_arch = "x86_64")]
        if let Some(f) = &active.full {
            // SAFETY: `full` is only built after avx512_rev() verified the features and
            // stride % 16 == 0; operand vectors are sized to the stride.
            return unsafe { h_rev_full_row_avx512(row, f, self.quantum) };
        }
        self.h_rev_scalar(row, active)
    }

    /// Reference evaluator (and the path for hosts without AVX-512).
    fn h_rev_scalar(&self, row: &[u16], active: &ActiveLandmarksRev) -> f32 {
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
        let lm = LandmarkHeuristic::new(2, 1, &tab, ALT_QUANTUM_MS);
        let active = lm.select_active(0, 1, 8);
        assert_eq!(active.indices, vec![0]);
        let h = lm.h_active(0, &active);
        assert!(h <= 6400.0 && h >= 6400.0 - 2.0 * ALT_QUANTUM_MS, "h={h}");
        // goal itself: zero-ish bound
        assert!(lm.h_active(1, &active) <= ALT_QUANTUM_MS);
    }

    #[test]
    fn saturated_goal_bw_disables_only_the_backward_term() {
        // Goal's backward entry saturated: using it as the subtrahend would overstate
        // bounds, so the backward term must be off — but the forward side (a saturated
        // gfw only understates the minuend) stays usable, and so does the backward INF
        // rule (the goal does reach the landmark).
        let tab: Vec<u16> = vec![
            0, 5,                          // node 0: the landmark's neighbourhood
            ALT_SATURATED, ALT_SATURATED,  // node 1: goal
            3, ALT_UNREACHABLE,            // node 2: cannot reach L => cannot reach goal
        ];
        let lm = LandmarkHeuristic::new(3, 1, &tab, ALT_QUANTUM_MS);
        let active = lm.select_active(0, 1, 8);
        assert_eq!(active.indices, vec![0]);
        assert_eq!(active.sides[0] & SIDE_BWD, 0, "backward term must be disabled");
        // Forward term only: gfw - fu = SATURATED - 0 quanta, minus the slack quantum.
        let h0 = lm.h_active(0, &active);
        assert_eq!(h0, ((ALT_SATURATED as i64 - 1) as f32) * ALT_QUANTUM_MS);
        assert!(lm.h_active(2, &active).is_infinite());
    }

    #[test]
    fn one_sided_landmarks_and_forward_inf_rule() {
        // Landmark 0 reaches node 0 and node 1 but NOT the goal (node 2), and the goal
        // cannot reach it either: its forward-INF rule prunes nodes it reaches (they
        // cannot reach the goal), nothing else. Landmark 1 is a one-way source: it
        // reaches everything (forward term usable) but nothing reaches it.
        let tab: Vec<u16> = vec![
            // lm0 fw, bw | lm1 fw, bw
            0, 0,                                  10, ALT_UNREACHABLE, // node 0
            4, 4,                                  12, ALT_UNREACHABLE, // node 1
            ALT_UNREACHABLE, ALT_UNREACHABLE,      50, ALT_UNREACHABLE, // node 2 = goal
            ALT_UNREACHABLE, ALT_UNREACHABLE,      20, ALT_UNREACHABLE, // node 3
        ];
        let lm = LandmarkHeuristic::new(4, 2, &tab, ALT_QUANTUM_MS);
        let active = lm.select_active(3, 2, usize::MAX);
        assert_eq!(active.indices, vec![0, 1]);
        assert!(lm.h_active(0, &active).is_infinite());
        assert!(lm.h_active(1, &active).is_infinite());
        // node 3: lm0 unreachable both ways (no rule fires), lm1 forward term 50-20.
        assert_eq!(lm.h_active(3, &active), ((50 - 20 - 1) as f32) * ALT_QUANTUM_MS);
        assert_eq!(lm.h_active(2, &active), 0.0);
        // The scalar subset evaluator agrees.
        let sub = lm.select_active(3, 2, 1);
        assert_eq!(sub.indices.len(), 1);
        for u in 0..4u32 {
            let h = lm.h_active(u, &sub);
            let full = lm.h_active(u, &active);
            assert!(h <= full || h.is_infinite() == full.is_infinite(), "u={u}: subset {h} > full {full}");
        }
    }

    fn random_table(nodes: usize, l: usize, seed: u32) -> Vec<u16> {
        let mut tab = vec![0u16; nodes * 2 * l];
        let mut x: u32 = seed;
        for v in tab.iter_mut() {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            *v = match (x >> 29) % 8 {
                0 => ALT_UNREACHABLE,
                1 => ALT_SATURATED,
                _ => (x % 4000) as u16,
            };
        }
        tab
    }

    /// The full-width implementations must agree bit-for-bit with the scalar reference
    /// on every lane pattern, including the UNREACHABLE/SATURATED sentinels that drive
    /// the INF and disabled-term rules.
    #[test]
    fn full_row_paths_match_scalar() {
        // 16 landmarks -> stride 32 = exactly one AVX-512 register; 32 landmarks ->
        // two, exercising the chunk loop and the per-chunk mask words.
        for l in [16usize, 32] {
            let stride = 2 * l;
            let nodes = 48usize;
            let tab = random_table(nodes, l, 0x9E3779B9);
            let lm = LandmarkHeuristic::new(nodes, l, &tab, ALT_QUANTUM_MS);
            for goal in 0..nodes as u32 {
                let active = lm.select_active(0, goal, usize::MAX);
                let Some(full) = &active.full else { continue };
                for u in 0..nodes as u32 {
                    let ub = u as usize * stride;
                    let row = &tab[ub..ub + stride];
                    let scalar = lm.h_scalar(row, &active);
                    let portable = h_full_row_portable(row, full, ALT_QUANTUM_MS);
                    assert_eq!(scalar.to_bits(), portable.to_bits(), "l={l} goal={goal} node={u}: scalar {scalar} != portable {portable}");
                    #[cfg(target_arch = "x86_64")]
                    if avx512_full_row() {
                        // SAFETY: features checked above; slice lengths match the contract.
                        let simd = unsafe {
                            h_full_row_avx512(row, &full.c, &full.valid_odd, &full.fwd_inf_even, ALT_QUANTUM_MS)
                        };
                        assert_eq!(scalar.to_bits(), simd.to_bits(), "l={l} goal={goal} node={u}: scalar {scalar} != simd {simd}");
                    }
                }
            }
        }
    }

    #[test]
    fn rev_full_row_matches_scalar_and_base_matches_full_list() {
        for l in [16usize, 32, 64] {
            let stride = 2 * l;
            let nodes = 40usize;
            let tab = random_table(nodes, l, 0x1234_5678 ^ l as u32);
            let lm = LandmarkHeuristic::new(nodes, l, &tab, ALT_QUANTUM_MS);
            let globals: Vec<(u32, f32)> = (1..9u32).map(|i| (i * 3 % nodes as u32, 600.0 * i as f32)).collect();
            for start in [0u32, 5, 17] {
                let mut anchors = vec![(start, 0.0f32)];
                anchors.extend(globals.iter().copied());
                let base = lm.rev_base(&globals);
                assert!(base.matches(globals.iter()));
                for goal in 0..nodes as u32 {
                    for k in [usize::MAX, 7] {
                        let plain = lm.select_active_rev(&anchors, goal, k);
                        let based = lm.select_active_rev_based(Some(&base), &[(start, 0.0)], goal, k);
                        assert_eq!(plain.indices, based.indices);
                        // A subset of the base's anchors re-aggregated from stored rows
                        // equals aggregating the filtered list directly.
                        let keep: Vec<bool> = (0..globals.len()).map(|i| (i + goal as usize) % 3 != 0).collect();
                        let mut filtered = vec![(start, 0.0f32)];
                        filtered.extend(globals.iter().zip(&keep).filter(|(_, &k)| k).map(|(&g, _)| g));
                        let sub_plain = lm.select_active_rev(&filtered, goal, k);
                        let sub = lm.select_active_rev_subset(&base, &keep, &[(start, 0.0)], goal, k);
                        assert_eq!(sub_plain.indices, sub.indices);
                        for u in 0..nodes as u32 {
                            let (x, y) = (lm.h_active_rev(u, &sub_plain), lm.h_active_rev(u, &sub));
                            assert_eq!(x.to_bits(), y.to_bits(), "subset l={l} goal={goal} u={u}: {x} vs {y}");
                        }
                        for u in 0..nodes as u32 {
                            let ub = u as usize * stride;
                            let row = &tab[ub..ub + stride];
                            let a = lm.h_rev_scalar(row, &plain);
                            let b = lm.h_active_rev(u, &plain);
                            let c = lm.h_active_rev(u, &based);
                            assert_eq!(a.to_bits(), b.to_bits(), "l={l} goal={goal} u={u}: scalar {a} != fast {b}");
                            assert_eq!(a.to_bits(), c.to_bits(), "l={l} goal={goal} u={u}: plain {a} != based {c}");
                        }
                    }
                }
            }
        }
    }

    /// Packed tables must evaluate bit-identically to the plain table when every value
    /// fits its cluster's offset range, and never exceed it (same INF verdicts) when
    /// some do not.
    #[test]
    fn packed_matches_plain() {
        use crate::snapshot::alt_pack::pack_alt;
        for (l, spread) in [(16usize, 200u32), (32, 200), (64, 200), (32, 5000), (64, 5000)] {
            let stride = 2 * l;
            let nodes = 70usize;
            // Values = smooth per-lane level + bounded noise, plus sentinels.
            let mut tab = vec![0u16; nodes * stride];
            let mut x: u32 = 0xC0FFEE ^ spread;
            for u in 0..nodes {
                for lane in 0..stride {
                    x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                    tab[u * stride + lane] = match (x >> 28) % 16 {
                        0 => ALT_UNREACHABLE,
                        1 => ALT_SATURATED,
                        _ => (1000 + lane as u32 * 7 + (x % spread)) as u16,
                    };
                }
            }
            let (packed, _) = pack_alt(&tab, nodes, l);
            let plain = LandmarkHeuristic::new(nodes, l, &tab, ALT_QUANTUM_MS);
            let pk = LandmarkHeuristic::new_packed(nodes, l, &packed, ALT_QUANTUM_MS);
            let exact = spread <= 200;
            for goal in (0..nodes as u32).step_by(7) {
                for k in [usize::MAX, 5] {
                    let a = plain.select_active(3, goal, k);
                    let b = pk.select_active(3, goal, k);
                    let anchors = [(3u32, 0.0f32), (11, 900.0), (40, 2400.0)];
                    let ar = plain.select_active_rev(&anchors, goal, k);
                    let br = pk.select_active_rev(&anchors, goal, k);
                    for u in 0..nodes as u32 {
                        let (hp, hk) = (plain.h_active(u, &a), pk.h_active(u, &b));
                        let (rp, rk) = (plain.h_active_rev(u, &ar), pk.h_active_rev(u, &br));
                        if exact {
                            assert_eq!(hp.to_bits(), hk.to_bits(), "l={l} goal={goal} u={u} k={k}: fwd plain {hp} packed {hk}");
                            assert_eq!(rp.to_bits(), rk.to_bits(), "l={l} goal={goal} u={u} k={k}: rev plain {rp} packed {rk}");
                        } else if k == usize::MAX {
                            // Full width: packed may only be weaker, never stronger.
                            assert!(hk <= hp, "l={l} goal={goal} u={u}: fwd packed {hk} > plain {hp}");
                            assert!(rk <= rp, "l={l} goal={goal} u={u}: rev packed {rk} > plain {rp}");
                        }
                    }
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
        let lm = LandmarkHeuristic::new(3, 1, &tab, ALT_QUANTUM_MS);
        let active = lm.select_active(0, 1, 8);
        assert!(lm.h_active(2, &active).is_infinite());
    }
}
