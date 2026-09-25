use navpath_core::snapshot::pack_coord;

use super::load_sqlite::Tile;

/// Sorted packed-coordinate index — the same binary search the snapshot reader uses —
/// replacing the SipHash `HashMap<(x,y,plane), u32>` node map (roadmap 7.4: ~36 MB +
/// ~9M hash probes at 1.1M tiles, ~142 MB live through the ALT peak at 4M). Resolution
/// results are exact-match identical, so builder output stays byte-for-byte the same.
pub struct NodeIndex<'a> {
    packed: std::borrow::Cow<'a, [u32]>,
    /// Explicit ids parallel to `packed` (test fixtures with sparse ids); `None` means
    /// the id IS the array position — the production invariant (node ids are assigned
    /// in packed-key order, which is also what the snapshot's binary search relies on).
    ids: Option<Vec<u32>>,
}

impl<'a> NodeIndex<'a> {
    /// `packed` must be strictly ascending packed coordinate keys in node-id order —
    /// exactly the `coords_packed` section the builder just constructed.
    pub fn new(packed: &'a [u32]) -> Self {
        Self { packed: std::borrow::Cow::Borrowed(packed), ids: None }
    }

    /// Owned index with explicit (coord, id) pairs — for tests and callers without the
    /// position-is-id invariant.
    pub fn from_coords(pairs: &[((i32, i32, i32), u32)]) -> NodeIndex<'static> {
        let mut entries: Vec<(u32, u32)> = pairs
            .iter()
            .map(|&((x, y, p), id)| (pack_coord(x, y, p), id))
            .collect();
        entries.sort_unstable_by_key(|&(k, _)| k);
        NodeIndex {
            packed: std::borrow::Cow::Owned(entries.iter().map(|&(k, _)| k).collect()),
            ids: Some(entries.iter().map(|&(_, id)| id).collect()),
        }
    }

    #[inline]
    pub fn get(&self, x: i32, y: i32, plane: i32) -> Option<u32> {
        // pack_coord only debug-asserts its ranges; check for real so malformed DB
        // coordinates miss instead of aliasing another tile's key in release builds.
        if !(0..32768).contains(&x) || !(0..32768).contains(&y) || !(0..4).contains(&plane) {
            return None;
        }
        let key = pack_coord(x, y, plane);
        self.packed.binary_search(&key).ok().map(|i| match &self.ids {
            Some(ids) => ids[i],
            None => i as u32,
        })
    }
}

/// The walk graph in the snapshot's v8 CSR form: `offsets` (n + 1), `dst` (one slot per
/// edge, each row in ascending direction-bit order) and a per-slot diagonal bitmap.
/// Weights are implied: `WALK_CARDINAL_MS` for cardinal slots, `walk_diagonal_ms()` for
/// diagonal ones — bit-identical to the old per-edge `cost * 300.0` weights
/// (`1.0 * 300.0` and `2f32.sqrt() * 300.0`).
pub struct WalkCsr {
    pub offsets: Vec<u32>,
    pub dst: Vec<u32>,
    pub diag: Vec<u8>,
}

impl WalkCsr {
    #[inline]
    pub fn nodes(&self) -> usize {
        self.offsets.len() - 1
    }
    #[inline]
    pub fn edges(&self) -> usize {
        self.dst.len()
    }
}

/// Direction bits: 0 left, 1 bottom, 2 right, 3 top, 4 top-left, 5 bottom-left,
/// 6 bottom-right, 7 top-right. This is also the emission order within a CSR row, which
/// canonical pruning's `popcount(mask & ((1<<d)-1))` slot addressing relies on.
pub const DIR_DELTA: [(i32, i32); 8] = [(-1, 0), (0, -1), (1, 0), (0, 1), (-1, 1), (-1, -1), (1, -1), (1, 1)];

/// Walk-rule gate for an edge u -> v in direction `d`, given both tiles' walk masks:
/// cardinals need the neighbour's reciprocal bit; diagonals need both orthogonal bits on
/// the source and both reciprocal orthogonals on the neighbour. These are the rules of
/// the old edge-list compiler, which the test module keeps verbatim as the reference.
#[inline]
fn walk_edge_ok(mu: u32, mv: u32, d: usize) -> bool {
    const L: u32 = 1 << 0;
    const B: u32 = 1 << 1;
    const R: u32 = 1 << 2;
    const T: u32 = 1 << 3;
    let has = |m: u32, bits: u32| m & bits == bits;
    match d {
        0 => has(mv, R),
        1 => has(mv, T),
        2 => has(mv, L),
        3 => has(mv, B),
        4 => has(mu, T | L) && has(mv, B | R),
        5 => has(mu, B | L) && has(mv, T | R),
        6 => has(mu, B | R) && has(mv, T | L),
        7 => has(mu, T | R) && has(mv, B | L),
        _ => false,
    }
}

/// Node id of the tile adjacent to `tiles[i]` in direction `d`, if it exists. Tiles are
/// sorted by their packed key with node id == position. Under the v9 Morton key an
/// x-step from an even x changes only key bit 0, so that neighbour — when it exists —
/// is the adjacent array slot (no key sorts strictly between k and k±1): the common
/// case is answered without a search. Every other case binary-searches the packed keys
/// exactly like [`NodeIndex::get`] (a mismatching adjacent slot proves nothing).
#[inline]
fn neighbor_of(tiles: &[Tile], packed: &[u32], i: usize, d: usize) -> Option<u32> {
    let t = tiles[i];
    let (dx, dy) = DIR_DELTA[d];
    if dy == 0 {
        let j = if dx < 0 { i.checked_sub(1) } else { Some(i + 1) };
        if let Some(n) = j.and_then(|j| tiles.get(j)) {
            if n.plane == t.plane && n.y == t.y && n.x == t.x + dx {
                return j.map(|j| j as u32);
            }
        }
    }
    let (nx, ny) = (t.x + dx, t.y + dy);
    if !(0..32768).contains(&nx) || !(0..32768).contains(&ny) || !(0..4).contains(&t.plane) {
        return None;
    }
    packed.binary_search(&pack_coord(nx, ny, t.plane)).ok().map(|j| j as u32)
}

/// Emit the walk CSR directly and in parallel (roadmap T5.4): one pass computes each
/// tile's emitted-direction byte, a prefix sum gives the offsets, a second pass writes
/// the destination slots. The result is identical to building src/dst/w edge lists in
/// tile order and counting-sorting them into CSR, which is what the builder did before
/// (pinned by `csr_matches_reference_edge_lists`); it never materializes the ~93 MB of
/// edge lists, their CSR re-sort, or the ALT stage's fwd/rev re-builds.
///
/// `packed` must be the strictly ascending packed keys of `tiles` (node id == index).
pub fn compile_walk_csr(tiles: &[Tile], packed: &[u32]) -> WalkCsr {
    use rayon::prelude::*;
    assert_eq!(tiles.len(), packed.len());
    let n = tiles.len();

    let emit: Vec<u8> = (0..n)
        .into_par_iter()
        .map(|i| {
            let mu = tiles[i].walk_mask;
            let mut bits = 0u8;
            for d in 0..8 {
                if mu & (1 << d) == 0 {
                    continue;
                }
                if let Some(j) = neighbor_of(tiles, packed, i, d) {
                    if walk_edge_ok(mu, tiles[j as usize].walk_mask, d) {
                        bits |= 1 << d;
                    }
                }
            }
            bits
        })
        .collect();

    let mut offsets = vec![0u32; n + 1];
    let mut acc: u64 = 0;
    for i in 0..n {
        offsets[i] = acc as u32;
        acc += emit[i].count_ones() as u64;
    }
    assert!(acc <= u32::MAX as u64, "walk edge count {acc} overflows the u32 CSR offsets");
    offsets[n] = acc as u32;
    let e = acc as usize;

    // Second pass: fill each row. Node-range chunks own disjoint dst sub-slices, so the
    // parallel writes need no synchronization.
    let mut dst = vec![0u32; e];
    const CHUNK: usize = 1 << 14;
    let mut parts: Vec<(usize, &mut [u32])> = Vec::with_capacity(n.div_ceil(CHUNK));
    {
        let mut rest: &mut [u32] = &mut dst;
        let mut a = 0usize;
        while a < n {
            let b = (a + CHUNK).min(n);
            let len = (offsets[b] - offsets[a]) as usize;
            let (head, tail) = rest.split_at_mut(len);
            parts.push((a, head));
            rest = tail;
            a = b;
        }
    }
    parts.into_par_iter().for_each(|(a, out)| {
        let b = (a + CHUNK).min(n);
        let mut k = 0usize;
        for i in a..b {
            let mut bits = emit[i];
            while bits != 0 {
                let d = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                out[k] = neighbor_of(tiles, packed, i, d).expect("neighbour vanished between passes");
                k += 1;
            }
        }
        debug_assert_eq!(k, out.len());
    });

    // Diagonal bitmap: slots within a row are in ascending direction-bit order, and bits
    // 4..7 are the diagonals.
    let mut diag = vec![0u8; e.div_ceil(8)];
    let mut slot = 0usize;
    for &bits in &emit {
        let mut b = bits;
        while b != 0 {
            let d = b.trailing_zeros();
            b &= b - 1;
            if d >= 4 {
                diag[slot / 8] |= 1 << (slot % 8);
            }
            slot += 1;
        }
    }
    debug_assert_eq!(slot, e);

    WalkCsr { offsets, dst, diag }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-T5.4 edge-list compiler, verbatim, as the reference for the direct CSR.
    fn compile_walk_edges(tiles: &[Tile], node_id_of: &NodeIndex) -> (Vec<u32>, Vec<u32>, Vec<f32>) {
        let mut src: Vec<u32> = Vec::new();
        let mut dst: Vec<u32> = Vec::new();
        let mut w: Vec<f32> = Vec::new();
        const LEFT: usize = 0;
        const BOTTOM: usize = 1;
        const RIGHT: usize = 2;
        const TOP: usize = 3;
        const TOPLEFT: usize = 4;
        const BOTTOMLEFT: usize = 5;
        const BOTTOMRIGHT: usize = 6;
        const TOPRIGHT: usize = 7;
        let dirs = [
            (LEFT, -1, 0, RIGHT, 1.0_f32),
            (BOTTOM, 0, -1, TOP, 1.0_f32),
            (RIGHT, 1, 0, LEFT, 1.0_f32),
            (TOP, 0, 1, BOTTOM, 1.0_f32),
            (TOPLEFT, -1, 1, TOPRIGHT, 2_f32.sqrt()),
            (BOTTOMLEFT, -1, -1, TOPRIGHT, 2_f32.sqrt()),
            (BOTTOMRIGHT, 1, -1, TOPLEFT, 2_f32.sqrt()),
            (TOPRIGHT, 1, 1, BOTTOMLEFT, 2_f32.sqrt()),
        ];
        let diag_require = [
            (TOPLEFT, TOP, LEFT),
            (BOTTOMLEFT, BOTTOM, LEFT),
            (BOTTOMRIGHT, BOTTOM, RIGHT),
            (TOPRIGHT, TOP, RIGHT),
        ];
        let has_bit = |mask: u32, bit: usize| -> bool { (mask & (1u32 << bit)) != 0 };
        for (i, t) in tiles.iter().enumerate() {
            let sid = i as u32;
            let mask = t.walk_mask;
            for &(bit, dx, dy, recip_bit, cost) in &dirs {
                if !has_bit(mask, bit) {
                    continue;
                }
                if let Some(did) = node_id_of.get(t.x + dx, t.y + dy, t.plane) {
                    let neighbor_mask = tiles[did as usize].walk_mask;
                    let reciprocal_ok = match bit {
                        LEFT | RIGHT | TOP | BOTTOM => has_bit(neighbor_mask, recip_bit),
                        _ => true,
                    };
                    if !reciprocal_ok {
                        continue;
                    }
                    let mut diag_ok = true;
                    if let TOPLEFT | BOTTOMLEFT | BOTTOMRIGHT | TOPRIGHT = bit {
                        let (o1, o2) = diag_require
                            .iter()
                            .find(|(b, _, _)| *b == bit)
                            .map(|(_, a, b)| (*a, *b))
                            .unwrap();
                        if !has_bit(mask, o1) || !has_bit(mask, o2) {
                            diag_ok = false;
                        } else {
                            let recip = |b: usize| match b {
                                LEFT => RIGHT,
                                RIGHT => LEFT,
                                TOP => BOTTOM,
                                BOTTOM => TOP,
                                _ => b,
                            };
                            if !has_bit(neighbor_mask, recip(o1)) || !has_bit(neighbor_mask, recip(o2)) {
                                diag_ok = false;
                            }
                        }
                    }
                    if !diag_ok {
                        continue;
                    }
                    src.push(sid);
                    dst.push(did);
                    w.push(cost * 300.0);
                }
            }
        }
        (src, dst, w)
    }

    /// The old main.rs counting-sort CSR emission over those edge lists.
    fn reference_csr(tiles: &[Tile], packed: &[u32]) -> (Vec<u32>, Vec<u32>, Vec<u8>, Vec<f32>) {
        let idx = NodeIndex::new(packed);
        let (src, dst, w) = compile_walk_edges(tiles, &idx);
        let n = tiles.len();
        let mut offsets = vec![0u32; n + 1];
        for &s in &src {
            offsets[s as usize + 1] += 1;
        }
        for i in 0..n {
            offsets[i + 1] += offsets[i];
        }
        let mut cur = offsets.clone();
        let mut out = vec![0u32; src.len()];
        let mut diag = vec![0u8; src.len().div_ceil(8)];
        let mut ws = vec![0f32; src.len()];
        for i in 0..src.len() {
            let slot = cur[src[i] as usize] as usize;
            out[slot] = dst[i];
            ws[slot] = w[i];
            if w[i] > 350.0 {
                diag[slot / 8] |= 1 << (slot % 8);
            }
            cur[src[i] as usize] += 1;
        }
        (offsets, out, diag, ws)
    }

    fn lcg(state: &mut u64) -> u64 {
        *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *state >> 33
    }

    #[test]
    fn csr_matches_reference_edge_lists() {
        // Random sparse tiles over a small window (x=0/y=0 borders, two planes) with
        // random masks: exercises every direction rule and missing neighbours.
        for seed in 1..8u64 {
            let mut st = seed;
            let mut tiles: Vec<Tile> = Vec::new();
            for p in 0..2 {
                for y in 0..40 {
                    for x in 0..40 {
                        if lcg(&mut st) % 100 < 70 {
                            let walk_mask =
                                if lcg(&mut st) % 4 == 0 { 0xFF } else { (lcg(&mut st) & 0xFF) as u32 };
                            tiles.push(Tile { x, y, plane: p, walk_mask });
                        }
                    }
                }
            }
            tiles.sort_by_key(|t| pack_coord(t.x, t.y, t.plane));
            let packed: Vec<u32> = tiles.iter().map(|t| pack_coord(t.x, t.y, t.plane)).collect();
            let (ro, rd, rdiag, rw) = reference_csr(&tiles, &packed);
            let csr = compile_walk_csr(&tiles, &packed);
            assert_eq!(csr.offsets, ro, "offsets differ (seed {seed})");
            assert_eq!(csr.dst, rd, "dst differ (seed {seed})");
            assert_eq!(csr.diag, rdiag, "diag differ (seed {seed})");
            // Implied weights equal the old per-edge weights bit for bit.
            let wd = navpath_core::snapshot::walk_diagonal_ms();
            for (slot, &w) in rw.iter().enumerate() {
                let d = (csr.diag[slot / 8] >> (slot % 8)) & 1 == 1;
                let implied = if d { wd } else { navpath_core::snapshot::WALK_CARDINAL_MS };
                assert_eq!(implied.to_bits(), w.to_bits());
            }
        }
    }
}
