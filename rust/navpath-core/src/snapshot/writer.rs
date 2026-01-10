use std::{fs::File, io::Write, path::Path};

use byteorder::{ByteOrder, LittleEndian};

use super::manifest::{Manifest, SnapshotCounts, SNAPSHOT_MAGIC, SNAPSHOT_VERSION};

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

#[cfg(feature = "builder")]
pub fn write_snapshot(
    path: impl AsRef<Path>,
    nodes_ids: &[u32],
    nodes_x: &[i32],
    nodes_y: &[i32],
    nodes_plane: &[i32],
    walk_src: &[u32],
    walk_dst: &[u32],
    walk_w: &[f32],
    macro_src: &[u32],
    macro_dst: &[u32],
    macro_w: &[f32],
    macro_kind_first: &[u32],
    macro_id_first: &[u32],
    macro_meta_offs: &[u32],
    macro_meta_lens: &[u32],
    macro_meta_blob: &[u8],
    req_tags: &[u32],
    landmarks: &[u32],
    lm_fw: &[f32],
    lm_bw: &[f32],
    // Fairy Ring sections
    fairy_nodes: &[u32],
    fairy_cost_ms: &[f32],
    fairy_meta_offs: &[u32],
    fairy_meta_lens: &[u32],
    fairy_meta_blob: &[u8],
) -> Result<WriteResult, WriterError> {
    if walk_src.len() != walk_dst.len() || walk_src.len() != walk_w.len() {
        return Err(WriterError::LengthMismatch("walk arrays"));
    }
    if macro_src.len() != macro_dst.len() || macro_src.len() != macro_w.len() {
        return Err(WriterError::LengthMismatch("macro arrays"));
    }
    if macro_src.len() != macro_kind_first.len() || macro_src.len() != macro_id_first.len() {
        return Err(WriterError::LengthMismatch("macro first-step arrays"));
    }
    if nodes_ids.len() != nodes_x.len() || nodes_ids.len() != nodes_y.len() || nodes_ids.len() != nodes_plane.len() {
        return Err(WriterError::LengthMismatch("nodes coord arrays"));
    }
    if macro_src.len() != macro_meta_offs.len() || macro_src.len() != macro_meta_lens.len() {
        return Err(WriterError::LengthMismatch("macro meta arrays"));
    }
    // Fairy ring validation
    if fairy_nodes.len() != fairy_cost_ms.len() {
        return Err(WriterError::LengthMismatch("fairy ring arrays"));
    }
    if fairy_nodes.len() != fairy_meta_offs.len() || fairy_nodes.len() != fairy_meta_lens.len() {
        return Err(WriterError::LengthMismatch("fairy meta arrays"));
    }

    let counts = SnapshotCounts {
        nodes: nodes_ids.len() as u32,
        walk_edges: walk_src.len() as u32,
        macro_edges: macro_src.len() as u32,
        req_tags: req_tags.len() as u32,
        landmarks: landmarks.len() as u32,
        fairy_rings: fairy_nodes.len() as u32,
    };
    // Validate ALT tables sizes if landmarks present
    if counts.landmarks > 0 {
        let expected = (counts.nodes as usize)
            .saturating_mul(counts.landmarks as usize);
        if lm_fw.len() != expected || lm_bw.len() != expected {
            return Err(WriterError::LengthMismatch("alt tables (fw/bw)"));
        }
    } else {
        if !lm_fw.is_empty() || !lm_bw.is_empty() {
            return Err(WriterError::LengthMismatch("alt tables must be empty when landmarks=0"));
        }
    }

    // Compute offsets
    let off_nodes_ids = Manifest::SIZE as u64;
    let off_nodes_x = off_nodes_ids + (counts.nodes as u64) * 4;
    let off_nodes_y = off_nodes_x + (counts.nodes as u64) * 4;
    let off_nodes_plane = off_nodes_y + (counts.nodes as u64) * 4;
    let off_walk_src = off_nodes_plane + (counts.nodes as u64) * 4;
    let off_walk_dst = off_walk_src + (counts.walk_edges as u64) * 4;
    let off_walk_w = off_walk_dst + (counts.walk_edges as u64) * 4;
    let off_macro_src = off_walk_w + (counts.walk_edges as u64) * 4; // f32
    let off_macro_dst = off_macro_src + (counts.macro_edges as u64) * 4;
    let off_macro_w = off_macro_dst + (counts.macro_edges as u64) * 4;
    let off_macro_kind_first = off_macro_w + (counts.macro_edges as u64) * 4; // f32
    let off_macro_id_first = off_macro_kind_first + (counts.macro_edges as u64) * 4;
    let off_macro_meta_offs = off_macro_id_first + (counts.macro_edges as u64) * 4;
    let off_macro_meta_lens = off_macro_meta_offs + (counts.macro_edges as u64) * 4;
    let off_macro_meta_blob = off_macro_meta_lens + (counts.macro_edges as u64) * 4;
    let off_req_tags = off_macro_meta_blob + (macro_meta_blob.len() as u64);
    let off_landmarks = off_req_tags + (counts.req_tags as u64) * 4;
    let alt_table_bytes = (counts.nodes as u64)
        .saturating_mul(counts.landmarks as u64)
        .saturating_mul(4);
    let off_lm_fw = off_landmarks + (counts.landmarks as u64) * 4;
    let off_lm_bw = off_lm_fw + alt_table_bytes;
    // Fairy ring offsets
    let off_fairy_nodes = off_lm_bw + alt_table_bytes;
    let off_fairy_cost_ms = off_fairy_nodes + (counts.fairy_rings as u64) * 4;
    let off_fairy_meta_offs = off_fairy_cost_ms + (counts.fairy_rings as u64) * 4;
    let off_fairy_meta_lens = off_fairy_meta_offs + (counts.fairy_rings as u64) * 4;
    let off_fairy_meta_blob = off_fairy_meta_lens + (counts.fairy_rings as u64) * 4;

    let manifest = Manifest {
        version: SNAPSHOT_VERSION,
        counts,
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
        off_fairy_nodes,
        off_fairy_cost_ms,
        off_fairy_meta_offs,
        off_fairy_meta_lens,
        off_fairy_meta_blob,
    };

    // Build header bytes
    let mut header = vec![0u8; Manifest::SIZE];
    header[0..4].copy_from_slice(&SNAPSHOT_MAGIC);
    LittleEndian::write_u32(&mut header[4..8], SNAPSHOT_VERSION);
    let c0 = 8;
    LittleEndian::write_u32(&mut header[c0..c0 + 4], counts.nodes);
    LittleEndian::write_u32(&mut header[c0 + 4..c0 + 8], counts.walk_edges);
    LittleEndian::write_u32(&mut header[c0 + 8..c0 + 12], counts.macro_edges);
    LittleEndian::write_u32(&mut header[c0 + 12..c0 + 16], counts.req_tags);
    LittleEndian::write_u32(&mut header[c0 + 16..c0 + 20], counts.landmarks);
    LittleEndian::write_u32(&mut header[c0 + 20..c0 + 24], counts.fairy_rings);
    let o0 = c0 + 24;
    LittleEndian::write_u64(&mut header[o0..o0 + 8], off_nodes_ids);
    LittleEndian::write_u64(&mut header[o0 + 8..o0 + 16], off_nodes_x);
    LittleEndian::write_u64(&mut header[o0 + 16..o0 + 24], off_nodes_y);
    LittleEndian::write_u64(&mut header[o0 + 24..o0 + 32], off_nodes_plane);
    LittleEndian::write_u64(&mut header[o0 + 32..o0 + 40], off_walk_src);
    LittleEndian::write_u64(&mut header[o0 + 40..o0 + 48], off_walk_dst);
    LittleEndian::write_u64(&mut header[o0 + 48..o0 + 56], off_walk_w);
    LittleEndian::write_u64(&mut header[o0 + 56..o0 + 64], off_macro_src);
    LittleEndian::write_u64(&mut header[o0 + 64..o0 + 72], off_macro_dst);
    LittleEndian::write_u64(&mut header[o0 + 72..o0 + 80], off_macro_w);
    LittleEndian::write_u64(&mut header[o0 + 80..o0 + 88], off_macro_kind_first);
    LittleEndian::write_u64(&mut header[o0 + 88..o0 + 96], off_macro_id_first);
    LittleEndian::write_u64(&mut header[o0 + 96..o0 + 104], off_macro_meta_offs);
    LittleEndian::write_u64(&mut header[o0 + 104..o0 + 112], off_macro_meta_lens);
    LittleEndian::write_u64(&mut header[o0 + 112..o0 + 120], off_macro_meta_blob);
    LittleEndian::write_u64(&mut header[o0 + 120..o0 + 128], off_req_tags);
    LittleEndian::write_u64(&mut header[o0 + 128..o0 + 136], off_landmarks);
    LittleEndian::write_u64(&mut header[o0 + 136..o0 + 144], off_lm_fw);
    LittleEndian::write_u64(&mut header[o0 + 144..o0 + 152], off_lm_bw);
    LittleEndian::write_u64(&mut header[o0 + 152..o0 + 160], off_fairy_nodes);
    LittleEndian::write_u64(&mut header[o0 + 160..o0 + 168], off_fairy_cost_ms);
    LittleEndian::write_u64(&mut header[o0 + 168..o0 + 176], off_fairy_meta_offs);
    LittleEndian::write_u64(&mut header[o0 + 176..o0 + 184], off_fairy_meta_lens);
    LittleEndian::write_u64(&mut header[o0 + 184..o0 + 192], off_fairy_meta_blob);

    // Build data bytes
    let mut data: Vec<u8> = Vec::with_capacity(
        (counts.nodes as usize * 4 // ids + x + y + plane
            + counts.walk_edges as usize * 3
            + counts.macro_edges as usize * 7 // + kind_first + id_first + meta_offs + meta_lens
            + counts.req_tags as usize
            + counts.landmarks as usize)
            * 4
            + (alt_table_bytes as usize) * 2
            + macro_meta_blob.len(),
    );

    // nodes ids
    for &v in nodes_ids {
        let mut b = [0u8; 4];
        LittleEndian::write_u32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // nodes x
    for &v in nodes_x {
        let mut b = [0u8; 4];
        LittleEndian::write_i32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // nodes y
    for &v in nodes_y {
        let mut b = [0u8; 4];
        LittleEndian::write_i32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // nodes plane
    for &v in nodes_plane {
        let mut b = [0u8; 4];
        LittleEndian::write_i32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // walk src
    for &v in walk_src {
        let mut b = [0u8; 4];
        LittleEndian::write_u32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // walk dst
    for &v in walk_dst {
        let mut b = [0u8; 4];
        LittleEndian::write_u32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // walk w (f32)
    for &v in walk_w {
        let mut b = [0u8; 4];
        LittleEndian::write_f32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // macro src
    for &v in macro_src {
        let mut b = [0u8; 4];
        LittleEndian::write_u32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // macro dst
    for &v in macro_dst {
        let mut b = [0u8; 4];
        LittleEndian::write_u32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // macro w (f32)
    for &v in macro_w {
        let mut b = [0u8; 4];
        LittleEndian::write_f32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // macro kind first (u32)
    for &v in macro_kind_first {
        let mut b = [0u8; 4];
        LittleEndian::write_u32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // macro id first (u32)
    for &v in macro_id_first {
        let mut b = [0u8; 4];
        LittleEndian::write_u32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // macro meta offs (u32)
    for &v in macro_meta_offs {
        let mut b = [0u8; 4];
        LittleEndian::write_u32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // macro meta lens (u32)
    for &v in macro_meta_lens {
        let mut b = [0u8; 4];
        LittleEndian::write_u32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // macro meta blob (raw bytes)
    data.extend_from_slice(macro_meta_blob);
    // req tags
    for &v in req_tags {
        let mut b = [0u8; 4];
        LittleEndian::write_u32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // landmarks
    for &v in landmarks {
        let mut b = [0u8; 4];
        LittleEndian::write_u32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // lm_fw table (f32)
    for &v in lm_fw {
        let mut b = [0u8; 4];
        LittleEndian::write_f32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // lm_bw table (f32)
    for &v in lm_bw {
        let mut b = [0u8; 4];
        LittleEndian::write_f32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // fairy_nodes (u32)
    for &v in fairy_nodes {
        let mut b = [0u8; 4];
        LittleEndian::write_u32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // fairy_cost_ms (f32)
    for &v in fairy_cost_ms {
        let mut b = [0u8; 4];
        LittleEndian::write_f32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // fairy_meta_offs (u32)
    for &v in fairy_meta_offs {
        let mut b = [0u8; 4];
        LittleEndian::write_u32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // fairy_meta_lens (u32)
    for &v in fairy_meta_lens {
        let mut b = [0u8; 4];
        LittleEndian::write_u32(&mut b, v);
        data.extend_from_slice(&b);
    }
    // fairy_meta_blob (raw bytes)
    data.extend_from_slice(fairy_meta_blob);

    // Compute hash over header + data and write file: [header][data][hash32]
    let mut hasher = blake3::Hasher::new();
    hasher.update(&header);
    hasher.update(&data);
    let hash = hasher.finalize();
    let hash_bytes: [u8; 32] = (*hash.as_bytes()).try_into().unwrap();

    let mut file = File::create(path)?;
    file.write_all(&header)?;
    file.write_all(&data)?;
    file.write_all(&hash_bytes)?;
    file.flush()?;

    Ok(WriteResult { manifest, hash: hash_bytes })
}

#[cfg(all(test, feature = "builder"))]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn write_and_read_back_with_hash() {
        let mut tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let nodes = [10u32, 11, 12];
        let wsrc = [0u32, 1];
        let wdst = [1u32, 2];
        let ww = [1.5f32, 2.25];
        let msrc = [0u32];
        let mdst = [2u32];
        let mw = [3.5f32];
        let mk = [2u32]; // lodestone
        let mi = [13u32];
        let req = [7u32, 8, 9, 10];
        let lm = [100u32, 200];
        // nodes * landmarks = 3 * 2 = 6 entries each
        let lm_fw = [
            0.0f32, 1.0, 2.0, // node 0..2 for landmark 0 (node-major or any consistent order)
            0.5, 1.5, 2.5,    // landmark 1
        ];
        let lm_bw = [
            0.2f32, 1.2, 2.2,
            0.7, 1.7, 2.7,
        ];

        let xs = [3200i32, 3201, 3202];
        let ys = [3200i32, 3200, 3200];
        let ps = [0i32, 0, 0];
        // v5 metadata: single macro edge with "{}" JSON blob
        let meta_offs = [0u32];
        let meta_lens = [2u32];
        let meta_blob = b"{}".to_vec();
        // v6 fairy ring data: one fairy ring
        let fairy_nodes = [0u32]; // node index 0
        let fairy_cost_ms = [600.0f32];
        let fairy_meta_offs = [0u32];
        let fairy_meta_blob = br#"{"code":"ALS","action":null}"#.to_vec();
        let fairy_meta_lens = [fairy_meta_blob.len() as u32];
        let res = write_snapshot(
            &path, &nodes, &xs, &ys, &ps,
            &wsrc, &wdst, &ww,
            &msrc, &mdst, &mw,
            &mk, &mi,
            &meta_offs, &meta_lens, &meta_blob,
            &req, &lm, &lm_fw, &lm_bw,
            &fairy_nodes, &fairy_cost_ms, &fairy_meta_offs, &fairy_meta_lens, &fairy_meta_blob,
        ).expect("write snapshot");

        // Reader can open and validate counts and arrays
        let snap = Snapshot::open(&path).expect("open snapshot");
        let c = snap.counts();
        assert_eq!(c.nodes, nodes.len() as u32);
        assert_eq!(c.walk_edges, wsrc.len() as u32);
        assert_eq!(c.macro_edges, msrc.len() as u32);
        assert_eq!(c.req_tags, req.len() as u32);
        assert_eq!(c.landmarks, lm.len() as u32);
        let v_nodes: Vec<u32> = snap.nodes_ids().iter().collect();
        assert_eq!(v_nodes, nodes);
        let vx: Vec<i32> = snap.nodes_x().iter().collect();
        let vy: Vec<i32> = snap.nodes_y().iter().collect();
        let vp: Vec<i32> = snap.nodes_plane().iter().collect();
        assert_eq!(vx, xs);
        assert_eq!(vy, ys);
        assert_eq!(vp, ps);
        let v_wsrc: Vec<u32> = snap.walk_src().iter().collect();
        let v_wdst: Vec<u32> = snap.walk_dst().iter().collect();
        let v_ww: Vec<f32> = snap.walk_w().iter().collect();
        assert_eq!(v_wsrc, wsrc);
        assert_eq!(v_wdst, wdst);
        assert!((v_ww[0] - ww[0]).abs() < 1e-6 && (v_ww[1] - ww[1]).abs() < 1e-6);
        let v_msrc: Vec<u32> = snap.macro_src().iter().collect();
        let v_mdst: Vec<u32> = snap.macro_dst().iter().collect();
        let v_mw: Vec<f32> = snap.macro_w().iter().collect();
        assert_eq!(v_msrc, msrc);
        assert_eq!(v_mdst, mdst);
        assert!((v_mw[0] - mw[0]).abs() < 1e-6);
        let v_req: Vec<u32> = snap.req_tags().iter().collect();
        assert_eq!(v_req, req);
        let v_lm: Vec<u32> = snap.landmarks().iter().collect();
        assert_eq!(v_lm, lm);
        let fw: Vec<f32> = snap.lm_fw().iter().collect();
        let bw: Vec<f32> = snap.lm_bw().iter().collect();
        assert_eq!(fw, lm_fw);
        assert_eq!(bw, lm_bw);

        // Validate fairy ring data
        assert_eq!(c.fairy_rings, 1);
        let v_fairy_nodes: Vec<u32> = snap.fairy_nodes().iter().collect();
        assert_eq!(v_fairy_nodes, fairy_nodes);
        let v_fairy_cost: Vec<f32> = snap.fairy_cost_ms().iter().collect();
        assert!((v_fairy_cost[0] - fairy_cost_ms[0]).abs() < 1e-6);
        let meta = snap.fairy_meta_at(0).expect("fairy meta exists");
        assert_eq!(meta, &fairy_meta_blob[..]);

        // Validate appended hash equals returned hash
        let mut f = File::open(&path).unwrap();
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();
        assert!(buf.len() >= 32);
        let tail = &buf[buf.len() - 32..];
        assert_eq!(tail, &res.hash);

        // Also recompute hash over header+data portion
        let mut hasher = blake3::Hasher::new();
        hasher.update(&buf[..buf.len() - 32]);
        let recomputed = hasher.finalize();
        assert_eq!(recomputed.as_bytes(), &res.hash);
    }
}
