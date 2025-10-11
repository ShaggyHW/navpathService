//! Plane tile existence cache with LRU eviction per plane.
//! Thread-safe via Mutex; zero invalidation at runtime (DB is opened RO).
//!
//! Provides a fast path to check if a tile (x,y,plane) exists without per-neighbor DB lookups.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;

use crate::db::Database;

/// Configuration for the plane cache.
#[derive(Copy, Clone, Debug)]
pub struct PlaneTileCacheConfig {
    /// Maximum number of planes to keep in memory. Oldest is evicted.
    pub capacity: usize,
}

impl Default for PlaneTileCacheConfig {
    fn default() -> Self { Self { capacity: 8 } }
}

/// A thread-safe LRU cache of per-plane tile existence sets.
/// - Key: plane (i32)
/// - Value: HashSet<(x,y)>
pub struct PlaneTileCache {
    cfg: PlaneTileCacheConfig,
    inner: Mutex<LruCache<i32, HashSet<(i32, i32)>>>,
}

impl PlaneTileCache {
    /// Create a cache with a given capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        let cap_nz = NonZeroUsize::new(capacity.max(1)).unwrap();
        Self { cfg: PlaneTileCacheConfig { capacity }, inner: Mutex::new(LruCache::new(cap_nz)) }
    }

    /// Create a cache with default configuration.
    pub fn new() -> Self { Self::with_capacity(PlaneTileCacheConfig::default().capacity) }

    /// Returns true if tile (x,y,plane) exists in `tiles` table (via cached set, loading lazily if needed).
    /// This performs at most one DB scan per plane lifetime in the cache.
    pub fn tile_exists(&self, db: &Database, x: i32, y: i32, plane: i32) -> rusqlite::Result<bool> {
        // Fast path: check cached set if present.
        if let Some(hit) = self.lookup_cached(x, y, plane) {
            return Ok(hit);
        }
        // Miss: build the set outside the lock
        let rows = db.iter_tiles_by_plane(plane)?;
        let mut set: HashSet<(i32, i32)> = HashSet::with_capacity(rows.len());
        for r in rows { set.insert((r.x, r.y)); }

        // Insert into LRU under lock with double-check to avoid duplicate loads.
        let mut guard = self.inner.lock().expect("plane cache mutex poisoned");
        if let Some(existing) = guard.get(&plane) {
            return Ok(existing.contains(&(x, y)));
        }
        guard.put(plane, set);
        Ok(guard.get(&plane).map(|s| s.contains(&(x, y))).unwrap_or(false))
    }

    /// Returns Some(exists) if plane already cached, otherwise None.
    fn lookup_cached(&self, x: i32, y: i32, plane: i32) -> Option<bool> {
        let mut guard = self.inner.lock().ok()?;
        guard.get(&plane).map(|set| set.contains(&(x, y)))
    }

    /// Current number of planes cached.
    pub fn planes_cached(&self) -> usize {
        self.inner.lock().map(|c| c.len()).unwrap_or(0)
    }

    /// Cache capacity (number of planes).
    pub fn capacity(&self) -> usize { self.cfg.capacity }
}

// Prove Send + Sync bounds for compile-time safety.
#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert_bound<T: Send + Sync>() {}
    assert_bound::<PlaneTileCache>();
}

/// Convenience function for parity with Python and spec prompt.
/// Delegates to `PlaneTileCache::tile_exists`.
#[inline]
pub fn _tile_exists(db: &Database, cache: &PlaneTileCache, x: i32, y: i32, plane: i32) -> rusqlite::Result<bool> {
    cache.tile_exists(db, x, y, plane)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_memory_db() -> Database {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE tiles (
                x INTEGER, y INTEGER, plane INTEGER,
                tiledata INTEGER, allowed_directions TEXT, blocked_directions TEXT
            );
            "#,
        ).unwrap();
        // Plane 0: (1,1), (2,2)
        conn.execute("INSERT INTO tiles (x,y,plane) VALUES (1,1,0)", []).unwrap();
        conn.execute("INSERT INTO tiles (x,y,plane) VALUES (2,2,0)", []).unwrap();
        // Plane 1: (10,10)
        conn.execute("INSERT INTO tiles (x,y,plane) VALUES (10,10,1)", []).unwrap();
        Database::from_connection(conn)
    }

    #[test]
    fn existence_queries_hit_cache() {
        let db = setup_memory_db();
        let cache = PlaneTileCache::with_capacity(8);

        // First access loads plane 0
        assert!(cache.tile_exists(&db, 1, 1, 0).unwrap());
        assert!(cache.tile_exists(&db, 2, 2, 0).unwrap());
        assert!(!cache.tile_exists(&db, 5, 5, 0).unwrap());
        assert!(cache.planes_cached() >= 1);

        // Plane 1 independent
        assert!(cache.tile_exists(&db, 10, 10, 1).unwrap());
        assert!(!cache.tile_exists(&db, 11, 10, 1).unwrap());
        assert!(cache.planes_cached() >= 2);

        // Re-access plane 0 should be a cache hit now
        assert!(cache.tile_exists(&db, 1, 1, 0).unwrap());
    }

    #[test]
    fn lru_eviction_of_planes() {
        let db = setup_memory_db();
        let cache = PlaneTileCache::with_capacity(1); // force eviction

        // Load plane 0
        assert!(cache.tile_exists(&db, 1, 1, 0).unwrap());
        assert_eq!(cache.planes_cached(), 1);

        // Load plane 1, evict plane 0
        assert!(cache.tile_exists(&db, 10, 10, 1).unwrap());
        assert_eq!(cache.planes_cached(), 1);

        // Access plane 0 again -> must reload, still correct
        assert!(cache.tile_exists(&db, 2, 2, 0).unwrap());
        assert_eq!(cache.planes_cached(), 1);
    }

    #[test]
    fn simple_bench_like_loop() {
        use std::time::Instant;
        let db = setup_memory_db();
        let cache = PlaneTileCache::new();
        let start = Instant::now();
        let mut c = 0;
        for i in 0..10_000 {
            if cache.tile_exists(&db, 1 + (i % 2), 1 + (i % 2), 0).unwrap() { c += 1; }
        }
        // Ensure it ran and used cache
        assert!(c > 0);
        let _elapsed = start.elapsed();
        // Not asserting time; just a smoke performance loop.
    }
}
