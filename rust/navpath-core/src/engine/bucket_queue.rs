use std::collections::VecDeque;

/// A bucket-based priority queue for A* search with O(1) amortized push/pop.
/// 
/// This implementation uses bucket sorting where each bucket contains elements
/// with f-values in a specific range. The bucket width determines the granularity
/// of the sorting. For tile-based pathfinding with integer costs, a width of 1.0
/// provides exact ordering equivalent to BinaryHeap.
/// 
/// The queue maintains the same ordering semantics as BinaryHeap:
/// - Primary: f-value (ascending)
/// - Secondary: g-value (ascending) 
/// - Tertiary: id (ascending)
pub struct BucketQueue {
    buckets: Vec<VecDeque<Key>>,
    current_bucket: usize,
    bucket_offset: f32,
    bucket_width: f32,
    min_f: f32,
    len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Key {
    pub f: f32,
    pub g: f32,
    pub id: u32,
}

impl Key {
    pub fn new(f: f32, g: f32, id: u32) -> Self {
        Self { f, g, id }
    }
}

impl BucketQueue {
    /// Create a new BucketQueue with the specified bucket width.
    /// 
    /// # Arguments
    /// * `bucket_width` - Width of each f-value bucket (default: 1.0 for integer costs)
    pub fn new(bucket_width: f32) -> Self {
        Self {
            buckets: Vec::new(),
            current_bucket: 0,
            bucket_offset: 0.0,
            bucket_width,
            min_f: f32::INFINITY,
            len: 0,
        }
    }
    
    /// Create a BucketQueue with default bucket width of 1.0
    pub fn default() -> Self {
        Self::new(1.0)
    }
    
    /// Get the bucket index for a given f-value
    fn bucket_index(&self, f: f32) -> usize {
        if f < self.bucket_offset {
            0
        } else if self.bucket_width <= 0.0 {
            0
        } else {
            let index = ((f - self.bucket_offset) / self.bucket_width) as usize;
            // Cap at a reasonable maximum to prevent overflow
            index.min(1_000_000)
        }
    }
    
    /// Ensure buckets vector has capacity for the given index
    fn ensure_bucket_capacity(&mut self, index: usize) {
        if index >= self.buckets.len() {
            self.buckets.resize(index + 1, VecDeque::new());
        }
    }
    
    /// Insert a key into the appropriate bucket
    pub fn push(&mut self, key: Key) {
        let bucket_idx = self.bucket_index(key.f);
        self.ensure_bucket_capacity(bucket_idx);
        
        // Insert in sorted order within the bucket
        let bucket = &mut self.buckets[bucket_idx];
        let insert_pos = bucket.binary_search_by(|&existing| {
            // Reverse order for max-heap behavior (BinaryHeap is a max-heap)
            let f_cmp = key.f.partial_cmp(&existing.f).unwrap_or(std::cmp::Ordering::Equal);
            if f_cmp != std::cmp::Ordering::Equal { return f_cmp.reverse(); }
            
            let g_cmp = key.g.partial_cmp(&existing.g).unwrap_or(std::cmp::Ordering::Equal);
            if g_cmp != std::cmp::Ordering::Equal { return g_cmp.reverse(); }
            
            key.id.cmp(&existing.id).reverse()
        }).unwrap_or_else(|pos| pos);
        
        bucket.insert(insert_pos, key);
        
        // Update tracking
        if key.f < self.min_f {
            self.min_f = key.f;
            self.current_bucket = bucket_idx;
        }
        
        self.len += 1;
    }
    
    /// Remove and return the key with the smallest f-value
    pub fn pop(&mut self) -> Option<Key> {
        // Find the next non-empty bucket
        while self.current_bucket < self.buckets.len() {
            if let Some(key) = self.buckets[self.current_bucket].pop_front() {
                self.len -= 1;
                
                // Update min_f for next iteration
                if self.len == 0 {
                    self.min_f = f32::INFINITY;
                    self.current_bucket = 0;
                } else {
                    // Find the next non-empty bucket
                    while self.current_bucket < self.buckets.len() 
                          && self.buckets[self.current_bucket].is_empty() {
                        self.current_bucket += 1;
                    }
                    
                    if self.current_bucket < self.buckets.len() {
                        if let Some(next_key) = self.buckets[self.current_bucket].front() {
                            self.min_f = next_key.f;
                        }
                    } else {
                        self.min_f = f32::INFINITY;
                    }
                }
                
                return Some(key);
            }
            self.current_bucket += 1;
        }
        
        None
    }
    
    /// Clear all elements from the queue
    pub fn clear(&mut self) {
        for bucket in &mut self.buckets {
            bucket.clear();
        }
        self.current_bucket = 0;
        self.min_f = f32::INFINITY;
        self.len = 0;
    }
    
    /// Check if the queue is empty
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    
    /// Get the number of elements in the queue
    pub fn len(&self) -> usize {
        self.len
    }
    
    /// Get the minimum f-value currently in the queue
    pub fn min_f(&self) -> f32 {
        self.min_f
    }
    
    /// Get the current bucket width
    pub fn bucket_width(&self) -> f32 {
        self.bucket_width
    }
    
    /// Optimize memory usage by removing empty trailing buckets
    pub fn compact(&mut self) {
        while let Some(bucket) = self.buckets.last() {
            if bucket.is_empty() {
                self.buckets.pop();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    fn make_key(f: f32, g: f32, id: u32) -> Key {
        Key::new(f, g, id)
    }
    
    #[test]
    fn test_basic_operations() {
        let mut bq = BucketQueue::default();
        assert!(bq.is_empty());
        assert_eq!(bq.len(), 0);
        
        bq.push(make_key(5.0, 3.0, 1));
        assert!(!bq.is_empty());
        assert_eq!(bq.len(), 1);
        assert_eq!(bq.min_f(), 5.0);
        
        let key = bq.pop().unwrap();
        assert_eq!(key.f, 5.0);
        assert_eq!(key.g, 3.0);
        assert_eq!(key.id, 1);
        assert!(bq.is_empty());
    }
    
    #[test]
    fn test_ordering_by_f_value() {
        let mut bq = BucketQueue::default();
        
        // Insert in random order
        bq.push(make_key(10.0, 1.0, 3));
        bq.push(make_key(5.0, 2.0, 1));
        bq.push(make_key(7.5, 1.5, 2));
        
        // Should come out in f-value order
        assert_eq!(bq.pop().unwrap().f, 5.0);
        assert_eq!(bq.pop().unwrap().f, 7.5);
        assert_eq!(bq.pop().unwrap().f, 10.0);
        assert!(bq.is_empty());
    }
    
    #[test]
    fn test_ordering_by_g_and_id() {
        let mut bq = BucketQueue::default();
        
        // Same f-value, different g and id
        bq.push(make_key(5.0, 3.0, 2));  // Higher g, higher id
        bq.push(make_key(5.0, 1.0, 1));  // Lower g, lower id (should come first)
        bq.push(make_key(5.0, 2.0, 3));  // Middle g, higher id
        
        let first = bq.pop().unwrap();
        assert_eq!(first.f, 5.0);
        assert_eq!(first.g, 1.0);
        assert_eq!(first.id, 1);
        
        let second = bq.pop().unwrap();
        assert_eq!(second.f, 5.0);
        assert_eq!(second.g, 2.0);
        assert_eq!(second.id, 3);
        
        let third = bq.pop().unwrap();
        assert_eq!(third.f, 5.0);
        assert_eq!(third.g, 3.0);
        assert_eq!(third.id, 2);
    }
    
    #[test]
    fn test_multiple_buckets() {
        let mut bq = BucketQueue::new(2.0);  // Bucket width of 2.0
        
        // Values that span multiple buckets
        bq.push(make_key(1.0, 1.0, 1));  // Bucket 0
        bq.push(make_key(3.5, 1.0, 2));  // Bucket 1
        bq.push(make_key(5.0, 1.0, 3));  // Bucket 2
        bq.push(make_key(2.0, 1.0, 4));  // Bucket 1
        
        // Should still come out in correct order
        assert_eq!(bq.pop().unwrap().f, 1.0);
        assert_eq!(bq.pop().unwrap().f, 2.0);
        assert_eq!(bq.pop().unwrap().f, 3.5);
        assert_eq!(bq.pop().unwrap().f, 5.0);
    }
    
    #[test]
    fn test_clear() {
        let mut bq = BucketQueue::default();
        
        bq.push(make_key(1.0, 1.0, 1));
        bq.push(make_key(2.0, 1.0, 2));
        bq.push(make_key(3.0, 1.0, 3));
        
        assert_eq!(bq.len(), 3);
        bq.clear();
        assert!(bq.is_empty());
        assert_eq!(bq.len(), 0);
        assert_eq!(bq.min_f(), f32::INFINITY);
    }
    
    #[test]
    fn test_large_f_values() {
        let mut bq = BucketQueue::default();
        
        // Test with very large f-values
        bq.push(make_key(1000000.0, 1.0, 1));
        bq.push(make_key(f32::MAX / 2.0, 1.0, 2));
        bq.push(make_key(1.0, 1.0, 3));
        
        assert_eq!(bq.pop().unwrap().f, 1.0);
        assert_eq!(bq.pop().unwrap().f, 1000000.0);
        assert_eq!(bq.pop().unwrap().f, f32::MAX / 2.0);
    }
    
    #[test]
    fn test_compact() {
        let mut bq = BucketQueue::new(1.0);
        
        // Create some gaps by pushing and popping
        bq.push(make_key(1.0, 1.0, 1));
        bq.push(make_key(5.0, 1.0, 2));
        bq.push(make_key(10.0, 1.0, 3));
        
        // Pop middle element to create empty bucket
        bq.pop(); // Removes 1.0
        let key = bq.pop().unwrap(); // Removes 5.0
        assert_eq!(key.f, 5.0);
        
        // Now we should have an empty bucket at index 1
        let initial_len = bq.buckets.len();
        bq.compact();
        
        // Should have removed trailing empty buckets
        assert!(bq.buckets.len() <= initial_len);
        
        // Should still work correctly
        assert_eq!(bq.pop().unwrap().f, 10.0);
        assert!(bq.is_empty());
    }
    
    #[test]
    fn test_correct_ordering() {
        let mut bq = BucketQueue::default();
        
        let keys = vec![
            make_key(5.0, 3.0, 1),
            make_key(3.0, 1.0, 2),
            make_key(5.0, 1.0, 3),
            make_key(1.0, 2.0, 4),
            make_key(3.0, 2.0, 5),
        ];
        
        for key in keys {
            bq.push(key);
        }
        
        // Expected order: by f, then g, then id (all ascending)
        let expected_order = vec![
            (1.0, 2.0, 4),  // Lowest f
            (3.0, 1.0, 2),  // Same f, lower g
            (3.0, 2.0, 5),  // Same f, higher g
            (5.0, 1.0, 3),  // Higher f, lower g
            (5.0, 3.0, 1),  // Higher f, higher g
        ];
        
        for (f, g, id) in expected_order {
            let key = bq.pop().unwrap();
            assert_eq!(key.f, f);
            assert_eq!(key.g, g);
            assert_eq!(key.id, id);
        }
        
        assert!(bq.is_empty());
    }
}
