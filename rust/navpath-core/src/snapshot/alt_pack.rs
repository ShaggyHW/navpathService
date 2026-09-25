//! Clustered u8 encoding of the ALT table (snapshot `alt_format = 1`).
//!
//! The plain table stores every (node, landmark, direction) distance as a u16 quantum:
//! 4 B per landmark per node, a 256 B row (4 cache lines) at 64 landmarks — 87% of the
//! snapshot and the dominant source of cold-page faults. Distances of spatially close
//! nodes are close, so rows are stored as u8 OFFSETS from a per-cluster u16 base:
//!
//! ```text
//! record(c) = [ base: S x u16 ][ node 0: S x u8 ] ... [ node C-1: S x u8 ]
//!             S = 2 * landmarks lanes (interleaved fw, bw as in the plain table),
//!             C = ALT_CLUSTER consecutive node ids (spatially clustered under the
//!                 Morton numbering of snapshot v9)
//! ```
//!
//! 64 landmarks: 256 B base + 16 x 128 B rows = 2,304 B per 16 nodes (144 B/node, 44%
//! smaller), and a heuristic evaluation reads one 2-line row plus the cluster's base
//! (shared by the cluster's 16 nodes, so usually cache-resident).
//!
//! Per-lane codes:
//! - `0..=OFF_MAX`: exact value `base + off`;
//! - [`CODE_UNKNOWN`]: an exact FINITE value that did not fit the offset range;
//! - [`CODE_SAT`] / [`CODE_UNREACH`]: [`ALT_SATURATED`] / [`ALT_UNREACHABLE`].
//!
//! `base` is the minimum exact (< SATURATED) value of the lane over the cluster, so
//! UNKNOWN is rare (a lane whose values spread over more than OFF_MAX quanta — ~16 s —
//! inside one cluster, i.e. walls / one-way edges; ~1.3% of exact lanes under Morton
//! numbering). An UNKNOWN value is known to be finite (reachability facts stay exact).
//! The hot per-node evaluators do not look its magnitude up: they substitute the value
//! that disables the lane's NUMERIC term for its role while keeping reachability right
//! — see [`Unknown`] — so bounds only weaken and admissibility is preserved. The exact
//! values ARE kept, in a sorted exception list after the cluster records, and every
//! per-QUERY row decode (goal row, anchors, start scoring) uses them via
//! [`PackedAlt::decode_row_exact`]: an inexact goal row would weaken the heuristic of
//! every node of that query (measured: three goals next to walls went from ~4k to
//! ~750k pops before exceptions existed).
//!
//! Section layout: `[cluster records][exceptions: (node u32, lane u16, value u16) LE,
//! sorted by (node, lane)]`; the exception count is stored in the snapshot header.

use super::manifest::{ALT_SATURATED, ALT_UNREACHABLE};

/// Nodes per cluster record.
pub const ALT_CLUSTER: usize = 16;
/// Largest exact offset.
pub const OFF_MAX: u8 = 252;
/// Finite value outside the cluster's offset range.
pub const CODE_UNKNOWN: u8 = 253;
/// [`ALT_SATURATED`].
pub const CODE_SAT: u8 = 254;
/// [`ALT_UNREACHABLE`].
pub const CODE_UNREACH: u8 = 255;

/// How a reader substitutes an [`CODE_UNKNOWN`] lane, per lane parity (even = fw =
/// d(L, n), odd = bw = d(n, L)). The substitute must never make a bound larger:
/// SATURATED disables a lane that is used as a subtrahend or is only usable when exact,
/// and 0 is a safe understatement of a minuend. Neither substitute is UNREACHABLE, so
/// the INF rules (which only need reachability) stay exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unknown {
    pub even: u16,
    pub odd: u16,
}

impl Unknown {
    /// A node row in the FORWARD heuristic: fu is the forward term's subtrahend (must
    /// not be understated) -> SATURATED; bu is the backward term's minuend -> 0.
    pub const FWD_NODE: Unknown = Unknown { even: ALT_SATURATED, odd: 0 };
    /// The goal row in forward selection: gfw is a minuend -> 0; gbw is the backward
    /// subtrahend and must be exact -> SATURATED (disables the term, keeps the goal
    /// "reaches L" fact).
    pub const GOAL: Unknown = Unknown { even: 0, odd: ALT_SATURATED };
    /// A node row in the BACKWARD (bidirectional) heuristic: fv is used only when exact
    /// and bv is a subtrahend -> both SATURATED.
    pub const REV_NODE: Unknown = Unknown { even: ALT_SATURATED, odd: ALT_SATURATED };
    /// An anchor row in the backward aggregation: fa is a subtrahend -> SATURATED
    /// (invalidates the a-side); ba is a minuend -> 0.
    pub const ANCHOR: Unknown = Unknown { even: ALT_SATURATED, odd: 0 };
}

/// Bytes of one cluster record for `landmarks` landmarks.
#[inline]
pub fn record_bytes(landmarks: usize) -> usize {
    let s = 2 * landmarks;
    2 * s + ALT_CLUSTER * s
}

/// Size of the cluster records for `nodes` nodes (the exception list follows).
#[inline]
pub fn packed_bytes(nodes: usize, landmarks: usize) -> usize {
    nodes.div_ceil(ALT_CLUSTER) * record_bytes(landmarks)
}

/// Bytes per exception entry.
pub const EXCEPTION_BYTES: usize = 8;

/// Encode a plain interleaved u16 table (`nodes * 2 * landmarks` entries). Returns the
/// section bytes (cluster records followed by the exception list) and the number of
/// exceptions.
pub fn pack_alt(tab: &[u16], nodes: usize, landmarks: usize) -> (Vec<u8>, u32) {
    let s = 2 * landmarks;
    assert_eq!(tab.len(), nodes * s, "plain ALT table size mismatch");
    let rec = record_bytes(landmarks);
    let clusters = nodes.div_ceil(ALT_CLUSTER);
    let mut out = vec![0u8; clusters * rec];
    let mut exceptions: Vec<(u32, u16, u16)> = Vec::new();
    for (c, r) in out.chunks_mut(rec).enumerate() {
        let first = c * ALT_CLUSTER;
        let last = (first + ALT_CLUSTER).min(nodes);
        let (base_bytes, rows) = r.split_at_mut(2 * s);
        for lane in 0..s {
            let mut base = u16::MAX;
            for u in first..last {
                let v = tab[u * s + lane];
                if v < ALT_SATURATED && v < base {
                    base = v;
                }
            }
            if base == u16::MAX {
                base = 0;
            }
            base_bytes[2 * lane..2 * lane + 2].copy_from_slice(&base.to_le_bytes());
            for u in first..last {
                let v = tab[u * s + lane];
                let code = if v == ALT_UNREACHABLE {
                    CODE_UNREACH
                } else if v == ALT_SATURATED {
                    CODE_SAT
                } else if v - base <= OFF_MAX as u16 {
                    (v - base) as u8
                } else {
                    exceptions.push((u as u32, lane as u16, v));
                    CODE_UNKNOWN
                };
                rows[(u - first) * s + lane] = code;
            }
            // Padding nodes of the last cluster: unreachable (never read).
            for u in last..first + ALT_CLUSTER {
                rows[(u - first) * s + lane] = CODE_UNREACH;
            }
        }
    }
    exceptions.sort_unstable();
    let count = u32::try_from(exceptions.len()).expect("ALT exception count exceeds u32");
    out.reserve(exceptions.len() * EXCEPTION_BYTES);
    for (node, lane, value) in exceptions {
        out.extend_from_slice(&node.to_le_bytes());
        out.extend_from_slice(&lane.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    (out, count)
}

/// Borrowed packed table.
#[derive(Clone, Copy)]
pub struct PackedAlt<'a> {
    /// Cluster records.
    pub data: &'a [u8],
    pub landmarks: usize,
    /// Exception entries ([`EXCEPTION_BYTES`] each, sorted by (node, lane)).
    pub exceptions: &'a [u8],
}

impl<'a> PackedAlt<'a> {
    /// Split a packed section (records + exceptions) for `nodes` nodes.
    pub fn from_section(section: &'a [u8], nodes: usize, landmarks: usize) -> Self {
        let rec = packed_bytes(nodes, landmarks);
        assert!(section.len() >= rec, "packed ALT section shorter than its cluster records");
        let exc = &section[rec..];
        let exc = &exc[..exc.len() / EXCEPTION_BYTES * EXCEPTION_BYTES];
        PackedAlt { data: &section[..rec], landmarks, exceptions: exc }
    }

    #[inline]
    fn exception(&self, i: usize) -> (u32, u16, u16) {
        let e = &self.exceptions[i * EXCEPTION_BYTES..(i + 1) * EXCEPTION_BYTES];
        (
            u32::from_le_bytes([e[0], e[1], e[2], e[3]]),
            u16::from_le_bytes([e[4], e[5]]),
            u16::from_le_bytes([e[6], e[7]]),
        )
    }

    /// Decode node `u`'s row EXACTLY (UNKNOWN lanes resolved from the exception list):
    /// identical to the plain table's row. For per-query rows; the hot per-node paths
    /// use the substituting decoders.
    pub fn decode_row_exact(&self, u: usize, out: &mut [u16]) {
        self.decode_row(u, Unknown::FWD_NODE, out);
        let n = self.exceptions.len() / EXCEPTION_BYTES;
        // First exception of node u.
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.exception(mid).0 < u as u32 { lo = mid + 1 } else { hi = mid }
        }
        let mut i = lo;
        while i < n {
            let (node, lane, value) = self.exception(i);
            if node != u as u32 {
                break;
            }
            out[lane as usize] = value;
            i += 1;
        }
    }
    /// (cluster base lanes as little-endian u16 bytes, node's offset row).
    #[inline(always)]
    pub fn parts(&self, u: usize) -> (&'a [u8], &'a [u8]) {
        let s = 2 * self.landmarks;
        let rec = record_bytes(self.landmarks);
        let c = u / ALT_CLUSTER;
        let i = u % ALT_CLUSTER;
        let r = &self.data[c * rec..(c + 1) * rec];
        (&r[..2 * s], &r[2 * s + i * s..2 * s + (i + 1) * s])
    }

    /// Decode node `u`'s row into `out` (`2 * landmarks` lanes) with the given
    /// UNKNOWN substitution.
    pub fn decode_row(&self, u: usize, unk: Unknown, out: &mut [u16]) {
        let (base, row) = self.parts(u);
        for lane in 0..row.len() {
            let code = row[lane];
            out[lane] = match code {
                CODE_UNREACH => ALT_UNREACHABLE,
                CODE_SAT => ALT_SATURATED,
                CODE_UNKNOWN => if lane % 2 == 0 { unk.even } else { unk.odd },
                off => u16::from_le_bytes([base[2 * lane], base[2 * lane + 1]]) + off as u16,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_exact_and_specials() {
        let landmarks = 2;
        let nodes = 21; // one full cluster + a partial one
        let s = 2 * landmarks;
        let mut tab = vec![0u16; nodes * s];
        for u in 0..nodes {
            tab[u * s] = 1000 + u as u16; // exact small spread
            tab[u * s + 1] = if u % 5 == 0 { ALT_UNREACHABLE } else { 7 };
            tab[u * s + 2] = if u == 3 { 4000 } else { 10 }; // out of range -> UNKNOWN
            tab[u * s + 3] = if u == 4 { ALT_SATURATED } else { 20 };
        }
        let (packed, exc) = pack_alt(&tab, nodes, landmarks);
        assert_eq!(exc, 1);
        assert_eq!(packed.len(), packed_bytes(nodes, landmarks) + EXCEPTION_BYTES);
        let p = PackedAlt::from_section(&packed, nodes, landmarks);
        let mut row = vec![0u16; s];
        for u in 0..nodes {
            p.decode_row(u, Unknown::FWD_NODE, &mut row);
            assert_eq!(row[0], tab[u * s]);
            assert_eq!(row[1], tab[u * s + 1]);
            if u == 3 {
                assert_eq!(row[2], Unknown::FWD_NODE.even);
            } else {
                assert_eq!(row[2], tab[u * s + 2]);
            }
            assert_eq!(row[3], tab[u * s + 3]);
            p.decode_row_exact(u, &mut row);
            assert_eq!(&row[..], &tab[u * s..(u + 1) * s], "exact decode of node {u}");
        }
    }
}
