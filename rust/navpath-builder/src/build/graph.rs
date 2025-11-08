use std::collections::HashMap;

use super::load_sqlite::Tile;

pub fn compile_walk_edges(
    tiles: &[Tile],
    node_id_of: &HashMap<(i32, i32, i32), u32>,
) -> (Vec<u32>, Vec<u32>, Vec<f32>) {
    let mut src: Vec<u32> = Vec::new();
    let mut dst: Vec<u32> = Vec::new();
    let mut w: Vec<f32> = Vec::new();

    // Direction mapping: (bit_index, dx, dy, reciprocal_bit)
    // Bits: 0:left,1:bottom,2:right,3:top,4:topleft,5:bottomleft,6:bottomright,7:topright
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
        (BOTTOMLEFT, -1, -1, TOPRIGHT /*placeholder*/, 2_f32.sqrt()),
        (BOTTOMRIGHT, 1, -1, TOPLEFT /*placeholder*/, 2_f32.sqrt()),
        (TOPRIGHT, 1, 1, BOTTOMLEFT /*placeholder*/, 2_f32.sqrt()),
    ];

    // For diagonals, we also need orthogonal requirements:
    // TOPLEFT requires TOP and LEFT; BOTTOMLEFT requires BOTTOM and LEFT; etc.
    let diag_require = [
        (TOPLEFT, TOP, LEFT),
        (BOTTOMLEFT, BOTTOM, LEFT),
        (BOTTOMRIGHT, BOTTOM, RIGHT),
        (TOPRIGHT, TOP, RIGHT),
    ];

    // Helper closures
    let has_bit = |mask: u32, bit: usize| -> bool { (mask & (1u32 << bit)) != 0 };

    for (i, t) in tiles.iter().enumerate() {
        let sid = i as u32; // nodes_ids are 0..n-1 by load order
        let mask = t.walk_mask as u32;

        for &(bit, dx, dy, recip_bit, cost) in &dirs {
            if !has_bit(mask, bit) { continue; }
            let nx = t.x + dx;
            let ny = t.y + dy;
            let np = t.plane;
            if let Some(&did) = node_id_of.get(&(nx, ny, np)) {
                // Reciprocity for cardinals: neighbor must allow opposite move
                let neighbor_mask = tiles[did as usize].walk_mask;
                let reciprocal_ok = match bit {
                    LEFT | RIGHT | TOP | BOTTOM => has_bit(neighbor_mask, recip_bit),
                    _ => true, // diagonals handled below
                };
                if !reciprocal_ok { continue; }

                // Diagonal constraints: require both orthogonal edges in source and neighbor
                let mut diag_ok = true;
                match bit {
                    TOPLEFT | BOTTOMLEFT | BOTTOMRIGHT | TOPRIGHT => {
                        // Map bit to required orthogonals
                        let maybe_pair = diag_require
                            .iter()
                            .find(|(b, _, _)| *b == bit)
                            .map(|(_, a, b)| (*a, *b));
                        let (o1, o2) = if let Some(p) = maybe_pair { p } else { continue };
                        // Source must have both
                        if !has_bit(mask, o1) || !has_bit(mask, o2) {
                            diag_ok = false;
                        } else {
                            // Neighbor must have the reciprocal of both
                            // reciprocal mapping for orthogonals
                            let recip = |b: usize| match b {
                                LEFT => RIGHT,
                                RIGHT => LEFT,
                                TOP => BOTTOM,
                                BOTTOM => TOP,
                                _ => b,
                            };
                            let nmask = neighbor_mask;
                            if !has_bit(nmask, recip(o1)) || !has_bit(nmask, recip(o2)) {
                                diag_ok = false;
                            }
                        }
                    }
                    _ => {}
                }
                if !diag_ok { continue; }

                src.push(sid);
                dst.push(did);
                w.push(cost * 600.0);
            }
        }
    }

    (src, dst, w)
}
