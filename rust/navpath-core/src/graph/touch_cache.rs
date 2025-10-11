//! Touching-nodes per-tile LRU caches (object/npc)
//! Thread-safe and read-only during runtime (DB opened RO), deterministic eviction.

use std::num::NonZeroUsize;
use std::sync::{atomic::{AtomicU64, Ordering}, Arc, Mutex};

use lru::LruCache;

use crate::db::Database;
use crate::models::Tile;
use crate::db::rows::{ObjectNodeRow, NpcNodeRow};

#[derive(Copy, Clone, Debug)]
pub struct TouchCacheConfig {
    pub objects_capacity: usize,
    pub npcs_capacity: usize,
}

impl Default for TouchCacheConfig {
    fn default() -> Self {
        Self { objects_capacity: 4096, npcs_capacity: 4096 }
    }
}

pub struct TouchingNodesCache {
    cfg: TouchCacheConfig,
    // Keyed by Tile (x,y,plane)
    objects: Mutex<LruCache<(i32, i32, i32), Arc<Vec<ObjectNodeRow>>>>,
    npcs: Mutex<LruCache<(i32, i32, i32), Arc<Vec<NpcNodeRow>>>>,
    // simple counters for tests/telemetry
    obj_hits: AtomicU64,
    obj_misses: AtomicU64,
    npc_hits: AtomicU64,
    npc_misses: AtomicU64,
}

impl TouchingNodesCache {
    pub fn new() -> Self { Self::with_config(Default::default()) }

    pub fn with_config(cfg: TouchCacheConfig) -> Self {
        let obj_cap = NonZeroUsize::new(cfg.objects_capacity.max(1)).unwrap();
        let npc_cap = NonZeroUsize::new(cfg.npcs_capacity.max(1)).unwrap();
        Self {
            cfg,
            objects: Mutex::new(LruCache::new(obj_cap)),
            npcs: Mutex::new(LruCache::new(npc_cap)),
            obj_hits: AtomicU64::new(0),
            obj_misses: AtomicU64::new(0),
            npc_hits: AtomicU64::new(0),
            npc_misses: AtomicU64::new(0),
        }
    }

    #[inline]
    fn key(tile: Tile) -> (i32, i32, i32) { (tile[0], tile[1], tile[2]) }

    /// Get object nodes touching tile from cache or load via provided fetcher on miss.
    pub fn object_nodes_touching<F>(&self, db: &Database, tile: Tile, fetch: F) -> rusqlite::Result<Arc<Vec<ObjectNodeRow>>>
    where
        F: Fn(&Database, Tile) -> rusqlite::Result<Vec<ObjectNodeRow>>,
    {
        let k = Self::key(tile);
        if let Some(hit) = self.objects.lock().unwrap().get(&k).cloned() {
            self.obj_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hit);
        }
        // Miss: fetch and insert
        let rows = fetch(db, tile)?;
        let arc = Arc::new(rows);
        let mut guard = self.objects.lock().unwrap();
        if let Some(hit2) = guard.get(&k).cloned() {
            // another thread inserted, use it
            self.obj_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hit2);
        }
        guard.put(k, Arc::clone(&arc));
        self.obj_misses.fetch_add(1, Ordering::Relaxed);
        Ok(arc)
    }

    /// Get NPC nodes touching tile from cache or load via provided fetcher on miss.
    pub fn npc_nodes_touching<F>(&self, db: &Database, tile: Tile, fetch: F) -> rusqlite::Result<Arc<Vec<NpcNodeRow>>>
    where
        F: Fn(&Database, Tile) -> rusqlite::Result<Vec<NpcNodeRow>>,
    {
        let k = Self::key(tile);
        if let Some(hit) = self.npcs.lock().unwrap().get(&k).cloned() {
            self.npc_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hit);
        }
        // Miss: fetch and insert
        let rows = fetch(db, tile)?;
        let arc = Arc::new(rows);
        let mut guard = self.npcs.lock().unwrap();
        if let Some(hit2) = guard.get(&k).cloned() {
            self.npc_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hit2);
        }
        guard.put(k, Arc::clone(&arc));
        self.npc_misses.fetch_add(1, Ordering::Relaxed);
        Ok(arc)
    }

    // Counters for tests/telemetry
    pub fn object_hits(&self) -> u64 { self.obj_hits.load(Ordering::Relaxed) }
    pub fn object_misses(&self) -> u64 { self.obj_misses.load(Ordering::Relaxed) }
    pub fn npc_hits(&self) -> u64 { self.npc_hits.load(Ordering::Relaxed) }
    pub fn npc_misses(&self) -> u64 { self.npc_misses.load(Ordering::Relaxed) }

    pub fn objects_cached(&self) -> usize { self.objects.lock().unwrap().len() }
    pub fn npcs_cached(&self) -> usize { self.npcs.lock().unwrap().len() }

    pub fn objects_capacity(&self) -> usize { self.cfg.objects_capacity }
    pub fn npcs_capacity(&self) -> usize { self.cfg.npcs_capacity }
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert_bound<T: Send + Sync>() {}
    assert_bound::<TouchingNodesCache>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Database {
        let conn = Connection::open_in_memory().unwrap();
        // Minimal schema to satisfy potential fetchers; tests use synthetic fetchers anyway
        conn.execute_batch(
            r#"
            CREATE TABLE tiles (x INTEGER, y INTEGER, plane INTEGER);
            CREATE TABLE object_nodes (
                id INTEGER PRIMARY KEY,
                match_type TEXT,
                object_id INTEGER, object_name TEXT, action TEXT,
                dest_min_x INTEGER, dest_max_x INTEGER, dest_min_y INTEGER, dest_max_y INTEGER, dest_plane INTEGER,
                orig_min_x INTEGER, orig_max_x INTEGER, orig_min_y INTEGER, orig_max_y INTEGER, orig_plane INTEGER,
                search_radius INTEGER, cost INTEGER, next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );
            CREATE TABLE npc_nodes (
                id INTEGER PRIMARY KEY,
                match_type TEXT,
                npc_id INTEGER, npc_name TEXT, action TEXT,
                dest_min_x INTEGER, dest_max_x INTEGER, dest_min_y INTEGER, dest_max_y INTEGER, dest_plane INTEGER,
                orig_min_x INTEGER, orig_max_x INTEGER, orig_min_y INTEGER, orig_max_y INTEGER, orig_plane INTEGER,
                search_radius INTEGER, cost INTEGER, next_node_type TEXT, next_node_id INTEGER, requirement_id INTEGER
            );
            "#,
        ).unwrap();
        Database::from_connection(conn)
    }

    #[test]
    fn object_cache_hit_rate_and_capacity() {
        let db = setup_db();
        let cache = TouchingNodesCache::with_config(TouchCacheConfig { objects_capacity: 2, npcs_capacity: 2 });
        let fetch = |_: &Database, tile: Tile| -> rusqlite::Result<Vec<ObjectNodeRow>> {
            Ok(vec![ObjectNodeRow {
                id: tile[0] * 100 + tile[1],
                match_type: "id".into(),
                object_id: Some(1), object_name: None, action: None,
                dest_min_x: None, dest_max_x: None, dest_min_y: None, dest_max_y: None, dest_plane: Some(tile[2]),
                orig_min_x: None, orig_max_x: None, orig_min_y: None, orig_max_y: None, orig_plane: Some(tile[2]),
                search_radius: 0, cost: None, next_node_type: None, next_node_id: None, requirement_id: None,
            }])
        };
        let t1: Tile = [1,1,0];
        let t2: Tile = [2,2,0];
        let t3: Tile = [3,3,0];
        // Misses
        let _ = cache.object_nodes_touching(&db, t1, &fetch).unwrap();
        let _ = cache.object_nodes_touching(&db, t2, &fetch).unwrap();
        assert_eq!(cache.object_misses(), 2);
        assert_eq!(cache.objects_cached(), 2);
        // Hit on t1
        let _ = cache.object_nodes_touching(&db, t1, &fetch).unwrap();
        assert_eq!(cache.object_hits(), 1);
        // Add t3 -> evicts LRU (which should be t2)
        let _ = cache.object_nodes_touching(&db, t3, &fetch).unwrap();
        assert_eq!(cache.objects_cached(), 2);
        // Access t2 again is a miss due to eviction
        let _ = cache.object_nodes_touching(&db, t2, &fetch).unwrap();
        assert!(cache.object_misses() >= 3);
    }

    #[test]
    fn npc_cache_hit_rate_and_capacity() {
        let db = setup_db();
        let cache = TouchingNodesCache::with_config(TouchCacheConfig { objects_capacity: 2, npcs_capacity: 1 });
        let fetch = |_: &Database, tile: Tile| -> rusqlite::Result<Vec<NpcNodeRow>> {
            Ok(vec![NpcNodeRow {
                id: tile[0] * 100 + tile[1],
                match_type: "id".into(),
                npc_id: Some(1), npc_name: None, action: None,
                dest_min_x: None, dest_max_x: None, dest_min_y: None, dest_max_y: None, dest_plane: Some(tile[2]),
                orig_min_x: None, orig_max_x: None, orig_min_y: None, orig_max_y: None, orig_plane: Some(tile[2]),
                search_radius: 0, cost: None, next_node_type: None, next_node_id: None, requirement_id: None,
            }])
        };
        let t1: Tile = [5,5,0];
        let t2: Tile = [6,5,0];
        // Miss then hit
        let _ = cache.npc_nodes_touching(&db, t1, &fetch).unwrap();
        let _ = cache.npc_nodes_touching(&db, t1, &fetch).unwrap();
        assert_eq!(cache.npc_misses(), 1);
        assert_eq!(cache.npc_hits(), 1);
        // Capacity 1: adding t2 evicts t1
        let _ = cache.npc_nodes_touching(&db, t2, &fetch).unwrap();
        assert_eq!(cache.npcs_cached(), 1);
        // Miss again for t1
        let _ = cache.npc_nodes_touching(&db, t1, &fetch).unwrap();
        assert!(cache.npc_misses() >= 2);
    }
}
