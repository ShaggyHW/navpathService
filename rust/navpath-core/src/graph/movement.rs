//! Movement bitmask mapping and utilities.
//! Matches Python `navpath/graph.py` semantics exactly.

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Movement {
    pub name: &'static str,
    pub bit: u32,
    pub dx: i32,
    pub dy: i32,
}

// Internal movement bit assignments (must match Python):
//  north=1<<0, south=1<<1, east=1<<2, west=1<<3,
//  northeast=1<<4, northwest=1<<5, southeast=1<<6, southwest=1<<7
pub const NORTH: Movement = Movement { name: "north", bit: 1 << 0, dx: 0, dy: 1 };
pub const SOUTH: Movement = Movement { name: "south", bit: 1 << 1, dx: 0, dy: -1 };
pub const EAST: Movement = Movement { name: "east", bit: 1 << 2, dx: 1, dy: 0 };
pub const WEST: Movement = Movement { name: "west", bit: 1 << 3, dx: -1, dy: 0 };
pub const NORTHEAST: Movement = Movement { name: "northeast", bit: 1 << 4, dx: 1, dy: 1 };
pub const NORTHWEST: Movement = Movement { name: "northwest", bit: 1 << 5, dx: -1, dy: 1 };
pub const SOUTHEAST: Movement = Movement { name: "southeast", bit: 1 << 6, dx: 1, dy: -1 };
pub const SOUTHWEST: Movement = Movement { name: "southwest", bit: 1 << 7, dx: -1, dy: -1 };

// Deterministic movement order: cardinals first, then diagonals.
pub const MOVEMENT_ORDER: [Movement; 8] = [
    NORTH, SOUTH, EAST, WEST, NORTHEAST, NORTHWEST, SOUTHEAST, SOUTHWEST,
];

// Lookup to map 8-bit external tiledata to our internal movement mask.
// External mapping: bit0=west, bit1=north, bit2=east, bit3=south,
// bit4=northwest, bit5=northeast, bit6=southeast, bit7=southwest
static TILEDATA_TO_MASK: [u32; 256] = build_tiledata_lookup();
const fn build_tiledata_lookup() -> [u32; 256] {
    let mut table = [0u32; 256];
    let b_west = WEST.bit;
    let b_north = NORTH.bit;
    let b_east = EAST.bit;
    let b_south = SOUTH.bit;
    let b_northwest = NORTHWEST.bit;
    let b_northeast = NORTHEAST.bit;
    let b_southeast = SOUTHEAST.bit;
    let b_southwest = SOUTHWEST.bit;
    let mut v = 0;
    while v < 256 {
        let mut m = 0u32;
        if v & (1 << 0) != 0 { m |= b_west; }
        if v & (1 << 1) != 0 { m |= b_north; }
        if v & (1 << 2) != 0 { m |= b_east; }
        if v & (1 << 3) != 0 { m |= b_south; }
        if v & (1 << 4) != 0 { m |= b_northwest; }
        if v & (1 << 5) != 0 { m |= b_northeast; }
        if v & (1 << 6) != 0 { m |= b_southeast; }
        if v & (1 << 7) != 0 { m |= b_southwest; }
        table[v as usize] = m;
        v += 1;
    }
    table
}

/// Map optional tiledata (i64) into internal mask. None -> None.
#[inline]
pub fn mask_from_tiledata(value: Option<i64>) -> Option<u32> {
    value.map(|v| {
        let iv = (v as i64) & 0xFF;
        TILEDATA_TO_MASK[iv as usize]
    })
}

/// Decode allowed_directions from an integer bitmask (internal format) or 0 when None.
#[inline]
pub fn decode_allowed_mask_int(value: Option<i64>) -> u32 {
    value.map(|v| v as u32).unwrap_or(0)
}

/// Decode allowed_directions from a comma-separated list of tokens (case-insensitive).
/// Unknown tokens are ignored.
pub fn decode_allowed_mask_str(s: Option<&str>) -> u32 {
    let Some(raw) = s else { return 0; };
    let stripped = raw.trim();
    if stripped.is_empty() {
        return 0;
    }
    // Try decimal integer first.
    if let Ok(n) = stripped.parse::<u32>() {
        return n;
    }
    let mut mask: u32 = 0;
    let mut i = 0;
    // Manual no-allocation split by comma
    while i <= stripped.len() {
        let start = i;
        // Find next comma or end
        while i < stripped.len() && stripped.as_bytes()[i] != b',' { i += 1; }
        let part = stripped[start..i].trim().to_ascii_lowercase();
        mask |= match part.as_str() {
            "north" => NORTH.bit,
            "south" => SOUTH.bit,
            "east" => EAST.bit,
            "west" => WEST.bit,
            "northeast" => NORTHEAST.bit,
            "northwest" => NORTHWEST.bit,
            "southeast" => SOUTHEAST.bit,
            "southwest" => SOUTHWEST.bit,
            _ => 0,
        };
        i += 1; // skip comma
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_matches_python() {
        let names: Vec<&str> = MOVEMENT_ORDER.iter().map(|m| m.name).collect();
        assert_eq!(names, vec![
            "north","south","east","west","northeast","northwest","southeast","southwest"
        ]);
    }

    #[test]
    fn tiledata_bits_map_to_internal_mask() {
        // External bit positions should map to expected internal bits
        // bit0 (west)
        assert_eq!(mask_from_tiledata(Some(1)).unwrap(), WEST.bit);
        // bit1 (north)
        assert_eq!(mask_from_tiledata(Some(1 << 1)).unwrap(), NORTH.bit);
        // bit2 (east)
        assert_eq!(mask_from_tiledata(Some(1 << 2)).unwrap(), EAST.bit);
        // bit3 (south)
        assert_eq!(mask_from_tiledata(Some(1 << 3)).unwrap(), SOUTH.bit);
        // bit4 (northwest)
        assert_eq!(mask_from_tiledata(Some(1 << 4)).unwrap(), NORTHWEST.bit);
        // bit5 (northeast)
        assert_eq!(mask_from_tiledata(Some(1 << 5)).unwrap(), NORTHEAST.bit);
        // bit6 (southeast)
        assert_eq!(mask_from_tiledata(Some(1 << 6)).unwrap(), SOUTHEAST.bit);
        // bit7 (southwest)
        assert_eq!(mask_from_tiledata(Some(1 << 7)).unwrap(), SOUTHWEST.bit);
        // Multiple bits combine
        let m = mask_from_tiledata(Some((1 << 0) | (1 << 2) | (1 << 7))).unwrap();
        assert_eq!(m, WEST.bit | EAST.bit | SOUTHWEST.bit);
    }

    #[test]
    fn decode_allowed_from_int_or_tokens() {
        // Integer format passes through (internal bits)
        assert_eq!(decode_allowed_mask_int(Some((NORTH.bit | WEST.bit) as i64)), NORTH.bit | WEST.bit);
        assert_eq!(decode_allowed_mask_int(None), 0);

        // Token list
        let m = decode_allowed_mask_str(Some("north, south, east, west, northeast, northwest, southeast, southwest"));
        assert_eq!(m, NORTH.bit | SOUTH.bit | EAST.bit | WEST.bit | NORTHEAST.bit | NORTHWEST.bit | SOUTHEAST.bit | SOUTHWEST.bit);

        // Decimal string
        let m2 = decode_allowed_mask_str(Some(&format!("{}", NORTH.bit | EAST.bit)));
        assert_eq!(m2, NORTH.bit | EAST.bit);

        // Unknown tokens ignored
        let m3 = decode_allowed_mask_str(Some("north,unknown"));
        assert_eq!(m3, NORTH.bit);

        // Empty and None
        assert_eq!(decode_allowed_mask_str(Some("   ")), 0);
        assert_eq!(decode_allowed_mask_str(None), 0);
    }
}
