use byteorder::{ByteOrder, LittleEndian};

pub const SNAPSHOT_MAGIC: [u8; 4] = *b"NPSS"; // NavPath SnapShot
pub const SNAPSHOT_VERSION: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCounts {
    pub nodes: u32,
    pub walk_edges: u32,
    pub macro_edges: u32,
    pub req_tags: u32,     // number of u32 entries (not necessarily triplets)
    pub landmarks: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Manifest {
    pub version: u32,
    pub counts: SnapshotCounts,
    pub off_nodes_ids: u64,
    pub off_nodes_x: u64,
    pub off_nodes_y: u64,
    pub off_nodes_plane: u64,
    pub off_walk_src: u64,
    pub off_walk_dst: u64,
    pub off_walk_w: u64,
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
    pub off_lm_fw: u64,
    pub off_lm_bw: u64,
}

impl Manifest {
    pub const SIZE: usize = 4 /*magic*/ + 4 /*version*/ + 5*4 /*counts*/ + 19*8 /*offsets*/;

    pub fn parse(header: &[u8]) -> Result<Self, ManifestError> {
        if header.len() < Self::SIZE { return Err(ManifestError::HeaderTooSmall); }
        if header[0..4] != SNAPSHOT_MAGIC { return Err(ManifestError::BadMagic); }
        let version = LittleEndian::read_u32(&header[4..8]);
        if version != SNAPSHOT_VERSION { return Err(ManifestError::UnsupportedVersion(version)); }
        let c0 = 8;
        let nodes = LittleEndian::read_u32(&header[c0..c0+4]);
        let walk_edges = LittleEndian::read_u32(&header[c0+4..c0+8]);
        let macro_edges = LittleEndian::read_u32(&header[c0+8..c0+12]);
        let req_tags = LittleEndian::read_u32(&header[c0+12..c0+16]);
        let landmarks = LittleEndian::read_u32(&header[c0+16..c0+20]);
        let o0 = c0 + 20;
        let off_nodes_ids = LittleEndian::read_u64(&header[o0..o0+8]);
        let off_nodes_x = LittleEndian::read_u64(&header[o0+8..o0+16]);
        let off_nodes_y = LittleEndian::read_u64(&header[o0+16..o0+24]);
        let off_nodes_plane = LittleEndian::read_u64(&header[o0+24..o0+32]);
        let off_walk_src = LittleEndian::read_u64(&header[o0+32..o0+40]);
        let off_walk_dst = LittleEndian::read_u64(&header[o0+40..o0+48]);
        let off_walk_w = LittleEndian::read_u64(&header[o0+48..o0+56]);
        let off_macro_src = LittleEndian::read_u64(&header[o0+56..o0+64]);
        let off_macro_dst = LittleEndian::read_u64(&header[o0+64..o0+72]);
        let off_macro_w = LittleEndian::read_u64(&header[o0+72..o0+80]);
        let off_macro_kind_first = LittleEndian::read_u64(&header[o0+80..o0+88]);
        let off_macro_id_first = LittleEndian::read_u64(&header[o0+88..o0+96]);
        let off_macro_meta_offs = LittleEndian::read_u64(&header[o0+96..o0+104]);
        let off_macro_meta_lens = LittleEndian::read_u64(&header[o0+104..o0+112]);
        let off_macro_meta_blob = LittleEndian::read_u64(&header[o0+112..o0+120]);
        let off_req_tags = LittleEndian::read_u64(&header[o0+120..o0+128]);
        let off_landmarks = LittleEndian::read_u64(&header[o0+128..o0+136]);
        let off_lm_fw = LittleEndian::read_u64(&header[o0+136..o0+144]);
        let off_lm_bw = LittleEndian::read_u64(&header[o0+144..o0+152]);
        Ok(Manifest {
            version,
            counts: SnapshotCounts { nodes, walk_edges, macro_edges, req_tags, landmarks },
            off_nodes_ids,
            off_nodes_x,
            off_nodes_y,
            off_nodes_plane,
            off_walk_src,
            off_walk_dst,
            off_walk_w,
            off_macro_src,
            off_macro_dst,
            off_macro_w,
            off_macro_kind_first,
            off_macro_id_first,
            off_macro_meta_offs,
            off_macro_meta_lens,
            off_macro_meta_blob,
            off_req_tags,
            off_landmarks,
            off_lm_fw,
            off_lm_bw,
        })
    }

    pub fn validate_layout(&self, file_len: usize) -> Result<(), ManifestError> {
        // helper to check a region fits
        fn fits(off: u64, bytes: usize, file_len: usize) -> bool {
            let off = off as usize;
            off <= file_len && file_len - off >= bytes
        }
        let u32b = core::mem::size_of::<u32>();
        let f32b = core::mem::size_of::<f32>();
        let nodes_bytes_u32 = (self.counts.nodes as usize) * u32b;
        let nodes_bytes_i32 = (self.counts.nodes as usize) * u32b; // same size
        let walk_src_bytes = (self.counts.walk_edges as usize) * u32b;
        let walk_dst_bytes = (self.counts.walk_edges as usize) * u32b;
        let walk_w_bytes = (self.counts.walk_edges as usize) * f32b;
        let macro_src_bytes = (self.counts.macro_edges as usize) * u32b;
        let macro_dst_bytes = (self.counts.macro_edges as usize) * u32b;
        let macro_w_bytes = (self.counts.macro_edges as usize) * f32b;
        let macro_kind_first_bytes = (self.counts.macro_edges as usize) * u32b; // enum as u32
        let macro_id_first_bytes = (self.counts.macro_edges as usize) * u32b; // id truncated to u32
        let req_bytes = (self.counts.req_tags as usize) * u32b;
        let meta_offs_bytes = (self.counts.macro_edges as usize) * u32b;
        let meta_lens_bytes = (self.counts.macro_edges as usize) * u32b;
        let lm_bytes = (self.counts.landmarks as usize) * u32b;
        // ALT tables sized by nodes * landmarks (f32 each)
        let alt_table_bytes = (self.counts.nodes as usize)
            .saturating_mul(self.counts.landmarks as usize)
            .saturating_mul(f32b);
        if !fits(self.off_nodes_ids, nodes_bytes_u32, file_len) { return Err(ManifestError::OutOfBounds("nodes_ids")); }
        if !fits(self.off_nodes_x, nodes_bytes_i32, file_len) { return Err(ManifestError::OutOfBounds("nodes_x")); }
        if !fits(self.off_nodes_y, nodes_bytes_i32, file_len) { return Err(ManifestError::OutOfBounds("nodes_y")); }
        if !fits(self.off_nodes_plane, nodes_bytes_i32, file_len) { return Err(ManifestError::OutOfBounds("nodes_plane")); }
        if !fits(self.off_walk_src, walk_src_bytes, file_len) { return Err(ManifestError::OutOfBounds("walk_src")); }
        if !fits(self.off_walk_dst, walk_dst_bytes, file_len) { return Err(ManifestError::OutOfBounds("walk_dst")); }
        if !fits(self.off_walk_w, walk_w_bytes, file_len) { return Err(ManifestError::OutOfBounds("walk_w")); }
        if !fits(self.off_macro_src, macro_src_bytes, file_len) { return Err(ManifestError::OutOfBounds("macro_src")); }
        if !fits(self.off_macro_dst, macro_dst_bytes, file_len) { return Err(ManifestError::OutOfBounds("macro_dst")); }
        if !fits(self.off_macro_w, macro_w_bytes, file_len) { return Err(ManifestError::OutOfBounds("macro_w")); }
        if !fits(self.off_macro_kind_first, macro_kind_first_bytes, file_len) { return Err(ManifestError::OutOfBounds("macro_kind_first")); }
        if !fits(self.off_macro_id_first, macro_id_first_bytes, file_len) { return Err(ManifestError::OutOfBounds("macro_id_first")); }
        if !fits(self.off_macro_meta_offs, meta_offs_bytes, file_len) { return Err(ManifestError::OutOfBounds("macro_meta_offs")); }
        if !fits(self.off_macro_meta_lens, meta_lens_bytes, file_len) { return Err(ManifestError::OutOfBounds("macro_meta_lens")); }
        // blob can be variable length; ensure start is within file
        if (self.off_macro_meta_blob as usize) > file_len { return Err(ManifestError::OutOfBounds("macro_meta_blob")); }
        if !fits(self.off_req_tags, req_bytes, file_len) { return Err(ManifestError::OutOfBounds("req_tags")); }
        if !fits(self.off_landmarks, lm_bytes, file_len) { return Err(ManifestError::OutOfBounds("landmarks")); }
        // ALT tables may be zero-sized if landmarks == 0
        if self.counts.landmarks > 0 {
            if !fits(self.off_lm_fw, alt_table_bytes, file_len) { return Err(ManifestError::OutOfBounds("lm_fw")); }
            if !fits(self.off_lm_bw, alt_table_bytes, file_len) { return Err(ManifestError::OutOfBounds("lm_bw")); }
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
}
