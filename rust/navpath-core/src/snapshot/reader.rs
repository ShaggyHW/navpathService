use std::{fs::File, path::Path};

use memmap2::Mmap;

use super::manifest::{pack_coord, unpack_coord, Manifest, ManifestError};

// The v8 reader reinterprets aligned mmap sections as typed slices; that requires a
// little-endian host (all deployment targets are x86-64/aarch64 LE).
#[cfg(not(target_endian = "little"))]
compile_error!("navpath snapshot reader requires a little-endian target");

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

pub struct Snapshot {
    mmap: Mmap,
    manifest: Manifest,
    /// `NAVPATH_ALT_HEAP=1`: an anonymous, huge-page-advised copy of the ALT table.
    /// File-backed mappings never get transparent huge pages automatically, so the
    /// 256 B random row gathers pay an L2-dTLB miss on most touches; an anon copy gets
    /// fault-time THP under `enabled=[always]`, shrinking the table to a few hundred
    /// 2 MiB pages (fully TLB-resident on Zen 4) for +table-size RSS and ~0.1 s of
    /// one-time copy.
    alt_heap: Option<Mmap>,
}

impl Snapshot {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SnapshotError> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < Manifest::SIZE {
            return Err(ManifestError::HeaderTooSmall.into());
        }
        let header = &mmap[0..Manifest::SIZE];
        let manifest = Manifest::parse(header)?;
        manifest.validate_layout(mmap.len())?;

        // Per-section paging policy instead of one blanket Advice::Random:
        //  - the ALT table (>=84% of the file) is random 256 B row gathers — readahead
        //    is pure waste, so it keeps Random, plus a huge-page request (harmless
        //    no-op where unsupported);
        //  - everything else (coords, walk CSR, macro/fairy/meta, req tags — hot on
        //    every request) gets WillNeed so post-load/reload traffic takes async
        //    readahead instead of scattered major faults.
        // NAVPATH_MMAP_POPULATE=1 still pre-faults the whole file.
        let lm_off = manifest.off_lm_tab as usize;
        let lm_len = (manifest.counts.nodes as usize)
            .saturating_mul(manifest.counts.landmarks as usize)
            .saturating_mul(4);
        let _ = mmap.advise(memmap2::Advice::Random);
        if lm_len > 0 && lm_off.checked_add(lm_len).is_some_and(|end| end <= mmap.len()) {
            if lm_off > 0 {
                let _ = mmap.advise_range(memmap2::Advice::WillNeed, 0, lm_off);
            }
            let tail = lm_off + lm_len;
            if tail < mmap.len() {
                let _ = mmap.advise_range(memmap2::Advice::WillNeed, tail, mmap.len() - tail);
            }
            #[cfg(target_os = "linux")]
            {
                let _ = mmap.advise_range(memmap2::Advice::HugePage, lm_off, lm_len);
            }
        } else {
            let _ = mmap.advise(memmap2::Advice::WillNeed);
        }
        if std::env::var("NAVPATH_MMAP_POPULATE").ok().as_deref() == Some("1") {
            let _ = mmap.advise(memmap2::Advice::WillNeed);
        }

        let alt_heap = if std::env::var("NAVPATH_ALT_HEAP").ok().as_deref() == Some("1")
            && lm_len > 0
            && lm_off + lm_len <= mmap.len()
        {
            let mut anon = memmap2::MmapMut::map_anon(lm_len)?;
            #[cfg(target_os = "linux")]
            {
                let _ = anon.advise(memmap2::Advice::HugePage);
            }
            anon.copy_from_slice(&mmap[lm_off..lm_off + lm_len]);
            Some(anon.make_read_only()?)
        } else {
            None
        };

        Ok(Snapshot { mmap, manifest, alt_heap })
    }

    pub fn manifest(&self) -> &Manifest { &self.manifest }
    pub fn counts(&self) -> super::manifest::SnapshotCounts { self.manifest.counts }

    /// # Safety rationale
    /// `validate_layout` guarantees the section fits the file and starts on a 64-byte
    /// boundary, and mmap bases are page-aligned, so the cast slice is in-bounds and
    /// properly aligned for T.
    #[inline]
    fn section<T>(&self, off: u64, count: usize) -> &[T] {
        let start = off as usize;
        debug_assert!(start + count * core::mem::size_of::<T>() <= self.mmap.len());
        debug_assert_eq!(start % core::mem::align_of::<T>(), 0);
        unsafe { std::slice::from_raw_parts(self.mmap.as_ptr().add(start) as *const T, count) }
    }

    #[inline]
    pub fn coords_packed(&self) -> &[u32] {
        self.section(self.manifest.off_coords, self.manifest.counts.nodes as usize)
    }
    #[inline]
    pub fn walk_offsets(&self) -> &[u32] {
        self.section(self.manifest.off_walk_offsets, self.manifest.counts.nodes as usize + 1)
    }
    #[inline]
    pub fn walk_dst(&self) -> &[u32] {
        self.section(self.manifest.off_walk_dst, self.manifest.counts.walk_edges as usize)
    }
    #[inline]
    pub fn walk_diag(&self) -> &[u8] {
        self.section(self.manifest.off_walk_diag, (self.manifest.counts.walk_edges as usize).div_ceil(8))
    }
    #[inline]
    pub fn comp_ids(&self) -> &[u16] {
        self.section(self.manifest.off_comp, self.manifest.counts.nodes as usize)
    }
    pub fn macro_src(&self) -> &[u32] {
        self.section(self.manifest.off_macro_src, self.manifest.counts.macro_edges as usize)
    }
    pub fn macro_dst(&self) -> &[u32] {
        self.section(self.manifest.off_macro_dst, self.manifest.counts.macro_edges as usize)
    }
    pub fn macro_w(&self) -> &[f32] {
        self.section(self.manifest.off_macro_w, self.manifest.counts.macro_edges as usize)
    }
    pub fn macro_kind_first(&self) -> &[u32] {
        self.section(self.manifest.off_macro_kind_first, self.manifest.counts.macro_edges as usize)
    }
    pub fn macro_id_first(&self) -> &[u32] {
        self.section(self.manifest.off_macro_id_first, self.manifest.counts.macro_edges as usize)
    }
    pub fn macro_meta_offs(&self) -> &[u32] {
        self.section(self.manifest.off_macro_meta_offs, self.manifest.counts.macro_edges as usize)
    }
    pub fn macro_meta_lens(&self) -> &[u32] {
        self.section(self.manifest.off_macro_meta_lens, self.manifest.counts.macro_edges as usize)
    }
    pub fn macro_meta_blob(&self) -> &[u8] {
        let start = self.manifest.off_macro_meta_blob as usize;
        let end = self.manifest.off_req_tags as usize;
        &self.mmap[start..end]
    }
    pub fn macro_meta_at(&self, idx: usize) -> Option<&[u8]> {
        let offs = self.macro_meta_offs();
        let lens = self.macro_meta_lens();
        if idx >= offs.len() { return None; }
        let o = offs[idx] as usize;
        let l = lens[idx] as usize;
        let blob = self.macro_meta_blob();
        if o + l <= blob.len() { Some(&blob[o..o + l]) } else { None }
    }
    pub fn req_tags(&self) -> &[u32] {
        self.section(self.manifest.off_req_tags, self.manifest.counts.req_tags as usize)
    }
    pub fn landmarks(&self) -> &[u32] {
        self.section(self.manifest.off_landmarks, self.manifest.counts.landmarks as usize)
    }
    /// Interleaved quantized ALT table: `[node][landmark][fw, bw]` u16 quanta. Served
    /// from the huge-page anon copy when `NAVPATH_ALT_HEAP=1` (see [`Snapshot`]).
    #[inline]
    pub fn lm_tab(&self) -> &[u16] {
        let n = (self.manifest.counts.nodes as usize)
            .saturating_mul(self.manifest.counts.landmarks as usize)
            .saturating_mul(2);
        if let Some(heap) = &self.alt_heap {
            // Anonymous mappings are page-aligned, comfortably satisfying u16.
            return unsafe { std::slice::from_raw_parts(heap.as_ptr() as *const u16, n) };
        }
        self.section(self.manifest.off_lm_tab, n)
    }

    // Fairy Ring accessors
    pub fn fairy_nodes(&self) -> &[u32] {
        self.section(self.manifest.off_fairy_nodes, self.manifest.counts.fairy_rings as usize)
    }
    pub fn fairy_cost_ms(&self) -> &[f32] {
        self.section(self.manifest.off_fairy_cost_ms, self.manifest.counts.fairy_rings as usize)
    }
    pub fn fairy_meta_offs(&self) -> &[u32] {
        self.section(self.manifest.off_fairy_meta_offs, self.manifest.counts.fairy_rings as usize)
    }
    pub fn fairy_meta_lens(&self) -> &[u32] {
        self.section(self.manifest.off_fairy_meta_lens, self.manifest.counts.fairy_rings as usize)
    }
    pub fn fairy_meta_blob(&self) -> &[u8] {
        let start = self.manifest.off_fairy_meta_blob as usize;
        let end = self.mmap.len().saturating_sub(32); // hash at tail
        if start <= end { &self.mmap[start..end] } else { &[] }
    }
    pub fn fairy_meta_at(&self, idx: usize) -> Option<&[u8]> {
        let offs = self.fairy_meta_offs();
        let lens = self.fairy_meta_lens();
        if idx >= offs.len() { return None; }
        let o = offs[idx] as usize;
        let l = lens[idx] as usize;
        let blob = self.fairy_meta_blob();
        if o + l <= blob.len() { Some(&blob[o..o + l]) } else { None }
    }

    /// Node coordinates, unpacked from the packed section.
    #[inline]
    pub fn node_coord(&self, id: u32) -> (i32, i32, i32) {
        let cs = self.coords_packed();
        match cs.get(id as usize) {
            Some(&k) => unpack_coord(k),
            None => (0, 0, 0),
        }
    }

    /// Coordinate -> node id. Node ids are assigned in ascending packed-key order, so
    /// this is a binary search over the mmap'd coords section — no heap index needed.
    pub fn find_node(&self, x: i32, y: i32, plane: i32) -> Option<u32> {
        if !(0..32768).contains(&x) || !(0..32768).contains(&y) || !(0..4).contains(&plane) {
            return None;
        }
        let key = pack_coord(x, y, plane);
        self.coords_packed().binary_search(&key).ok().map(|i| i as u32)
    }

    /// Weight of the walk edge u->v if it exists (scans u's neighbor slice; degree <= 8).
    pub fn walk_edge_weight(&self, u: u32, v: u32) -> Option<f32> {
        let offs = self.walk_offsets();
        let u = u as usize;
        if u + 1 >= offs.len() { return None; }
        let (s, e) = (offs[u] as usize, offs[u + 1] as usize);
        let dst = self.walk_dst();
        let diag = self.walk_diag();
        for slot in s..e {
            if dst[slot] == v {
                let is_diag = diag[slot / 8] & (1 << (slot % 8)) != 0;
                return Some(if is_diag {
                    super::manifest::walk_diagonal_ms()
                } else {
                    super::manifest::WALK_CARDINAL_MS
                });
            }
        }
        None
    }
}

#[cfg(all(test, feature = "builder"))]
mod tests {
    use super::*;
    use crate::snapshot::manifest::{pack_coord, ALT_UNREACHABLE};
    use crate::snapshot::writer::{write_snapshot_v8, SnapshotSections};
    use tempfile::NamedTempFile;

    #[test]
    fn v8_roundtrip() {
        // 3 nodes in a line on plane 0: (100,50) (101,50) (102,50); edges 0<->1<->2
        // cardinal; one diagonal edge 0->2 for bitmap coverage (synthetic).
        let coords = [
            pack_coord(100, 50, 0),
            pack_coord(101, 50, 0),
            pack_coord(102, 50, 0),
        ];
        let walk_offsets = [0u32, 2, 4, 6];
        let walk_dst = [1u32, 2, 0, 2, 1, 0];
        // slot 1 (0->2) and slot 5 (2->0) are diagonal
        let walk_diag = [0b0010_0010u8];
        let comp = [0u16, 0, 0];
        let lm_ids = [0u32, 2];
        // lm_tab: [node][lm][fw,bw]; node1 unreachable from lm1 for sentinel coverage
        let lm_tab: [u16; 12] = [
            0, 0, 5, 5,
            2, 2, ALT_UNREACHABLE, 3,
            5, 5, 0, 0,
        ];
        let meta_blob = b"{}".to_vec();
        let fairy_blob = br#"{"code":"ALS"}"#.to_vec();
        let s = SnapshotSections {
            coords_packed: &coords,
            walk_offsets: &walk_offsets,
            walk_dst: &walk_dst,
            walk_diag: &walk_diag,
            comp: &comp,
            walk_components: 1,
            macro_src: &[0],
            macro_dst: &[2],
            macro_w: &[3.5],
            macro_kind_first: &[2],
            macro_id_first: &[42],
            macro_meta_offs: &[0],
            macro_meta_lens: &[2],
            macro_meta_blob: &meta_blob,
            req_tags: &[7, 8, 9, 10],
            landmarks: &lm_ids,
            lm_tab: &lm_tab,
            fairy_nodes: &[0],
            fairy_cost_ms: &[600.0],
            fairy_meta_offs: &[0],
            fairy_meta_lens: &[fairy_blob.len() as u32],
            fairy_meta_blob: &fairy_blob,
        };
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let res = write_snapshot_v8(&path, &s).expect("write v8");

        let snap = Snapshot::open(&path).expect("open v8");
        let c = snap.counts();
        assert_eq!(c.nodes, 3);
        assert_eq!(c.walk_edges, 6);
        assert_eq!(c.macro_edges, 1);
        assert_eq!(c.landmarks, 2);
        assert_eq!(c.walk_components, 1);

        assert_eq!(snap.coords_packed(), &coords);
        assert_eq!(snap.node_coord(1), (101, 50, 0));
        assert_eq!(snap.find_node(102, 50, 0), Some(2));
        assert_eq!(snap.find_node(103, 50, 0), None);

        assert_eq!(snap.walk_offsets(), &walk_offsets);
        assert_eq!(snap.walk_dst(), &walk_dst);
        // slot 0 (0->1) cardinal, slot 1 (0->2) diagonal
        assert_eq!(snap.walk_edge_weight(0, 1), Some(300.0));
        let d = snap.walk_edge_weight(0, 2).unwrap();
        assert!((d - 424.26407).abs() < 1e-3);
        assert_eq!(snap.walk_edge_weight(1, 1), None);

        assert_eq!(snap.comp_ids(), &comp);
        assert_eq!(snap.macro_src(), &[0]);
        assert_eq!(snap.macro_w(), &[3.5]);
        assert_eq!(snap.macro_meta_at(0).unwrap(), b"{}");
        assert_eq!(snap.req_tags(), &[7, 8, 9, 10]);
        assert_eq!(snap.landmarks(), &lm_ids);
        assert_eq!(snap.lm_tab(), &lm_tab);
        assert_eq!(snap.fairy_meta_at(0).unwrap(), &fairy_blob[..]);

        // hash tail matches
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[bytes.len() - 32..], &res.hash);
        let mut hasher = blake3::Hasher::new();
        hasher.update(&bytes[..bytes.len() - 32]);
        assert_eq!(hasher.finalize().as_bytes(), &res.hash);
    }
}
