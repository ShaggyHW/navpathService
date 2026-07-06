use std::{fs::File, io::{BufWriter, Write}, path::Path};

#[cfg(not(target_endian = "little"))]
use byteorder::{ByteOrder, LittleEndian};

use super::manifest::{align_up, Manifest, SnapshotCounts, ALT_QUANTUM_MS, MANIFEST_OFFSET_COUNT, SNAPSHOT_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum WriterError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("length mismatch: {0}")]
    LengthMismatch(&'static str),
}

#[derive(Debug, Clone, Copy)]
pub struct WriteResult {
    pub manifest: Manifest,
    pub hash: [u8; 32],
}

/// Streams every written byte into the blake3 hasher as well as the file, so the
/// snapshot is hashed incrementally instead of buffering the whole image in RAM.
struct HashingWriter<W: Write> {
    inner: W,
    hasher: blake3::Hasher,
    written: u64,
}

impl<W: Write> HashingWriter<W> {
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), WriterError> {
        self.hasher.update(bytes);
        self.inner.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// Zero-pad up to the given absolute offset (section alignment).
    fn pad_to(&mut self, off: u64) -> Result<(), WriterError> {
        debug_assert!(off >= self.written);
        const ZEROS: [u8; 64] = [0u8; 64];
        let mut remaining = off - self.written;
        while remaining > 0 {
            let n = remaining.min(64) as usize;
            self.write_bytes(&ZEROS[..n])?;
            remaining -= n as u64;
        }
        Ok(())
    }

    /// Write a 4-byte-element slice as little-endian bytes. On little-endian targets this
    /// is a single zero-copy pass; elsewhere it converts through a scratch buffer.
    fn write_u32s(&mut self, vals: &[u32]) -> Result<(), WriterError> {
        #[cfg(target_endian = "little")]
        {
            let bytes = unsafe {
                std::slice::from_raw_parts(vals.as_ptr() as *const u8, vals.len() * 4)
            };
            self.write_bytes(bytes)
        }
        #[cfg(not(target_endian = "little"))]
        {
            let mut scratch = vec![0u8; 64 * 1024];
            for chunk in vals.chunks(scratch.len() / 4) {
                let buf = &mut scratch[..chunk.len() * 4];
                LittleEndian::write_u32_into(chunk, buf);
                self.write_bytes(buf)?;
            }
            Ok(())
        }
    }

    fn write_f32s(&mut self, vals: &[f32]) -> Result<(), WriterError> {
        #[cfg(target_endian = "little")]
        {
            let bytes = unsafe {
                std::slice::from_raw_parts(vals.as_ptr() as *const u8, vals.len() * 4)
            };
            self.write_bytes(bytes)
        }
        #[cfg(not(target_endian = "little"))]
        {
            let mut scratch = vec![0u8; 64 * 1024];
            for chunk in vals.chunks(scratch.len() / 4) {
                let buf = &mut scratch[..chunk.len() * 4];
                LittleEndian::write_f32_into(chunk, buf);
                self.write_bytes(buf)?;
            }
            Ok(())
        }
    }

    fn write_u16s(&mut self, vals: &[u16]) -> Result<(), WriterError> {
        #[cfg(target_endian = "little")]
        {
            let bytes = unsafe {
                std::slice::from_raw_parts(vals.as_ptr() as *const u8, vals.len() * 2)
            };
            self.write_bytes(bytes)
        }
        #[cfg(not(target_endian = "little"))]
        {
            let mut scratch = vec![0u8; 64 * 1024];
            for chunk in vals.chunks(scratch.len() / 2) {
                let buf = &mut scratch[..chunk.len() * 2];
                LittleEndian::write_u16_into(chunk, buf);
                self.write_bytes(buf)?;
            }
            Ok(())
        }
    }
}

/// All inputs for a v8 snapshot. Coordinates are pre-packed (see `pack_coord`) and MUST
/// be ascending (node ids are assigned in (plane,y,x) order); the walk graph is CSR with
/// a per-slot diagonal bitmap; `lm_tab` is the interleaved quantized ALT table
/// `[node][landmark][fw,bw]` (u16 quanta, 0xFFFF = unreachable).
#[cfg(feature = "builder")]
pub struct SnapshotSections<'a> {
    pub coords_packed: &'a [u32],
    pub walk_offsets: &'a [u32],
    pub walk_dst: &'a [u32],
    pub walk_diag: &'a [u8],
    pub comp: &'a [u16],
    pub walk_components: u32,
    pub macro_src: &'a [u32],
    pub macro_dst: &'a [u32],
    pub macro_w: &'a [f32],
    pub macro_kind_first: &'a [u32],
    pub macro_id_first: &'a [u32],
    pub macro_meta_offs: &'a [u32],
    pub macro_meta_lens: &'a [u32],
    pub macro_meta_blob: &'a [u8],
    pub req_tags: &'a [u32],
    pub landmarks: &'a [u32],
    pub lm_tab: &'a [u16],
    pub fairy_nodes: &'a [u32],
    pub fairy_cost_ms: &'a [f32],
    pub fairy_meta_offs: &'a [u32],
    pub fairy_meta_lens: &'a [u32],
    pub fairy_meta_blob: &'a [u8],
}

#[cfg(feature = "builder")]
pub fn write_snapshot_v8(path: impl AsRef<Path>, s: &SnapshotSections) -> Result<WriteResult, WriterError> {
    let n = s.coords_packed.len();
    let e = s.walk_dst.len();
    let m = s.macro_src.len();
    let fr = s.fairy_nodes.len();
    let lm = s.landmarks.len();

    if s.walk_offsets.len() != n + 1 {
        return Err(WriterError::LengthMismatch("walk_offsets"));
    }
    if s.walk_diag.len() != e.div_ceil(8) {
        return Err(WriterError::LengthMismatch("walk_diag"));
    }
    if s.comp.len() != n {
        return Err(WriterError::LengthMismatch("comp"));
    }
    if s.macro_dst.len() != m || s.macro_w.len() != m || s.macro_kind_first.len() != m
        || s.macro_id_first.len() != m || s.macro_meta_offs.len() != m || s.macro_meta_lens.len() != m
    {
        return Err(WriterError::LengthMismatch("macro arrays"));
    }
    if s.lm_tab.len() != n * lm * 2 {
        return Err(WriterError::LengthMismatch("lm_tab"));
    }
    if s.fairy_cost_ms.len() != fr || s.fairy_meta_offs.len() != fr || s.fairy_meta_lens.len() != fr {
        return Err(WriterError::LengthMismatch("fairy arrays"));
    }

    let counts = SnapshotCounts {
        nodes: n as u32,
        walk_edges: e as u32,
        macro_edges: m as u32,
        req_tags: s.req_tags.len() as u32,
        landmarks: lm as u32,
        fairy_rings: fr as u32,
        walk_components: s.walk_components,
    };

    // Lay out sections with aligned starts.
    let sizes: [u64; MANIFEST_OFFSET_COUNT] = [
        (n * 4) as u64,               // coords
        ((n + 1) * 4) as u64,         // walk_offsets
        (e * 4) as u64,               // walk_dst
        e.div_ceil(8) as u64,         // walk_diag
        (n * 2) as u64,               // comp
        (m * 4) as u64,               // macro_src
        (m * 4) as u64,               // macro_dst
        (m * 4) as u64,               // macro_w
        (m * 4) as u64,               // macro_kind_first
        (m * 4) as u64,               // macro_id_first
        (m * 4) as u64,               // macro_meta_offs
        (m * 4) as u64,               // macro_meta_lens
        s.macro_meta_blob.len() as u64,
        (s.req_tags.len() * 4) as u64,
        (lm * 4) as u64,              // landmarks
        (n * lm * 4) as u64,          // lm_tab (2 u16 per entry pair)
        (fr * 4) as u64,              // fairy_nodes
        (fr * 4) as u64,              // fairy_cost_ms
        (fr * 4) as u64,              // fairy_meta_offs
        (fr * 4) as u64,              // fairy_meta_lens
        s.fairy_meta_blob.len() as u64,
    ];
    let mut offs = [0u64; MANIFEST_OFFSET_COUNT];
    let mut cur = Manifest::SIZE as u64;
    for i in 0..MANIFEST_OFFSET_COUNT {
        cur = align_up(cur);
        offs[i] = cur;
        cur += sizes[i];
    }

    let manifest = Manifest {
        version: SNAPSHOT_VERSION,
        counts,
        alt_quantum_ms: ALT_QUANTUM_MS,
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
    };
    let header = manifest.write_header();

    // Write to a temp file and rename into place: overwriting the target in place would
    // truncate an inode that a running service (or bench) still has mmap'd, turning its
    // next page access into SIGBUS. Rename keeps the old inode alive until unmapped.
    let path = path.as_ref();
    let tmp_path = {
        let mut os = path.as_os_str().to_os_string();
        os.push(".tmp");
        std::path::PathBuf::from(os)
    };
    let file = File::create(&tmp_path)?;
    let mut w = HashingWriter {
        inner: BufWriter::with_capacity(4 * 1024 * 1024, file),
        hasher: blake3::Hasher::new(),
        written: 0,
    };
    w.write_bytes(&header)?;

    w.pad_to(offs[0])?;
    w.write_u32s(s.coords_packed)?;
    w.pad_to(offs[1])?;
    w.write_u32s(s.walk_offsets)?;
    w.pad_to(offs[2])?;
    w.write_u32s(s.walk_dst)?;
    w.pad_to(offs[3])?;
    w.write_bytes(s.walk_diag)?;
    w.pad_to(offs[4])?;
    w.write_u16s(s.comp)?;
    w.pad_to(offs[5])?;
    w.write_u32s(s.macro_src)?;
    w.pad_to(offs[6])?;
    w.write_u32s(s.macro_dst)?;
    w.pad_to(offs[7])?;
    w.write_f32s(s.macro_w)?;
    w.pad_to(offs[8])?;
    w.write_u32s(s.macro_kind_first)?;
    w.pad_to(offs[9])?;
    w.write_u32s(s.macro_id_first)?;
    w.pad_to(offs[10])?;
    w.write_u32s(s.macro_meta_offs)?;
    w.pad_to(offs[11])?;
    w.write_u32s(s.macro_meta_lens)?;
    w.pad_to(offs[12])?;
    w.write_bytes(s.macro_meta_blob)?;
    w.pad_to(offs[13])?;
    w.write_u32s(s.req_tags)?;
    w.pad_to(offs[14])?;
    w.write_u32s(s.landmarks)?;
    w.pad_to(offs[15])?;
    w.write_u16s(s.lm_tab)?;
    w.pad_to(offs[16])?;
    w.write_u32s(s.fairy_nodes)?;
    w.pad_to(offs[17])?;
    w.write_f32s(s.fairy_cost_ms)?;
    w.pad_to(offs[18])?;
    w.write_u32s(s.fairy_meta_offs)?;
    w.pad_to(offs[19])?;
    w.write_u32s(s.fairy_meta_lens)?;
    w.pad_to(offs[20])?;
    w.write_bytes(s.fairy_meta_blob)?;

    let hash = w.hasher.finalize();
    let hash_bytes: [u8; 32] = (*hash.as_bytes()).try_into().unwrap();
    w.inner.write_all(&hash_bytes)?;
    w.inner.flush()?;
    drop(w);
    std::fs::rename(&tmp_path, path)?;

    Ok(WriteResult { manifest, hash: hash_bytes })
}
