use byteorder::{ByteOrder, LittleEndian};

pub const SNAPSHOT_MAGIC: [u8; 4] = *b"NPSS"; // NavPath SnapShot
// v8: format overhaul for read-side speed and 4M-tile scale:
//  - node coords packed into one u32 section (plane<<30 | y<<15 | x), ascending in node
//    id order, so coordinate->id lookup is a binary search over the mmap (no heap index);
//  - walk graph stored CSR-native (offsets + dst + per-edge diagonal bitmap; weights are
//    derived: 300ms cardinal / 300*sqrt(2) diagonal) — no load-time CSR rebuild;
//  - ALT tables quantized to u16 in ALT_QUANTUM_MS units and interleaved
//    [node][landmark][fw,bw] so one heuristic call reads one contiguous row;
//  - per-node walk-component ids (u16) for reachability prechecks;
//  - every section start is 64-byte aligned, enabling zero-copy typed slices.
// Snapshots must be rebuilt.
pub const SNAPSHOT_VERSION: u32 = 8;

/// Quantum for the u16 ALT tables, in milliseconds. Stored values are
/// floor(distance_ms / quantum); 0xFFFF marks unreachable and 0xFFFE saturation
/// (distance >= 0xFFFE quanta). Readers subtract one quantum from derived bounds to
/// preserve admissibility under the floor rounding, and must never use a SATURATED
/// entry where understating a distance would OVERstate a bound: the forward d(L,u)
/// side of the max, and BOTH goal entries (select_active drops saturated landmarks
/// for the query — measured live as a suboptimal-route bug when this was violated).
///
/// 64ms gives a 4.19M ms range (~14,000 walk tiles), which no in-map distance reaches
/// today, so saturation is a safety net rather than a working state. A 16ms quantum was
/// measured to give no fewer expansions (the equal-cost plateau dominates, not the
/// quantization slack) while shrinking the range enough to actually saturate.
pub const ALT_QUANTUM_MS: f32 = 64.0;
/// Sentinel for "unreachable" in the quantized ALT tables.
pub const ALT_UNREACHABLE: u16 = u16::MAX;
/// Saturation marker: the true distance is at least this many quanta.
pub const ALT_SATURATED: u16 = u16::MAX - 1;

/// Walk-edge weights are not stored: cardinal steps cost 300ms, diagonal steps
/// 300*sqrt(2), selected by the per-edge diagonal bitmap.
pub const WALK_CARDINAL_MS: f32 = 300.0;

#[inline]
pub fn walk_diagonal_ms() -> f32 {
    2f32.sqrt() * WALK_CARDINAL_MS
}

/// Pack node coordinates into the v8 key: plane<<30 | y<<15 | x. Key order equals
/// (plane, y, x) lexicographic order, which is also node-id assignment order.
#[inline]
pub fn pack_coord(x: i32, y: i32, plane: i32) -> u32 {
    debug_assert!((0..32768).contains(&x) && (0..32768).contains(&y) && (0..4).contains(&plane));
    ((plane as u32) << 30) | ((y as u32) << 15) | (x as u32)
}

#[inline]
pub fn unpack_coord(key: u32) -> (i32, i32, i32) {
    ((key & 0x7FFF) as i32, ((key >> 15) & 0x7FFF) as i32, (key >> 30) as i32)
}

/// Alignment for every section start (and the header size), so mmap'd sections can be
/// reinterpreted as typed slices.
pub const SECTION_ALIGN: u64 = 64;

#[inline]
pub fn align_up(off: u64) -> u64 {
    (off + (SECTION_ALIGN - 1)) & !(SECTION_ALIGN - 1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCounts {
    pub nodes: u32,
    pub walk_edges: u32,
    pub macro_edges: u32,
    pub req_tags: u32,     // number of u32 entries (not necessarily triplets)
    pub landmarks: u32,
    pub fairy_rings: u32,  // number of fairy ring nodes
    pub walk_components: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Manifest {
    pub version: u32,
    pub counts: SnapshotCounts,
    pub off_coords: u64,
    pub off_walk_offsets: u64,
    pub off_walk_dst: u64,
    pub off_walk_diag: u64,
    pub off_comp: u64,
    pub off_macro_src: u64,
    pub off_macro_dst: u64,
    pub off_macro_w: u64,
    pub off_macro_kind_first: u64,
    pub off_macro_id_first: u64,
    pub off_macro_meta_offs: u64,
    pub off_macro_meta_lens: u64,
    pub off_macro_meta_blob: u64,
    pub off_req_tags: u64,
    pub off_landmarks: u64,
    pub off_lm_tab: u64,
    pub off_fairy_nodes: u64,
    pub off_fairy_cost_ms: u64,
    pub off_fairy_meta_offs: u64,
    pub off_fairy_meta_lens: u64,
    pub off_fairy_meta_blob: u64,
}

pub const MANIFEST_OFFSET_COUNT: usize = 21;

impl Manifest {
    /// Header occupies one aligned block: 4 magic + 4 version + 7*4 counts + 21*8 offsets
    /// = 204 bytes, padded to the section alignment.
    pub const SIZE: usize = 256;

    pub fn offsets(&self) -> [u64; MANIFEST_OFFSET_COUNT] {
        [
            self.off_coords,
            self.off_walk_offsets,
            self.off_walk_dst,
            self.off_walk_diag,
            self.off_comp,
            self.off_macro_src,
            self.off_macro_dst,
            self.off_macro_w,
            self.off_macro_kind_first,
            self.off_macro_id_first,
            self.off_macro_meta_offs,
            self.off_macro_meta_lens,
            self.off_macro_meta_blob,
            self.off_req_tags,
            self.off_landmarks,
            self.off_lm_tab,
            self.off_fairy_nodes,
            self.off_fairy_cost_ms,
            self.off_fairy_meta_offs,
            self.off_fairy_meta_lens,
            self.off_fairy_meta_blob,
        ]
    }

    pub fn parse(header: &[u8]) -> Result<Self, ManifestError> {
        if header.len() < Self::SIZE { return Err(ManifestError::HeaderTooSmall); }
        if header[0..4] != SNAPSHOT_MAGIC { return Err(ManifestError::BadMagic); }
        let version = LittleEndian::read_u32(&header[4..8]);
        if version != SNAPSHOT_VERSION { return Err(ManifestError::UnsupportedVersion(version)); }
        let c0 = 8;
        let counts = SnapshotCounts {
            nodes: LittleEndian::read_u32(&header[c0..c0 + 4]),
            walk_edges: LittleEndian::read_u32(&header[c0 + 4..c0 + 8]),
            macro_edges: LittleEndian::read_u32(&header[c0 + 8..c0 + 12]),
            req_tags: LittleEndian::read_u32(&header[c0 + 12..c0 + 16]),
            landmarks: LittleEndian::read_u32(&header[c0 + 16..c0 + 20]),
            fairy_rings: LittleEndian::read_u32(&header[c0 + 20..c0 + 24]),
            walk_components: LittleEndian::read_u32(&header[c0 + 24..c0 + 28]),
        };
        let o0 = c0 + 28;
        let mut offs = [0u64; MANIFEST_OFFSET_COUNT];
        for (i, o) in offs.iter_mut().enumerate() {
            *o = LittleEndian::read_u64(&header[o0 + i * 8..o0 + i * 8 + 8]);
        }
        Ok(Manifest {
            version,
            counts,
            off_coords: offs[0],
            off_walk_offsets: offs[1],
            off_walk_dst: offs[2],
            off_walk_diag: offs[3],
            off_comp: offs[4],
            off_macro_src: offs[5],
            off_macro_dst: offs[6],
            off_macro_w: offs[7],
            off_macro_kind_first: offs[8],
            off_macro_id_first: offs[9],
            off_macro_meta_offs: offs[10],
            off_macro_meta_lens: offs[11],
            off_macro_meta_blob: offs[12],
            off_req_tags: offs[13],
            off_landmarks: offs[14],
            off_lm_tab: offs[15],
            off_fairy_nodes: offs[16],
            off_fairy_cost_ms: offs[17],
            off_fairy_meta_offs: offs[18],
            off_fairy_meta_lens: offs[19],
            off_fairy_meta_blob: offs[20],
        })
    }

    pub fn write_header(&self) -> Vec<u8> {
        let mut header = vec![0u8; Self::SIZE];
        header[0..4].copy_from_slice(&SNAPSHOT_MAGIC);
        LittleEndian::write_u32(&mut header[4..8], self.version);
        let c0 = 8;
        LittleEndian::write_u32(&mut header[c0..c0 + 4], self.counts.nodes);
        LittleEndian::write_u32(&mut header[c0 + 4..c0 + 8], self.counts.walk_edges);
        LittleEndian::write_u32(&mut header[c0 + 8..c0 + 12], self.counts.macro_edges);
        LittleEndian::write_u32(&mut header[c0 + 12..c0 + 16], self.counts.req_tags);
        LittleEndian::write_u32(&mut header[c0 + 16..c0 + 20], self.counts.landmarks);
        LittleEndian::write_u32(&mut header[c0 + 20..c0 + 24], self.counts.fairy_rings);
        LittleEndian::write_u32(&mut header[c0 + 24..c0 + 28], self.counts.walk_components);
        let o0 = c0 + 28;
        for (i, o) in self.offsets().iter().enumerate() {
            LittleEndian::write_u64(&mut header[o0 + i * 8..o0 + i * 8 + 8], *o);
        }
        header
    }

    pub fn validate_layout(&self, file_len: usize) -> Result<(), ManifestError> {
        fn fits(off: u64, bytes: usize, file_len: usize) -> bool {
            let off = off as usize;
            off <= file_len && file_len - off >= bytes
        }
        let n = self.counts.nodes as usize;
        let e = self.counts.walk_edges as usize;
        let m = self.counts.macro_edges as usize;
        let fr = self.counts.fairy_rings as usize;
        let lm = self.counts.landmarks as usize;

        let checks: [(&'static str, u64, usize); 12] = [
            ("coords", self.off_coords, n * 4),
            ("walk_offsets", self.off_walk_offsets, (n + 1) * 4),
            ("walk_dst", self.off_walk_dst, e * 4),
            ("walk_diag", self.off_walk_diag, e.div_ceil(8)),
            ("comp", self.off_comp, n * 2),
            ("macro_src", self.off_macro_src, m * 4),
            ("macro_dst", self.off_macro_dst, m * 4),
            ("macro_w", self.off_macro_w, m * 4),
            ("req_tags", self.off_req_tags, self.counts.req_tags as usize * 4),
            ("landmarks", self.off_landmarks, lm * 4),
            ("lm_tab", self.off_lm_tab, n.saturating_mul(lm) * 4), // 2 u16 per (node, lm)
            ("fairy_nodes", self.off_fairy_nodes, fr * 4),
        ];
        for (name, off, bytes) in checks {
            if !fits(off, bytes, file_len) { return Err(ManifestError::OutOfBounds(name)); }
        }
        for (name, off, bytes) in [
            ("macro_kind_first", self.off_macro_kind_first, m * 4),
            ("macro_id_first", self.off_macro_id_first, m * 4),
            ("macro_meta_offs", self.off_macro_meta_offs, m * 4),
            ("macro_meta_lens", self.off_macro_meta_lens, m * 4),
            ("fairy_cost_ms", self.off_fairy_cost_ms, fr * 4),
            ("fairy_meta_offs", self.off_fairy_meta_offs, fr * 4),
            ("fairy_meta_lens", self.off_fairy_meta_lens, fr * 4),
        ] {
            if !fits(off, bytes, file_len) { return Err(ManifestError::OutOfBounds(name)); }
        }
        // blobs are variable length; ensure starts are within the file
        if (self.off_macro_meta_blob as usize) > file_len { return Err(ManifestError::OutOfBounds("macro_meta_blob")); }
        if (self.off_fairy_meta_blob as usize) > file_len { return Err(ManifestError::OutOfBounds("fairy_meta_blob")); }
        // alignment: hot typed sections must sit on SECTION_ALIGN boundaries
        for (name, off) in [
            ("coords", self.off_coords),
            ("walk_offsets", self.off_walk_offsets),
            ("walk_dst", self.off_walk_dst),
            ("comp", self.off_comp),
            ("req_tags", self.off_req_tags),
            ("lm_tab", self.off_lm_tab),
        ] {
            if off % SECTION_ALIGN != 0 { return Err(ManifestError::Misaligned(name)); }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("snapshot header too small")]
    HeaderTooSmall,
    #[error("bad magic")]
    BadMagic,
    #[error("unsupported version {0}")]
    UnsupportedVersion(u32),
    #[error("section out of bounds: {0}")]
    OutOfBounds(&'static str),
    #[error("section misaligned: {0}")]
    Misaligned(&'static str),
}
