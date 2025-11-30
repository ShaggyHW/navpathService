use crate::snapshot::LeSliceF32;
use byteorder::{LittleEndian, ByteOrder};

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

/// High-performance landmark heuristic with pre-decoded f32 arrays
pub struct NativeLandmarkHeuristic {
    pub nodes: usize,
    pub landmarks: usize,
    pub lm_fw: Vec<f32>,
    pub lm_bw: Vec<f32>,
}

impl NativeLandmarkHeuristic {
    /// Create from LeSliceF32 by decoding all values at construction time
    pub fn from_le_slices(nodes: usize, landmarks: usize, lm_fw: LeSliceF32, lm_bw: LeSliceF32) -> Self {
        let expected_len = nodes * landmarks;
        
        // Validate input bounds to ensure safety of later unchecked access
        assert_eq!(lm_fw.len(), expected_len, "lm_fw length mismatch: expected {}, got {}", expected_len, lm_fw.len());
        assert_eq!(lm_bw.len(), expected_len, "lm_bw length mismatch: expected {}, got {}", expected_len, lm_bw.len());
        
        // Pre-decode all little-endian f32 values to native f32
        let mut lm_fw_decoded = Vec::with_capacity(expected_len);
        let mut lm_bw_decoded = Vec::with_capacity(expected_len);
        
        for i in 0..expected_len {
            let start = i * 4;
            unsafe {
                let fw_bytes = lm_fw.bytes.get_unchecked(start..start + 4);
                let bw_bytes = lm_bw.bytes.get_unchecked(start..start + 4);
                lm_fw_decoded.push(LittleEndian::read_f32(fw_bytes));
                lm_bw_decoded.push(LittleEndian::read_f32(bw_bytes));
            }
        }
        
        Self {
            nodes,
            landmarks,
            lm_fw: lm_fw_decoded,
            lm_bw: lm_bw_decoded,
        }
    }
    
    /// High-performance heuristic calculation using unchecked indexing
    #[inline(always)]
    pub fn h(&self, u: u32, goal: u32) -> f32 {
        if self.landmarks == 0 || self.nodes == 0 {
            return 0.0;
        }
        let u = u as usize;
        let g = goal as usize;
        let n = self.nodes;
        let mut best = 0.0f32;
        
        // Safe because bounds were validated at construction time
        for li in 0..self.landmarks {
            unsafe {
                let a = *self.lm_fw.get_unchecked(li * n + g) - *self.lm_fw.get_unchecked(li * n + u);
                let b = *self.lm_bw.get_unchecked(li * n + u) - *self.lm_bw.get_unchecked(li * n + g);
                let v1 = if a > 0.0 { a } else { 0.0 };
                let v2 = if b > 0.0 { b } else { 0.0 };
                let v = if v1 > v2 { v1 } else { v2 };
                if v > best { best = v; }
            }
        }
        best
    }

    /// SIMD-accelerated heuristic calculation using x86_64 intrinsics
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    #[inline(always)]
    pub fn h_simd(&self, u: u32, goal: u32) -> f32 {
        if self.landmarks == 0 || self.nodes == 0 {
            return 0.0;
        }
        
        // Check if CPU supports AVX2
        #[cfg(target_arch = "x86_64")]
        {
            if is_x86_feature_detected!("avx2") && self.landmarks >= 8 {
                return unsafe { self.h_simd_avx2(u, goal) };
            } else if is_x86_feature_detected!("sse") && self.landmarks >= 4 {
                return unsafe { self.h_simd_sse(u, goal) };
            }
        }
        
        // Fallback to scalar implementation
        self.h(u, goal)
    }

    /// AVX2 implementation processing 8 landmarks at a time
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    #[inline(always)]
    unsafe fn h_simd_avx2(&self, u: u32, goal: u32) -> f32 {
        use std::arch::x86_64::*;
        
        let u = u as usize;
        let g = goal as usize;
        let n = self.nodes;
        let mut best = 0.0f32;
        
        // Process 8 landmarks at a time
        let chunks = self.landmarks / 8;
        let _remainder = self.landmarks % 8;
        
        for chunk in 0..chunks {
            let base_idx = chunk * 8;
            
            // Load 8 forward distances for goal and start
            let fw_goal_ptr = self.lm_fw.as_ptr().add(base_idx * n + g);
            let fw_start_ptr = self.lm_fw.as_ptr().add(base_idx * n + u);
            let fw_goal = _mm256_loadu_ps(fw_goal_ptr);
            let fw_start = _mm256_loadu_ps(fw_start_ptr);
            
            // Load 8 backward distances for start and goal  
            let bw_start_ptr = self.lm_bw.as_ptr().add(base_idx * n + u);
            let bw_goal_ptr = self.lm_bw.as_ptr().add(base_idx * n + g);
            let bw_start = _mm256_loadu_ps(bw_start_ptr);
            let bw_goal = _mm256_loadu_ps(bw_goal_ptr);
            
            // Calculate differences: fw_goal - fw_start and bw_start - bw_goal
            let diff_fw = _mm256_sub_ps(fw_goal, fw_start);
            let diff_bw = _mm256_sub_ps(bw_start, bw_goal);
            
            // Max with zero to clamp negative values
            let zero = _mm256_setzero_ps();
            let clamp_fw = _mm256_max_ps(diff_fw, zero);
            let clamp_bw = _mm256_max_ps(diff_bw, zero);
            
            // Take maximum of forward and backward for each landmark
            let max_vals = _mm256_max_ps(clamp_fw, clamp_bw);
            
            // Extract values and find maximum
            let mut vals = [0.0f32; 8];
            _mm256_storeu_ps(vals.as_mut_ptr(), max_vals);
            
            for &val in &vals {
                if val > best { best = val; }
            }
        }
        
        // Process remaining landmarks with scalar code
        for li in (chunks * 8)..self.landmarks {
            let a = *self.lm_fw.get_unchecked(li * n + g) - *self.lm_fw.get_unchecked(li * n + u);
            let b = *self.lm_bw.get_unchecked(li * n + u) - *self.lm_bw.get_unchecked(li * n + g);
            let v1 = if a > 0.0 { a } else { 0.0 };
            let v2 = if b > 0.0 { b } else { 0.0 };
            let v = if v1 > v2 { v1 } else { v2 };
            if v > best { best = v; }
        }
        
        best
    }

    /// SSE implementation processing 4 landmarks at a time
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    #[inline(always)]
    unsafe fn h_simd_sse(&self, u: u32, goal: u32) -> f32 {
        use std::arch::x86_64::*;
        
        let u = u as usize;
        let g = goal as usize;
        let n = self.nodes;
        let mut best = 0.0f32;
        
        // Process 4 landmarks at a time
        let chunks = self.landmarks / 4;
        let _remainder = self.landmarks % 4;
        
        for chunk in 0..chunks {
            let base_idx = chunk * 4;
            
            // Load 4 forward distances for goal and start
            let fw_goal_ptr = self.lm_fw.as_ptr().add(base_idx * n + g);
            let fw_start_ptr = self.lm_fw.as_ptr().add(base_idx * n + u);
            let fw_goal = _mm_loadu_ps(fw_goal_ptr);
            let fw_start = _mm_loadu_ps(fw_start_ptr);
            
            // Load 4 backward distances for start and goal
            let bw_start_ptr = self.lm_bw.as_ptr().add(base_idx * n + u);
            let bw_goal_ptr = self.lm_bw.as_ptr().add(base_idx * n + g);
            let bw_start = _mm_loadu_ps(bw_start_ptr);
            let bw_goal = _mm_loadu_ps(bw_goal_ptr);
            
            // Calculate differences
            let diff_fw = _mm_sub_ps(fw_goal, fw_start);
            let diff_bw = _mm_sub_ps(bw_start, bw_goal);
            
            // Max with zero
            let zero = _mm_setzero_ps();
            let clamp_fw = _mm_max_ps(diff_fw, zero);
            let clamp_bw = _mm_max_ps(diff_bw, zero);
            
            // Take maximum
            let max_vals = _mm_max_ps(clamp_fw, clamp_bw);
            
            // Extract values
            let mut vals = [0.0f32; 4];
            _mm_storeu_ps(vals.as_mut_ptr(), max_vals);
            
            for &val in &vals {
                if val > best { best = val; }
            }
        }
        
        // Process remaining landmarks with scalar code
        for li in (chunks * 4)..self.landmarks {
            let a = *self.lm_fw.get_unchecked(li * n + g) - *self.lm_fw.get_unchecked(li * n + u);
            let b = *self.lm_bw.get_unchecked(li * n + u) - *self.lm_bw.get_unchecked(li * n + g);
            let v1 = if a > 0.0 { a } else { 0.0 };
            let v2 = if b > 0.0 { b } else { 0.0 };
            let v = if v1 > v2 { v1 } else { v2 };
            if v > best { best = v; }
        }
        
        best
    }

    /// Fallback implementation for non-SIMD platforms
    #[cfg(not(all(feature = "simd", target_arch = "x86_64")))]
    #[inline(always)]
    pub fn h_simd(&self, u: u32, goal: u32) -> f32 {
        self.h(u, goal)
    }
}

#[inline(always)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    struct TestCoords;
    impl OctileCoords for TestCoords { 
        fn coords(&self, node: u32) -> (i32,i32,i32) { (node as i32, 0, 0) } 
    }

    #[test]
    fn native_landmark_heuristic_identical_to_original() {
        let nodes = 4;
        let landmarks = 2;
        
        // Create test data: [l0_node0, l0_node1, l0_node2, l0_node3, l1_node0, l1_node1, l1_node2, l1_node3]
        let fw_data = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let bw_data = [0.5f32, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5];
        
        // Convert to little-endian bytes
        let mut fw_bytes = Vec::new();
        let mut bw_bytes = Vec::new();
        for &val in &fw_data {
            fw_bytes.extend_from_slice(&val.to_le_bytes());
        }
        for &val in &bw_data {
            bw_bytes.extend_from_slice(&val.to_le_bytes());
        }
        
        let lm_fw = LeSliceF32 { bytes: &fw_bytes };
        let lm_bw = LeSliceF32 { bytes: &bw_bytes };
        
        // Create both heuristics
        let original = LandmarkHeuristic { nodes, landmarks, lm_fw, lm_bw };
        let native = NativeLandmarkHeuristic::from_le_slices(nodes, landmarks, lm_fw, lm_bw);
        
        // Test various node pairs
        let test_cases = [(0, 1), (0, 3), (1, 2), (2, 3), (0, 0)];
        
        for &(u, goal) in &test_cases {
            let orig_result = original.h(u, goal);
            let native_result = native.h(u, goal);
            assert!((orig_result - native_result).abs() < 1e-6, 
                "Mismatch for nodes {}->{}, original: {}, native: {}", u, goal, orig_result, native_result);
        }
    }
    
    #[test]
    fn native_landmark_heuristic_zero_landmarks() {
        let native = NativeLandmarkHeuristic::from_le_slices(4, 0, LeSliceF32 { bytes: &[] }, LeSliceF32 { bytes: &[] });
        assert_eq!(native.h(0, 1), 0.0);
    }
    
    #[test]
    fn native_landmark_heuristic_zero_nodes() {
        let native = NativeLandmarkHeuristic::from_le_slices(0, 2, LeSliceF32 { bytes: &[] }, LeSliceF32 { bytes: &[] });
        assert_eq!(native.h(0, 1), 0.0);
    }

    #[test]
    fn simd_heuristic_identical_to_scalar() {
        let nodes = 100;
        let landmarks = 32; // Enough for SIMD testing
        
        // Create test data with deterministic pattern
        let mut fw_data = Vec::with_capacity(nodes * landmarks);
        let mut bw_data = Vec::with_capacity(nodes * landmarks);
        
        for li in 0..landmarks {
            for ni in 0..nodes {
                fw_data.push((li as f32 + ni as f32 * 0.1) * 0.5);
                bw_data.push((landmarks as f32 - li as f32 + ni as f32 * 0.1) * 0.3);
            }
        }
        
        // Convert to little-endian bytes
        let mut fw_bytes = Vec::new();
        let mut bw_bytes = Vec::new();
        for &val in &fw_data {
            fw_bytes.extend_from_slice(&val.to_le_bytes());
        }
        for &val in &bw_data {
            bw_bytes.extend_from_slice(&val.to_le_bytes());
        }
        
        let lm_fw = LeSliceF32 { bytes: &fw_bytes };
        let lm_bw = LeSliceF32 { bytes: &bw_bytes };
        
        let native = NativeLandmarkHeuristic::from_le_slices(nodes, landmarks, lm_fw, lm_bw);
        
        // Test various node pairs
        let test_cases = [(0, 1), (10, 90), (25, 75), (50, 50), (0, 99)];
        
        for &(u, goal) in &test_cases {
            let scalar_result = native.h(u, goal);
            let simd_result = native.h_simd(u, goal);
            
            assert!((scalar_result - simd_result).abs() < 1e-6, 
                "SIMD mismatch for nodes {}->{}, scalar: {}, simd: {}", u, goal, scalar_result, simd_result);
        }
    }
}
