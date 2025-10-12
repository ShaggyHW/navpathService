use std::convert::TryInto;
use std::error::Error;
use std::fmt::{Display, Formatter};

#[derive(Debug)]
pub enum DecodeError {
    TooShort,
    UnsupportedByteOrder,
    UnsupportedGeomType(u32),
    InvalidCounts,
}

#[inline]
fn signed_area(ring: &[[f64; 2]]) -> f64 {
    let n = ring.len();
    if n < 3 { return 0.0; }
    let mut a = 0.0f64;
    for i in 0..n {
        let j = (i + 1) % n;
        a += ring[i][0] * ring[j][1] - ring[j][0] * ring[i][1];
    }
    0.5 * a
}

impl Display for DecodeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::TooShort => write!(f, "input too short"),
            DecodeError::UnsupportedByteOrder => write!(f, "unsupported byte order (only little-endian supported)"),
            DecodeError::UnsupportedGeomType(t) => write!(f, "unsupported geometry type {t} (expected Polygon=3 or Triangle=17)"),
            DecodeError::InvalidCounts => write!(f, "invalid counts in WKB"),
        }
    }
}

impl Error for DecodeError {}

/// Minimal decode of little-endian WKB Polygon/Triangle to exterior ring points (x,y).
/// Returns only the first ring (exterior) and ignores interior rings.
pub fn decode_exterior_ring_points(wkb: &[u8]) -> Result<Vec<[f64; 2]>, DecodeError> {
    // Byte order
    if wkb.len() < 1 + 4 { return Err(DecodeError::TooShort); }
    let byte_order = wkb[0];
    if byte_order != 1 { return Err(DecodeError::UnsupportedByteOrder); }
    // Geometry type (little-endian u32)
    let gtype = le_u32(&wkb[1..1 + 4]);
    // Only Polygon (3) and Triangle (17)
    match gtype {
        3 | 17 => {}
        other => return Err(DecodeError::UnsupportedGeomType(other)),
    }

    // Number of rings (u32 LE) follows
    if wkb.len() < 1 + 4 + 4 { return Err(DecodeError::TooShort); }
    let nrings = le_u32(&wkb[5..9]) as usize;
    if nrings == 0 { return Ok(vec![]); }

    // Read first ring (exterior)
    // Cursor points at start of first ring's numPoints
    let mut off = 9usize;
    // Skip any rings before first? None; we are at first.
    if wkb.len() < off + 4 { return Err(DecodeError::TooShort); }
    let npoints = le_u32(&wkb[off..off + 4]) as usize;
    off += 4;
    if npoints == 0 { return Ok(vec![]); }
    // Each point: two f64 = 16 bytes
    let needed = npoints.checked_mul(16).ok_or(DecodeError::InvalidCounts)?;
    if wkb.len() < off + needed { return Err(DecodeError::TooShort); }

    let mut pts = Vec::with_capacity(npoints);
    for i in 0..npoints {
        let p_off = off + i * 16;
        let x = le_f64(&wkb[p_off..p_off + 8]);
        let y = le_f64(&wkb[p_off + 8..p_off + 16]);
        pts.push([x, y]);
    }

    // Some encoders repeat the first point at the end; drop it for convenience
    if pts.len() >= 2 && approx_eq2(pts[0], *pts.last().unwrap(), 0.0) {
        pts.pop();
    }

    Ok(pts)
}

#[inline]
fn le_u32(b: &[u8]) -> u32 { u32::from_le_bytes(b[0..4].try_into().unwrap()) }
#[inline]
fn le_f64(b: &[u8]) -> f64 { f64::from_le_bytes(b[0..8].try_into().unwrap()) }

#[inline]
fn approx_eq(a: f64, b: f64, eps: f64) -> bool { (a - b).abs() <= eps }
#[inline]
fn approx_eq2(a: [f64; 2], b: [f64; 2], eps: f64) -> bool { approx_eq(a[0], b[0], eps) && approx_eq(a[1], b[1], eps) }

/// Point-in-polygon for a simple closed ring using ray casting.
/// Returns true if strictly inside or on boundary. Degenerate (near-zero area) rings return false.
pub fn point_in_ring(p: [f64; 2], ring: &[[f64; 2]], eps: f64) -> bool {
    let n = ring.len();
    if n < 3 { return false; }

    // Degenerate polygons have no interior
    if signed_area(ring).abs() <= eps { return false; }

    // Boundary check: if point lies on any segment, return true
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        if point_on_segment(p, a, b, eps) { return true; }
    }

    // Ray cast to the right using standard method
    let (px, py) = (p[0], p[1]);
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (ring[i][0], ring[i][1]);
        let (xj, yj) = (ring[j][0], ring[j][1]);
        let crosses = ((yi > py) != (yj > py)) && (px < (xj - xi) * (py - yi) / (yj - yi) + xi);
        if crosses { inside = !inside; }
        j = i;
    }
    inside
}

/// Centroid of a simple polygon ring. Uses the area-weighted formula.
/// Falls back to average of vertices if area is near-zero.
pub fn centroid_of_ring(ring: &[[f64; 2]], eps: f64) -> [f64; 2] {
    let n = ring.len();
    if n == 0 { return [0.0, 0.0]; }
    if n == 1 { return ring[0]; }

    let mut a = 0.0f64;
    let mut cx = 0.0f64;
    let mut cy = 0.0f64;
    for i in 0..n {
        let j = (i + 1) % n;
        let (xi, yi) = (ring[i][0], ring[i][1]);
        let (xj, yj) = (ring[j][0], ring[j][1]);
        let cross = xi * yj - xj * yi;
        a += cross;
        cx += (xi + xj) * cross;
        cy += (yi + yj) * cross;
    }
    a *= 0.5;
    if a.abs() <= eps {
        // Degenerate: average vertices deterministically
        let mut sx = 0.0;
        let mut sy = 0.0;
        for p in ring { sx += p[0]; sy += p[1]; }
        let n_f = ring.len() as f64;
        return [sx / n_f, sy / n_f];
    }
    let factor = 1.0 / (6.0 * a);
    [cx * factor, cy * factor]
}

#[inline]
fn point_on_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2], eps: f64) -> bool {
    // Check if p is collinear with a-b and lies within bounding box with epsilon
    let (px, py) = (p[0], p[1]);
    let (ax, ay) = (a[0], a[1]);
    let (bx, by) = (b[0], b[1]);
    let cross = (bx - ax) * (py - ay) - (by - ay) * (px - ax);
    if cross.abs() > eps { return false; }
    let dot = (px - ax) * (bx - ax) + (py - ay) * (by - ay);
    if dot < -eps { return false; }
    let len_sq = (bx - ax) * (bx - ax) + (by - ay) * (by - ay);
    if dot - len_sq > eps { return false; }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wkb_polygon_of_ring(points: &[[f64; 2]]) -> Vec<u8> {
        // Build WKB little-endian Polygon with one ring, closing point repeated
        let mut pts = points.to_vec();
        if pts.first() != pts.last() { pts.push(points[0]); }
        let npoints = pts.len() as u32;
        let mut out = Vec::with_capacity(1 + 4 + 4 + 4 + (npoints as usize) * 16);
        out.push(1u8); // little-endian
        out.extend_from_slice(&3u32.to_le_bytes()); // Polygon
        out.extend_from_slice(&1u32.to_le_bytes()); // 1 ring
        out.extend_from_slice(&npoints.to_le_bytes());
        for p in &pts {
            out.extend_from_slice(&p[0].to_le_bytes());
            out.extend_from_slice(&p[1].to_le_bytes());
        }
        out
    }

    #[test]
    fn decode_square_and_centroid_and_pip() {
        let ring = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let wkb = wkb_polygon_of_ring(&ring);
        let decoded = decode_exterior_ring_points(&wkb).unwrap();
        assert_eq!(decoded.len(), 4);
        assert_eq!(decoded[0], [0.0, 0.0]);
        assert_eq!(decoded[3], [0.0, 1.0]);

        let c = centroid_of_ring(&decoded, 1e-12);
        assert!((c[0] - 0.5).abs() < 1e-12 && (c[1] - 0.5).abs() < 1e-12);

        assert!(point_in_ring([0.5, 0.5], &decoded, 1e-12));
        assert!(point_in_ring([0.0, 0.5], &decoded, 1e-12)); // on edge
        assert!(!point_in_ring([1.5, 0.5], &decoded, 1e-12));
    }

    #[test]
    fn decode_triangle_and_centroid() {
        // Simple right triangle: (0,0)-(2,0)-(0,2)
        let ring = vec![[0.0, 0.0], [2.0, 0.0], [0.0, 2.0]];
        let wkb = wkb_polygon_of_ring(&ring);
        let decoded = decode_exterior_ring_points(&wkb).unwrap();
        // Centroid of triangle is average of vertices when using area formula? For triangle:
        // centroid = (x1+x2+x3)/3, (y1+y2+y3)/3
        let c = centroid_of_ring(&decoded, 1e-12);
        assert!((c[0] - (0.0 + 2.0 + 0.0) / 3.0).abs() < 1e-12);
        assert!((c[1] - (0.0 + 0.0 + 2.0) / 3.0).abs() < 1e-12);

        assert!(point_in_ring([0.2, 0.2], &decoded, 1e-12));
        assert!(!point_in_ring([1.9, 1.9], &decoded, 1e-12));
    }

    #[test]
    fn degenerate_area_fallback() {
        // Collinear points
        let ring = vec![[0.0, 0.0], [1.0, 1.0], [2.0, 2.0]];
        let wkb = wkb_polygon_of_ring(&ring);
        let decoded = decode_exterior_ring_points(&wkb).unwrap();
        let c = centroid_of_ring(&decoded, 1e-12);
        // Fallback is average of vertices
        assert!((c[0] - (0.0 + 1.0 + 2.0) / 3.0).abs() < 1e-12);
        assert!((c[1] - (0.0 + 1.0 + 2.0) / 3.0).abs() < 1e-12);
        // PIP should be false (not a real polygon)
        assert!(!point_in_ring([0.5, 0.5], &decoded, 1e-12));
    }
}
