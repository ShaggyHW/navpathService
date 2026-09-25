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
    /// Every [`KEY_SAMPLE_STRIDE`]-th packed coordinate key (heap copy, ~18 KB at 1.17M
    /// nodes): `find_node` binary-searches this L1/L2-resident sample first and then one
    /// ~1 KB window of the mapped coords section, instead of ~21 dependent probes
    /// scattered over 4.7 MB of mmap (up to ~10 page faults after eviction).
    key_sample: Vec<u32>,
}

/// Stride of [`Snapshot::key_sample`].
const KEY_SAMPLE_STRIDE: usize = 256;

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

        // Per-section paging policy. Only the ALT table (>=84% of the file, random 256 B
        // row gathers) is advised Random — readahead there is pure waste — plus a
        // huge-page request (ext4 serves PMD-mapped 2 MiB page-cache folios under it, so
        // a cold row fault reads one folio instead of one 4 KiB page). Everything else
        // (coords, walk CSR, comp, macro/fairy/meta, req tags — hot on every request)
        // keeps NORMAL readahead: advising the whole map Random and then WillNeed on the
        // head (the pre-2026-09 policy) left VM_RAND_READ set on the head, so every
        // post-eviction CSR fault was a synchronous 4 KiB read with no read-around.
        let (lm_off, lm_len) = manifest.lm_tab_range();
        if lm_len > 0 && lm_off.checked_add(lm_len).is_some_and(|end| end <= mmap.len()) {
            let _ = mmap.advise_range(memmap2::Advice::Random, lm_off, lm_len);
            #[cfg(target_os = "linux")]
            {
                let _ = mmap.advise_range(memmap2::Advice::HugePage, lm_off, lm_len);
                if lm_off > 0 {
                    let _ = mmap.advise_range(memmap2::Advice::HugePage, 0, lm_off);
                }
            }
            if lm_off > 0 {
                let _ = mmap.advise_range(memmap2::Advice::WillNeed, 0, lm_off);
            }
            let tail = lm_off + lm_len;
            if tail < mmap.len() {
                let _ = mmap.advise_range(memmap2::Advice::WillNeed, tail, mmap.len() - tail);
            }
        } else {
            let _ = mmap.advise(memmap2::Advice::WillNeed);
        }
        if std::env::var("NAVPATH_ALT_HEAP").is_ok() {
            // Retired 2026-09: the file-backed ALT range is already huge-page mapped
            // (FilePmdMapped observed), so the anon copy only doubled the table's memory.
            eprintln!("navpath: NAVPATH_ALT_HEAP is retired and ignored");
        }

        let mut snap = Snapshot { mmap, manifest, key_sample: Vec::new() };
        snap.validate_graph()?;
        snap.key_sample = snap.coords_packed().iter().step_by(KEY_SAMPLE_STRIDE).copied().collect();
        Ok(snap)
    }

    /// One O(nodes + edges) pass over the walk CSR at load: offsets monotone and in
    /// range, every destination a valid node, degree <= 8 (an 8-connected grid), and
    /// coordinate keys strictly ascending (the find_node / canonical-grid invariant). A
    /// malformed or truncated snapshot then fails the load with an error instead of
    /// panicking (or silently mis-routing) inside a request. Reading these ~40 MB also
    /// warms exactly the sections every search touches.
    fn validate_graph(&self) -> Result<(), SnapshotError> {
        let n = self.manifest.counts.nodes as usize;
        let e = self.manifest.counts.walk_edges as usize;
        let offs = self.walk_offsets();
        if offs.first() != Some(&0) || offs.last().map(|&x| x as usize) != Some(e) {
            return Err(ManifestError::Invalid("walk_offsets must start at 0 and end at walk_edges").into());
        }
        for w in offs.windows(2) {
            if w[1] < w[0] || w[1] - w[0] > 8 {
                return Err(ManifestError::Invalid("walk_offsets not monotone or degree > 8").into());
            }
        }
        if self.walk_dst().iter().any(|&d| d as usize >= n) {
            return Err(ManifestError::Invalid("walk_dst references a node out of range").into());
        }
        if self.coords_packed().windows(2).any(|w| w[0] >= w[1]) {
            return Err(ManifestError::Invalid("coordinate keys not strictly ascending").into());
        }
        Ok(())
    }

    /// Byte range of the ALT table section inside the mapping.
    fn lm_range(&self) -> (usize, usize) {
        self.manifest.lm_tab_range()
    }

    /// Populate (prefault readable) `[off, off+len)` of the mapping with
    /// MADV_POPULATE_READ over a few parallel chunks — several I/Os in flight on a cold
    /// page cache instead of the one a page-by-page touch loop keeps, and no SIGBUS on
    /// a truncated file (an error is returned instead; the fallback touch loop is only
    /// used where the advice is unsupported).
    fn populate_range(&self, off: usize, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        #[cfg(target_os = "linux")]
        {
            const CHUNK_MIN: usize = 8 << 20;
            let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 8);
            let chunks = (len / CHUNK_MIN).clamp(1, threads);
            // Chunk boundaries on 2 MiB so huge folios are not split between threads.
            let step = (len.div_ceil(chunks) + (2 << 20) - 1) & !((2 << 20) - 1);
            let ok = std::sync::atomic::AtomicBool::new(true);
            std::thread::scope(|scope| {
                let mut start = off;
                while start < off + len {
                    let end = (start + step).min(off + len);
                    let (mmap, ok) = (&self.mmap, &ok);
                    scope.spawn(move || {
                        if mmap.advise_range(memmap2::Advice::PopulateRead, start, end - start).is_err() {
                            ok.store(false, std::sync::atomic::Ordering::Relaxed);
                        }
                    });
                    start = end;
                }
            });
            if ok.load(std::sync::atomic::Ordering::Relaxed) {
                return len;
            }
        }
        const PAGE: usize = 4096;
        let bytes = &self.mmap[off..off + len];
        let mut acc = 0u64;
        let mut i = 0;
        while i < bytes.len() {
            // SAFETY: `i < len`; volatile so the loads are not elided.
            acc = acc.wrapping_add(unsafe { std::ptr::read_volatile(bytes.as_ptr().add(i)) } as u64);
            i += PAGE;
        }
        std::hint::black_box(acc);
        len
    }

    /// Make the whole snapshot resident before the first search. The ALT range is
    /// advised Random, so without this every untouched row page is a synchronous read
    /// inside a request: measured 50-90 µs per pop on cold map regions vs 150-230 ns
    /// warm (docs/route_latency_improvements_2026-09-17.md §1.4). Returns the number of
    /// bytes populated.
    pub fn populate(&self) -> usize {
        self.populate_range(0, self.mmap.len())
    }

    /// Populate everything EXCEPT the ALT table: coords, walk CSR, component ids and
    /// metadata — the ~45 MB every search touches on every pop. Cheap enough to repeat
    /// periodically (a keep-warm loop) when the mapping cannot be locked: page-table
    /// hits cost microseconds, and evicted pages come back before a request needs them.
    pub fn populate_head(&self) -> usize {
        let (lm_off, lm_len) = self.lm_range();
        let mut n = self.populate_range(0, lm_off.min(self.mmap.len()));
        let tail = lm_off + lm_len;
        if tail < self.mmap.len() {
            n += self.populate_range(tail, self.mmap.len() - tail);
        }
        n
    }

    /// Populate the ALT table only (see [`Snapshot::populate_head`]).
    pub fn populate_alt(&self) -> usize {
        let (lm_off, lm_len) = self.lm_range();
        self.populate_range(lm_off, lm_len.min(self.mmap.len().saturating_sub(lm_off)))
    }

    /// `mlock` the whole mapping so the page cache cannot evict it under memory
    /// pressure. Fails (harmlessly) when `RLIMIT_MEMLOCK` is below the snapshot size.
    pub fn lock_memory(&self) -> std::io::Result<()> {
        self.mmap.lock()
    }

    /// `mlock` only the non-ALT sections (~45 MB): the fallback when the memlock limit
    /// admits the per-pop hot data but not the whole table.
    pub fn lock_head(&self) -> std::io::Result<()> {
        let (lm_off, lm_len) = self.lm_range();
        self.lock_range(0, lm_off)?;
        let tail = lm_off + lm_len;
        if tail < self.mmap.len() {
            self.lock_range(tail, self.mmap.len() - tail)?;
        }
        Ok(())
    }

    fn lock_range(&self, off: usize, len: usize) -> std::io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        #[cfg(unix)]
        {
            // SAFETY: the range lies inside the live mapping owned by `self`.
            let rc = unsafe { libc::mlock(self.mmap.as_ptr().add(off) as *const libc::c_void, len) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Cold-cache study hook: ask the kernel to reclaim this mapping's pages
    /// (MADV_PAGEOUT; clean file pages leave the page cache unless another process maps
    /// them), for the whole file (`alt_only = false`) or just the ALT table. The next
    /// access re-reads from disk exactly as after memory-pressure eviction. Linux only;
    /// an error elsewhere.
    pub fn evict(&self, alt_only: bool) -> std::io::Result<()> {
        let (off, len) = if alt_only { self.lm_range() } else { (0, self.mmap.len()) };
        #[cfg(target_os = "linux")]
        {
            // SAFETY: the range lies inside the live mapping; MADV_PAGEOUT only drops
            // clean pages, which are re-read transparently on the next access.
            let rc = unsafe { libc::madvise(self.mmap.as_ptr().add(off) as *mut libc::c_void, len, libc::MADV_PAGEOUT) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (off, len);
            Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "MADV_PAGEOUT is Linux-only"))
        }
    }

    /// The builder's blake3 content hash (the file's trailing 32 bytes), as hex. Read
    /// from the MAPPED file, so it always describes the snapshot actually being served
    /// (re-opening the path could observe a newer file renamed into place).
    pub fn tail_hash_hex(&self) -> Option<String> {
        let len = self.mmap.len();
        if len < Manifest::SIZE + 32 {
            return None;
        }
        let mut out = String::with_capacity(64);
        for b in &self.mmap[len - 32..] {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
        }
        Some(out)
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
    /// The clustered u8 ALT table ([`super::alt_pack`]) when the snapshot stores one
    /// (None: plain table, see [`Snapshot::lm_tab`]).
    #[inline]
    pub fn lm_packed(&self) -> Option<&[u8]> {
        if self.manifest.alt_format != super::manifest::ALT_FORMAT_PACKED {
            return None;
        }
        let (off, len) = self.lm_range();
        Some(&self.mmap[off..off + len])
    }

    /// Interleaved quantized ALT table: `[node][landmark][fw, bw]` u16 quanta. Empty
    /// when the snapshot stores the packed encoding (see [`Snapshot::lm_packed`]).
    #[inline]
    pub fn lm_tab(&self) -> &[u16] {
        if self.manifest.alt_format == super::manifest::ALT_FORMAT_PACKED {
            return &[];
        }
        let n = (self.manifest.counts.nodes as usize)
            .saturating_mul(self.manifest.counts.landmarks as usize)
            .saturating_mul(2);
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
    /// this is a binary search: first over the heap-resident key sample, then over one
    /// [`KEY_SAMPLE_STRIDE`]-key window of the mmap'd coords section.
    pub fn find_node(&self, x: i32, y: i32, plane: i32) -> Option<u32> {
        if !(0..32768).contains(&x) || !(0..32768).contains(&y) || !(0..4).contains(&plane) {
            return None;
        }
        let key = pack_coord(x, y, plane);
        let coords = self.coords_packed();
        // Window start: the last sample <= key (keys are strictly ascending).
        let w = match self.key_sample.binary_search(&key) {
            Ok(i) => return Some((i * KEY_SAMPLE_STRIDE) as u32),
            Err(0) => return None,
            Err(i) => i - 1,
        };
        let lo = w * KEY_SAMPLE_STRIDE;
        let hi = (lo + KEY_SAMPLE_STRIDE).min(coords.len());
        coords[lo..hi].binary_search(&key).ok().map(|i| (lo + i) as u32)
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
    use crate::snapshot::manifest::{pack_coord, unpack_coord, ALT_UNREACHABLE};
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
        let hex: String = res.hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(snap.tail_hash_hex().as_deref(), Some(hex.as_str()));
        // The ALT section and the one after it start on 2 MiB boundaries.
        assert_eq!(snap.manifest().off_lm_tab % crate::snapshot::manifest::ALT_SECTION_ALIGN, 0);
        assert_eq!(snap.manifest().off_fairy_nodes % crate::snapshot::manifest::ALT_SECTION_ALIGN, 0);
        assert!(snap.lm_packed().is_none());
        // Populate / head-populate cover the mapping without error.
        assert_eq!(snap.populate(), bytes.len());
    }

    #[test]
    fn packed_alt_roundtrip() {
        use crate::snapshot::writer::{write_snapshot, AltFormat, WriteOptions};
        // 40 nodes on a line, 16 landmarks (stride 32) with one out-of-range value to
        // force an exception entry, plus sentinels.
        let n = 40usize;
        let l = 16usize;
        let coords: Vec<u32> = (0..n as i32).map(|i| pack_coord(200 + i, 70, 0)).collect();
        let mut walk_offsets = vec![0u32];
        let mut walk_dst = Vec::new();
        for i in 0..n as u32 {
            if i > 0 { walk_dst.push(i - 1); }
            if i + 1 < n as u32 { walk_dst.push(i + 1); }
            walk_offsets.push(walk_dst.len() as u32);
        }
        let walk_diag = vec![0u8; walk_dst.len().div_ceil(8)];
        let comp = vec![0u16; n];
        let lm_ids: Vec<u32> = (0..l as u32).collect();
        let mut lm_tab = vec![0u16; n * 2 * l];
        for u in 0..n {
            for lane in 0..2 * l {
                lm_tab[u * 2 * l + lane] = match (u + lane) % 11 {
                    0 => ALT_UNREACHABLE,
                    1 => crate::snapshot::ALT_SATURATED,
                    _ => (100 + u + lane) as u16,
                };
            }
        }
        lm_tab[5 * 2 * l + 3] = 9000; // spreads its cluster lane past the offset range
        let meta_blob = b"{}".to_vec();
        let s = SnapshotSections {
            coords_packed: &coords, walk_offsets: &walk_offsets, walk_dst: &walk_dst, walk_diag: &walk_diag,
            comp: &comp, walk_components: 1, macro_src: &[], macro_dst: &[], macro_w: &[],
            macro_kind_first: &[], macro_id_first: &[], macro_meta_offs: &[], macro_meta_lens: &[],
            macro_meta_blob: &meta_blob, req_tags: &[], landmarks: &lm_ids, lm_tab: &lm_tab,
            fairy_nodes: &[], fairy_cost_ms: &[], fairy_meta_offs: &[], fairy_meta_lens: &[], fairy_meta_blob: &[],
        };
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        write_snapshot(&path, &s, &WriteOptions { alt_format: AltFormat::Packed }).expect("write packed");
        let snap = Snapshot::open(&path).expect("open packed");
        assert!(snap.lm_tab().is_empty());
        assert!(snap.manifest().alt_exceptions >= 1);
        let section = snap.lm_packed().expect("packed section");
        let p = crate::snapshot::alt_pack::PackedAlt::from_section(section, n, l);
        let mut row = vec![0u16; 2 * l];
        for u in 0..n {
            p.decode_row_exact(u, &mut row);
            assert_eq!(&row[..], &lm_tab[u * 2 * l..(u + 1) * 2 * l], "node {u}");
        }
        for (u, &k) in coords.iter().enumerate() {
            let (x, y, pl) = unpack_coord(k);
            assert_eq!(snap.find_node(x, y, pl), Some(u as u32));
        }
    }
}
