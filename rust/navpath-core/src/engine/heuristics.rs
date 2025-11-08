use crate::snapshot::LeSliceF32;

pub trait OctileCoords {
    fn coords(&self, node: u32) -> (i32, i32, i32);
}

pub struct LandmarkHeuristic<'a> {
    pub nodes: usize,
    pub landmarks: usize,
    pub lm_fw: LeSliceF32<'a>,
    pub lm_bw: LeSliceF32<'a>,
}

impl<'a> LandmarkHeuristic<'a> {
    pub fn h(&self, u: u32, goal: u32) -> f32 {
        if self.landmarks == 0 || self.nodes == 0 {
            return 0.0;
        }
        let u = u as usize;
        let g = goal as usize;
        let n = self.nodes;
        let mut best = 0.0f32;
        for li in 0..self.landmarks {
            let a = self.lm_fw.get(li * n + g).unwrap_or(0.0) - self.lm_fw.get(li * n + u).unwrap_or(0.0);
            let b = self.lm_bw.get(li * n + u).unwrap_or(0.0) - self.lm_bw.get(li * n + g).unwrap_or(0.0);
            let v1 = if a > 0.0 { a } else { 0.0 };
            let v2 = if b > 0.0 { b } else { 0.0 };
            let v = if v1 > v2 { v1 } else { v2 };
            if v > best { best = v; }
        }
        best
    }
}

pub fn octile<C: OctileCoords>(c: &C, a: u32, b: u32) -> f32 {
    let (ax, ay, ap) = c.coords(a);
    let (bx, by, bp) = c.coords(b);
    if ap != bp { return 0.0; }
    let dx = (ax - bx).abs() as f32;
    let dy = (ay - by).abs() as f32;
    let dmin = if dx < dy { dx } else { dy };
    let dmax = if dx > dy { dx } else { dy };
    dmin * 2_f32.sqrt() + (dmax - dmin)
}
