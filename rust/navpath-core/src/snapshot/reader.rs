use std::{fs::File, path::Path};

use byteorder::{ByteOrder, LittleEndian};
use memmap2::Mmap;

use super::manifest::{Manifest, ManifestError};

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
        Ok(Snapshot { mmap, manifest })
    }

    pub fn manifest(&self) -> &Manifest { &self.manifest }
    pub fn counts(&self) -> super::manifest::SnapshotCounts { self.manifest.counts }

    pub fn nodes_ids(&self) -> LeSliceU32<'_> {
        self.view_u32(self.manifest.off_nodes_ids, self.manifest.counts.nodes as usize)
    }
    pub fn nodes_x(&self) -> LeSliceI32<'_> {
        self.view_i32(self.manifest.off_nodes_x, self.manifest.counts.nodes as usize)
    }
    pub fn nodes_y(&self) -> LeSliceI32<'_> {
        self.view_i32(self.manifest.off_nodes_y, self.manifest.counts.nodes as usize)
    }
    pub fn nodes_plane(&self) -> LeSliceI32<'_> {
        self.view_i32(self.manifest.off_nodes_plane, self.manifest.counts.nodes as usize)
    }
    pub fn walk_src(&self) -> LeSliceU32<'_> {
        self.view_u32(self.manifest.off_walk_src, self.manifest.counts.walk_edges as usize)
    }
    pub fn walk_dst(&self) -> LeSliceU32<'_> {
        self.view_u32(self.manifest.off_walk_dst, self.manifest.counts.walk_edges as usize)
    }
    pub fn walk_w(&self) -> LeSliceF32<'_> {
        self.view_f32(self.manifest.off_walk_w, self.manifest.counts.walk_edges as usize)
    }
    pub fn macro_src(&self) -> LeSliceU32<'_> {
        self.view_u32(self.manifest.off_macro_src, self.manifest.counts.macro_edges as usize)
    }
    pub fn macro_dst(&self) -> LeSliceU32<'_> {
        self.view_u32(self.manifest.off_macro_dst, self.manifest.counts.macro_edges as usize)
    }
    pub fn macro_w(&self) -> LeSliceF32<'_> {
        self.view_f32(self.manifest.off_macro_w, self.manifest.counts.macro_edges as usize)
    }
    pub fn macro_kind_first(&self) -> LeSliceU32<'_> {
        self.view_u32(self.manifest.off_macro_kind_first, self.manifest.counts.macro_edges as usize)
    }
    pub fn macro_id_first(&self) -> LeSliceU32<'_> {
        self.view_u32(self.manifest.off_macro_id_first, self.manifest.counts.macro_edges as usize)
    }
    pub fn macro_meta_offs(&self) -> LeSliceU32<'_> {
        self.view_u32(self.manifest.off_macro_meta_offs, self.manifest.counts.macro_edges as usize)
    }
    pub fn macro_meta_lens(&self) -> LeSliceU32<'_> {
        self.view_u32(self.manifest.off_macro_meta_lens, self.manifest.counts.macro_edges as usize)
    }
    pub fn macro_meta_blob(&self) -> &[u8] {
        let start = self.manifest.off_macro_meta_blob as usize;
        let end = self.manifest.off_req_tags as usize;
        &self.mmap[start..end]
    }
    pub fn macro_meta_at(&self, idx: usize) -> Option<&[u8]> {
        let offs = self.macro_meta_offs();
        let lens = self.macro_meta_lens();
        if idx >= offs.len() || idx >= lens.len() { return None; }
        let o = offs.get(idx)? as usize;
        let l = lens.get(idx)? as usize;
        let blob = self.macro_meta_blob();
        if o + l <= blob.len() { Some(&blob[o..o+l]) } else { None }
    }
    pub fn req_tags(&self) -> LeSliceU32<'_> {
        self.view_u32(self.manifest.off_req_tags, self.manifest.counts.req_tags as usize)
    }
    pub fn landmarks(&self) -> LeSliceU32<'_> {
        self.view_u32(self.manifest.off_landmarks, self.manifest.counts.landmarks as usize)
    }
    pub fn lm_fw(&self) -> LeSliceF32<'_> {
        let n = (self.manifest.counts.nodes as usize)
            .saturating_mul(self.manifest.counts.landmarks as usize);
        self.view_f32(self.manifest.off_lm_fw, n)
    }
    pub fn lm_bw(&self) -> LeSliceF32<'_> {
        let n = (self.manifest.counts.nodes as usize)
            .saturating_mul(self.manifest.counts.landmarks as usize);
        self.view_f32(self.manifest.off_lm_bw, n)
    }

    // Fairy Ring accessors
    pub fn fairy_nodes(&self) -> LeSliceU32<'_> {
        self.view_u32(self.manifest.off_fairy_nodes, self.manifest.counts.fairy_rings as usize)
    }
    pub fn fairy_cost_ms(&self) -> LeSliceF32<'_> {
        self.view_f32(self.manifest.off_fairy_cost_ms, self.manifest.counts.fairy_rings as usize)
    }
    pub fn fairy_meta_offs(&self) -> LeSliceU32<'_> {
        self.view_u32(self.manifest.off_fairy_meta_offs, self.manifest.counts.fairy_rings as usize)
    }
    pub fn fairy_meta_lens(&self) -> LeSliceU32<'_> {
        self.view_u32(self.manifest.off_fairy_meta_lens, self.manifest.counts.fairy_rings as usize)
    }
    pub fn fairy_meta_blob(&self) -> &[u8] {
        let start = self.manifest.off_fairy_meta_blob as usize;
        // The blob extends to the end of the file (minus the 32-byte hash) or to the next section
        // For now we'll compute based on the last entry's offset + len
        let end = self.mmap.len().saturating_sub(32); // hash at tail
        if start <= end { &self.mmap[start..end] } else { &[] }
    }
    pub fn fairy_meta_at(&self, idx: usize) -> Option<&[u8]> {
        let offs = self.fairy_meta_offs();
        let lens = self.fairy_meta_lens();
        if idx >= offs.len() || idx >= lens.len() { return None; }
        let o = offs.get(idx)? as usize;
        let l = lens.get(idx)? as usize;
        let blob = self.fairy_meta_blob();
        if o + l <= blob.len() { Some(&blob[o..o+l]) } else { None }
    }

    fn view_u32(&self, off: u64, count: usize) -> LeSliceU32<'_> {
        let b = &self.mmap[off as usize..off as usize + count * core::mem::size_of::<u32>()];
        LeSliceU32 { bytes: b }
    }
    fn view_f32(&self, off: u64, count: usize) -> LeSliceF32<'_> {
        let b = &self.mmap[off as usize..off as usize + count * core::mem::size_of::<f32>()];
        LeSliceF32 { bytes: b }
    }
    fn view_i32(&self, off: u64, count: usize) -> LeSliceI32<'_> {
        let b = &self.mmap[off as usize..off as usize + count * core::mem::size_of::<i32>()];
        LeSliceI32 { bytes: b }
    }
}

#[derive(Clone, Copy)]
pub struct LeSliceU32<'a> { pub(crate) bytes: &'a [u8] }
impl<'a> LeSliceU32<'a> {
    pub fn len(&self) -> usize { self.bytes.len() / 4 }
    pub fn is_empty(&self) -> bool { self.len() == 0 }
    pub fn get(&self, idx: usize) -> Option<u32> {
        if idx < self.len() {
            let start = idx * 4;
            Some(LittleEndian::read_u32(&self.bytes[start..start+4]))
        } else { None }
    }
    pub fn iter(&self) -> LeIterU32<'a> { LeIterU32 { s: *self, i: 0 } }
}

pub struct LeIterU32<'a> { s: LeSliceU32<'a>, i: usize }
impl<'a> Iterator for LeIterU32<'a> {
    type Item = u32;
    fn next(&mut self) -> Option<Self::Item> {
        let v = self.s.get(self.i)?;
        self.i += 1;
        Some(v)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let r = self.s.len() - self.i;
        (r, Some(r))
    }
}
impl<'a> ExactSizeIterator for LeIterU32<'a> {}

#[derive(Clone, Copy)]
pub struct LeSliceF32<'a> { pub(crate) bytes: &'a [u8] }
impl<'a> LeSliceF32<'a> {
    pub fn len(&self) -> usize { self.bytes.len() / 4 }
    pub fn is_empty(&self) -> bool { self.len() == 0 }
    pub fn get(&self, idx: usize) -> Option<f32> {
        if idx < self.len() {
            let start = idx * 4;
            Some(LittleEndian::read_f32(&self.bytes[start..start+4]))
        } else { None }
    }
    pub fn iter(&self) -> LeIterF32<'a> { LeIterF32 { s: *self, i: 0 } }
}

pub struct LeIterF32<'a> { s: LeSliceF32<'a>, i: usize }
impl<'a> Iterator for LeIterF32<'a> {
    type Item = f32;
    fn next(&mut self) -> Option<Self::Item> {
        let v = self.s.get(self.i)?;
        self.i += 1;
        Some(v)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let r = self.s.len() - self.i;
        (r, Some(r))
    }
}
impl<'a> ExactSizeIterator for LeIterF32<'a> {}

#[derive(Clone, Copy)]
pub struct LeSliceI32<'a> { pub(crate) bytes: &'a [u8] }
impl<'a> LeSliceI32<'a> {
    pub fn len(&self) -> usize { self.bytes.len() / 4 }
    pub fn is_empty(&self) -> bool { self.len() == 0 }
    pub fn get(&self, idx: usize) -> Option<i32> {
        if idx < self.len() {
            let start = idx * 4;
            Some(LittleEndian::read_i32(&self.bytes[start..start+4]))
        } else { None }
    }
    pub fn iter(&self) -> LeIterI32<'a> { LeIterI32 { s: *self, i: 0 } }
}

pub struct LeIterI32<'a> { s: LeSliceI32<'a>, i: usize }
impl<'a> Iterator for LeIterI32<'a> {
    type Item = i32;
    fn next(&mut self) -> Option<Self::Item> {
        let v = self.s.get(self.i)?;
        self.i += 1;
        Some(v)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let r = self.s.len() - self.i;
        (r, Some(r))
    }
}
impl<'a> ExactSizeIterator for LeIterI32<'a> {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;    

    #[test]
    fn open_and_read_small_fixture() {
        // Build header
        let mut header = vec![0u8; Manifest::SIZE];
        header[0..4].copy_from_slice(&super::super::manifest::SNAPSHOT_MAGIC);
        LittleEndian::write_u32(&mut header[4..8], super::super::manifest::SNAPSHOT_VERSION);
        let n_nodes = 3u32; let walk = 2u32; let mac = 1u32; let req = 4u32; let lm = 2u32; let fairy = 1u32;
        let c0 = 8;
        LittleEndian::write_u32(&mut header[c0..c0+4], n_nodes);
        LittleEndian::write_u32(&mut header[c0+4..c0+8], walk);
        LittleEndian::write_u32(&mut header[c0+8..c0+12], mac);
        LittleEndian::write_u32(&mut header[c0+12..c0+16], req);
        LittleEndian::write_u32(&mut header[c0+16..c0+20], lm);
        LittleEndian::write_u32(&mut header[c0+20..c0+24], fairy);
        let o0 = c0 + 24;
        let off_nodes_ids = Manifest::SIZE as u64;
        let off_nodes_x = off_nodes_ids + (n_nodes as u64)*4;
        let off_nodes_y = off_nodes_x + (n_nodes as u64)*4;
        let off_nodes_plane = off_nodes_y + (n_nodes as u64)*4;
        let off_walk_src = off_nodes_plane + (n_nodes as u64)*4;
        let off_walk_dst = off_walk_src + (walk as u64)*4;
        let off_walk_w = off_walk_dst + (walk as u64)*4;
        let off_macro_src = off_walk_w + (walk as u64)*4;
        let off_macro_dst = off_macro_src + (mac as u64)*4;
        let off_macro_w = off_macro_dst + (mac as u64)*4;
        let off_macro_kind_first = off_macro_w + (mac as u64)*4;
        let off_macro_id_first = off_macro_kind_first + (mac as u64)*4;
        let off_meta_offs = off_macro_id_first + (mac as u64)*4;
        let off_meta_lens = off_meta_offs + (mac as u64)*4;
        let off_meta_blob = off_meta_lens + (mac as u64)*4;
        let off_req = off_meta_blob + 2u64; // "{}" blob
        let off_lm = off_req + (req as u64)*4;
        let alt_bytes = (n_nodes as u64)
            .saturating_mul(lm as u64)
            .saturating_mul(4);
        let off_lm_fw = off_lm + (lm as u64)*4;
        let off_lm_bw = off_lm_fw + alt_bytes;
        // Fairy ring offsets
        let off_fairy_nodes = off_lm_bw + alt_bytes;
        let off_fairy_cost_ms = off_fairy_nodes + (fairy as u64)*4;
        let off_fairy_meta_offs = off_fairy_cost_ms + (fairy as u64)*4;
        let off_fairy_meta_lens = off_fairy_meta_offs + (fairy as u64)*4;
        let off_fairy_meta_blob = off_fairy_meta_lens + (fairy as u64)*4;

        LittleEndian::write_u64(&mut header[o0..o0+8], off_nodes_ids);
        LittleEndian::write_u64(&mut header[o0+8..o0+16], off_nodes_x);
        LittleEndian::write_u64(&mut header[o0+16..o0+24], off_nodes_y);
        LittleEndian::write_u64(&mut header[o0+24..o0+32], off_nodes_plane);
        LittleEndian::write_u64(&mut header[o0+32..o0+40], off_walk_src);
        LittleEndian::write_u64(&mut header[o0+40..o0+48], off_walk_dst);
        LittleEndian::write_u64(&mut header[o0+48..o0+56], off_walk_w);
        LittleEndian::write_u64(&mut header[o0+56..o0+64], off_macro_src);
        LittleEndian::write_u64(&mut header[o0+64..o0+72], off_macro_dst);
        LittleEndian::write_u64(&mut header[o0+72..o0+80], off_macro_w);
        LittleEndian::write_u64(&mut header[o0+80..o0+88], off_macro_kind_first);
        LittleEndian::write_u64(&mut header[o0+88..o0+96], off_macro_id_first);
        LittleEndian::write_u64(&mut header[o0+96..o0+104], off_meta_offs);
        LittleEndian::write_u64(&mut header[o0+104..o0+112], off_meta_lens);
        LittleEndian::write_u64(&mut header[o0+112..o0+120], off_meta_blob);
        LittleEndian::write_u64(&mut header[o0+120..o0+128], off_req);
        LittleEndian::write_u64(&mut header[o0+128..o0+136], off_lm);
        LittleEndian::write_u64(&mut header[o0+136..o0+144], off_lm_fw);
        LittleEndian::write_u64(&mut header[o0+144..o0+152], off_lm_bw);
        LittleEndian::write_u64(&mut header[o0+152..o0+160], off_fairy_nodes);
        LittleEndian::write_u64(&mut header[o0+160..o0+168], off_fairy_cost_ms);
        LittleEndian::write_u64(&mut header[o0+168..o0+176], off_fairy_meta_offs);
        LittleEndian::write_u64(&mut header[o0+176..o0+184], off_fairy_meta_lens);
        LittleEndian::write_u64(&mut header[o0+184..o0+192], off_fairy_meta_blob);

        // Build data
        let mut data = Vec::new();
        // nodes ids
        for v in [10u32,11,12] { let mut b=[0u8;4]; LittleEndian::write_u32(&mut b, v); data.extend_from_slice(&b); }
        // nodes x
        for v in [3200i32,3201,3202] { let mut b=[0u8;4]; LittleEndian::write_i32(&mut b, v); data.extend_from_slice(&b); }
        // nodes y
        for v in [3200i32,3200,3200] { let mut b=[0u8;4]; LittleEndian::write_i32(&mut b, v); data.extend_from_slice(&b); }
        // nodes plane
        for v in [0i32,0,0] { let mut b=[0u8;4]; LittleEndian::write_i32(&mut b, v); data.extend_from_slice(&b); }
        // walk src
        for v in [0u32,1] { let mut b=[0u8;4]; LittleEndian::write_u32(&mut b, v); data.extend_from_slice(&b); }
        // walk dst
        for v in [1u32,2] { let mut b=[0u8;4]; LittleEndian::write_u32(&mut b, v); data.extend_from_slice(&b); }
        // walk w
        for v in [1.5f32, 2.25] { let mut b=[0u8;4]; LittleEndian::write_f32(&mut b, v); data.extend_from_slice(&b); }
        // macro src
        for v in [0u32] { let mut b=[0u8;4]; LittleEndian::write_u32(&mut b, v); data.extend_from_slice(&b); }
        // macro dst
        for v in [2u32] { let mut b=[0u8;4]; LittleEndian::write_u32(&mut b, v); data.extend_from_slice(&b); }
        // macro w
        for v in [3.5f32] { let mut b=[0u8;4]; LittleEndian::write_f32(&mut b, v); data.extend_from_slice(&b); }
        // macro kind first (e.g., 1 = door)
        for v in [1u32] { let mut b=[0u8;4]; LittleEndian::write_u32(&mut b, v); data.extend_from_slice(&b); }
        // macro id first
        for v in [42u32] { let mut b=[0u8;4]; LittleEndian::write_u32(&mut b, v); data.extend_from_slice(&b); }
        // meta offs/lens
        for v in [0u32] { let mut b=[0u8;4]; LittleEndian::write_u32(&mut b, v); data.extend_from_slice(&b); }
        for v in [2u32] { let mut b=[0u8;4]; LittleEndian::write_u32(&mut b, v); data.extend_from_slice(&b); }
        // meta blob = "{}"
        data.extend_from_slice(b"{}");
        // req tags
        for v in [7u32,8,9,10] { let mut b=[0u8;4]; LittleEndian::write_u32(&mut b, v); data.extend_from_slice(&b); }
        // landmarks
        for v in [100u32,200] { let mut b=[0u8;4]; LittleEndian::write_u32(&mut b, v); data.extend_from_slice(&b); }
        // lm_fw (zeros ok for test)
        for _ in 0..(n_nodes as usize * lm as usize) { let mut b=[0u8;4]; LittleEndian::write_f32(&mut b, 0.0); data.extend_from_slice(&b); }
        // lm_bw (zeros)
        for _ in 0..(n_nodes as usize * lm as usize) { let mut b=[0u8;4]; LittleEndian::write_f32(&mut b, 0.0); data.extend_from_slice(&b); }
        // fairy ring data
        // fairy_nodes
        for v in [0u32] { let mut b=[0u8;4]; LittleEndian::write_u32(&mut b, v); data.extend_from_slice(&b); }
        // fairy_cost_ms
        for v in [500.0f32] { let mut b=[0u8;4]; LittleEndian::write_f32(&mut b, v); data.extend_from_slice(&b); }
        // fairy_meta_offs
        for v in [0u32] { let mut b=[0u8;4]; LittleEndian::write_u32(&mut b, v); data.extend_from_slice(&b); }
        // fairy_meta_lens
        for v in [2u32] { let mut b=[0u8;4]; LittleEndian::write_u32(&mut b, v); data.extend_from_slice(&b); }
        // fairy_meta_blob = "{}"
        data.extend_from_slice(b"{}");
        // Add 32-byte hash placeholder at end
        data.extend_from_slice(&[0u8; 32]);

        // write to file
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(&header).unwrap();
        tmp.write_all(&data).unwrap();
        let path = tmp.into_temp_path();

        let snap = Snapshot::open(&path).expect("open snapshot");
        assert_eq!(snap.manifest().version, super::super::manifest::SNAPSHOT_VERSION);
        let c = snap.counts();
        assert_eq!(c.nodes, 3);
        assert_eq!(c.walk_edges, 2);
        assert_eq!(c.macro_edges, 1);
        assert_eq!(c.req_tags, 4);
        assert_eq!(c.landmarks, 2);
        assert_eq!(c.fairy_rings, 1);

        let nodes: Vec<u32> = snap.nodes_ids().iter().collect();
        assert_eq!(nodes, vec![10,11,12]);
        let wsrc: Vec<u32> = snap.walk_src().iter().collect();
        let wdst: Vec<u32> = snap.walk_dst().iter().collect();
        let ww: Vec<f32> = snap.walk_w().iter().collect();
        assert_eq!(wsrc, vec![0,1]);
        assert_eq!(wdst, vec![1,2]);
        assert!((ww[0]-1.5).abs() < 1e-6 && (ww[1]-2.25).abs() < 1e-6);
        let msrc: Vec<u32> = snap.macro_src().iter().collect();
        let mdst: Vec<u32> = snap.macro_dst().iter().collect();
        let mw: Vec<f32> = snap.macro_w().iter().collect();
        assert_eq!(msrc, vec![0]);
        assert_eq!(mdst, vec![2]);
        assert!((mw[0]-3.5).abs() < 1e-6);
        let req: Vec<u32> = snap.req_tags().iter().collect();
        assert_eq!(req, vec![7,8,9,10]);
        let lm: Vec<u32> = snap.landmarks().iter().collect();
        assert_eq!(lm, vec![100,200]);
        assert_eq!(snap.lm_fw().len(), (c.nodes as usize * 2));
        assert_eq!(snap.lm_bw().len(), (c.nodes as usize * 2));
        // Fairy ring assertions
        let fairy_nodes: Vec<u32> = snap.fairy_nodes().iter().collect();
        assert_eq!(fairy_nodes, vec![0]);
        let fairy_costs: Vec<f32> = snap.fairy_cost_ms().iter().collect();
        assert!((fairy_costs[0] - 500.0).abs() < 1e-6);
    }
}
